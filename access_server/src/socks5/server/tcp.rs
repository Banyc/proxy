use std::{collections::HashMap, io, net::SocketAddr, num::NonZeroU8, sync::Arc};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    crypto::XorCrypto,
    filter::{self, Filter, FilterBuilder},
    loading::{self, Hook},
    stream::{
        addr::{StreamAddr, StreamType},
        io_copy::CopyBidirectional,
        pool::Pool,
        proxy_table::StreamProxyTable,
        session_table::StreamSessionTable,
        streams::tcp::TcpServer,
        CreatedStreamAndAddr, IoAddr, IoStream, StreamServerHook,
    },
};
use proxy_client::stream::StreamEstablishError;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tracing::{error, trace, warn};

use crate::{
    socks5::messages::{
        sub_negotiations::{
            UsernamePasswordRequest, UsernamePasswordResponse, UsernamePasswordStatus,
        },
        Command, MethodIdentifier, NegotiationRequest, NegotiationResponse, RelayRequest,
        RelayResponse, Reply,
    },
    stream::proxy_table::{StreamProxyTableBuildError, StreamProxyTableBuilder},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Socks5ServerTcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub proxy_table: SharableConfig<StreamProxyTableBuilder>,
    pub filter: SharableConfig<FilterBuilder>,
    pub speed_limit: Option<f64>,
    pub udp_server_addr: Option<InternetAddrStr>,
    #[serde(default)]
    pub users: Vec<User>,
}

impl Socks5ServerTcpAccessServerConfig {
    pub fn into_builder(
        self,
        stream_pool: Pool,
        proxy_tables: &HashMap<Arc<str>, StreamProxyTable>,
        filters: &HashMap<Arc<str>, Filter>,
        cancellation: CancellationToken,
        session_table: StreamSessionTable,
    ) -> Result<Socks5ServerTcpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(&stream_pool, cancellation)?,
        };
        let filter = match self.filter {
            SharableConfig::SharingKey(key) => filters
                .get(&key)
                .ok_or_else(|| BuildError::FilterKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(filters, &Default::default())?,
        };
        let users = self
            .users
            .into_iter()
            .map(|u| (u.username.as_bytes().into(), u.password.as_bytes().into()))
            .collect();

        Ok(Socks5ServerTcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            proxy_table,
            stream_pool,
            filter,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_server_addr: self.udp_server_addr.map(|a| a.0),
            users,
            session_table,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("Filter key not found: {0}")]
    FilterKeyNotFound(Arc<str>),
    #[error("Filter error: {0}")]
    Filter(#[from] filter::FilterBuildError),
    #[error("{0}")]
    ProxyTable(#[from] StreamProxyTableBuildError),
}

#[derive(Debug, Clone)]
pub struct Socks5ServerTcpAccessServerBuilder {
    listen_addr: Arc<str>,
    proxy_table: StreamProxyTable,
    stream_pool: Pool,
    filter: Filter,
    speed_limit: f64,
    udp_server_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    session_table: StreamSessionTable,
}

#[async_trait]
impl loading::Builder for Socks5ServerTcpAccessServerBuilder {
    type Hook = Socks5ServerTcpAccess;
    type Server = TcpServer<Self::Hook>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref()).await?;
        Ok(TcpServer::new(tcp_listener, access))
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        let access = Socks5ServerTcpAccess::new(
            self.proxy_table,
            self.stream_pool,
            self.filter,
            self.speed_limit,
            self.udp_server_addr,
            self.users,
            self.session_table,
        );
        Ok(access)
    }
}

#[derive(Debug)]
pub struct Socks5ServerTcpAccess {
    proxy_table: StreamProxyTable,
    stream_pool: Pool,
    filter: Filter,
    speed_limiter: Limiter,
    udp_listen_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    session_table: StreamSessionTable,
}

impl Hook for Socks5ServerTcpAccess {}

#[async_trait]
impl StreamServerHook for Socks5ServerTcpAccess {
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let res = self.proxy(stream).await;
        match res {
            Ok(ProxyResult::Blocked) => (),
            Ok(ProxyResult::IoCopy) => (),
            Ok(ProxyResult::Udp) => (),
            Err(e) => warn!(?e, "Failed to proxy"),
        }
    }
}

impl Socks5ServerTcpAccess {
    pub fn new(
        proxy_table: StreamProxyTable,
        stream_pool: Pool,
        filter: Filter,
        speed_limit: f64,
        udp_listen_addr: Option<InternetAddr>,
        users: HashMap<Arc<[u8]>, Arc<[u8]>>,
        session_table: StreamSessionTable,
    ) -> Self {
        Self {
            proxy_table,
            stream_pool,
            filter,
            speed_limiter: Limiter::new(speed_limit),
            udp_listen_addr,
            users,
            session_table,
        }
    }

    async fn proxy<S>(&self, downstream: S) -> Result<ProxyResult, ProxyError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream.peer_addr().ok();

        let res = self.establish(downstream).await?;
        let (destination, downstream, upstream, payload_crypto) = match res {
            EstablishResult::Blocked { destination } => {
                trace!(?destination, "Blocked");
                return Ok(ProxyResult::Blocked);
            }
            EstablishResult::Direct {
                downstream,
                upstream,
                upstream_addr,
                upstream_sock_addr,
            } => {
                let speed_limiter = self.speed_limiter.clone();
                let session_table = self.session_table.clone();
                tokio::spawn(async move {
                    let upstream_local = upstream.local_addr().ok();
                    let io_copy = CopyBidirectional {
                        downstream,
                        upstream,
                        payload_crypto: None,
                        speed_limiter,
                        start,
                        upstream_addr: StreamAddr {
                            stream_type: StreamType::Tcp,
                            address: upstream_addr.clone(),
                        },
                        upstream_sock_addr,
                        downstream_addr,
                    };
                    let _ = io_copy
                        .serve_as_access_server(
                            StreamAddr {
                                stream_type: StreamType::Tcp,
                                address: upstream_addr,
                            },
                            session_table,
                            upstream_local,
                            "SOCKS5 TCP direct",
                        )
                        .await;
                });
                return Ok(ProxyResult::IoCopy);
            }
            EstablishResult::Udp { mut downstream } => {
                tokio::spawn(async move {
                    // Prevent the UDP association from terminating
                    let mut buf = [0; 1];
                    let _ = downstream.read_exact(&mut buf).await;
                });
                return Ok(ProxyResult::Udp);
            }
            EstablishResult::Proxy {
                destination,
                downstream,
                upstream,
                payload_crypto,
            } => (destination, downstream, upstream, payload_crypto),
        };

        let speed_limiter = self.speed_limiter.clone();
        let session_table = self.session_table.clone();
        tokio::spawn(async move {
            let upstream_local = upstream.stream.local_addr().ok();
            let io_copy = CopyBidirectional {
                downstream,
                upstream: upstream.stream,
                payload_crypto,
                speed_limiter,
                start,
                upstream_addr: upstream.addr,
                upstream_sock_addr: upstream.sock_addr,
                downstream_addr,
            };
            let _ = io_copy
                .serve_as_access_server(
                    StreamAddr {
                        stream_type: StreamType::Tcp,
                        address: destination.clone(),
                    },
                    session_table,
                    upstream_local,
                    "SOCKS5 TCP",
                )
                .await;
        });
        Ok(ProxyResult::IoCopy)
    }

    async fn establish<S>(&self, stream: S) -> Result<EstablishResult<S>, EstablishError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let (mut stream, relay_request) = self
            .steer(stream)
            .await
            .map_err(EstablishError::Negotiate)?;

        let local_addr = stream.local_addr()?;

        let (relay_response, res) = self.request(relay_request, local_addr).await;
        relay_response.encode(&mut stream).await?;

        Ok(match res? {
            RequestResult::Blocked { destination } => EstablishResult::Blocked { destination },
            RequestResult::Direct {
                upstream,
                upstream_addr,
                upstream_sock_addr,
            } => EstablishResult::Direct {
                downstream: stream,
                upstream,
                upstream_addr,
                upstream_sock_addr,
            },
            RequestResult::Udp {} => EstablishResult::Udp { downstream: stream },
            RequestResult::Proxy {
                destination,
                upstream,
                payload_crypto,
            } => EstablishResult::Proxy {
                destination,
                downstream: stream,
                upstream,
                payload_crypto,
            },
        })
    }

    async fn request(
        &self,
        relay_request: RelayRequest,
        local_addr: SocketAddr,
    ) -> (RelayResponse, Result<RequestResult, EstablishError>) {
        // Filter
        let action = self.filter.filter(&relay_request.destination);
        if matches!(action, filter::Action::Block) {
            let relay_response = RelayResponse {
                reply: Reply::ConnectionNotAllowedByRuleset,
                bind: InternetAddr::zero_ipv4_addr(),
            };
            return (
                relay_response,
                Ok(RequestResult::Blocked {
                    destination: relay_request.destination,
                }),
            );
        }

        match relay_request.command {
            Command::Connect => (),
            Command::Bind => {
                let relay_response = RelayResponse {
                    reply: Reply::CommandNotSupported,
                    bind: InternetAddr::zero_ipv4_addr(),
                };
                return (relay_response, Err(EstablishError::CmdBindNotSupported));
            }
            Command::UdpAssociate => match &self.udp_listen_addr {
                Some(addr) => {
                    let relay_response = RelayResponse {
                        reply: Reply::Succeeded,
                        bind: addr.clone(),
                    };
                    return (relay_response, Ok(RequestResult::Udp {}));
                }
                None => {
                    let relay_response = RelayResponse {
                        reply: Reply::CommandNotSupported,
                        bind: InternetAddr::zero_ipv4_addr(),
                    };
                    return (relay_response, Err(EstablishError::NoUdpServerAvailable));
                }
            },
        }

        fn general_socks_server_failure() -> RelayResponse {
            RelayResponse {
                reply: Reply::GeneralSocksServerFailure,
                bind: InternetAddr::zero_ipv4_addr(),
            }
        }

        if matches!(action, filter::Action::Direct) {
            let sock_addr = match relay_request.destination.to_socket_addr().await {
                Ok(sock_addr) => sock_addr,
                Err(e) => {
                    return (
                        general_socks_server_failure(),
                        Err(EstablishError::DirectConnect {
                            source: e,
                            destination: relay_request.destination.clone(),
                        }),
                    );
                }
            };
            let upstream = match tokio::net::TcpStream::connect(sock_addr).await {
                Ok(upstream) => upstream,
                Err(e) => {
                    return (
                        general_socks_server_failure(),
                        Err(EstablishError::DirectConnect {
                            source: e,
                            destination: relay_request.destination.clone(),
                        }),
                    );
                }
            };
            let relay_response = RelayResponse {
                reply: Reply::Succeeded,
                bind: local_addr.into(),
            };
            return (
                relay_response,
                Ok(RequestResult::Direct {
                    upstream,
                    upstream_addr: relay_request.destination,
                    upstream_sock_addr: sock_addr,
                }),
            );
        }

        let (upstream, payload_crypto) = match self
            .establish_proxy_chain(relay_request.destination.clone())
            .await
        {
            Ok(res) => res,
            Err(e) => {
                return (general_socks_server_failure(), Err(e.into()));
            }
        };
        let relay_response = RelayResponse {
            reply: Reply::Succeeded,
            bind: local_addr.into(),
        };
        (
            relay_response,
            Ok(RequestResult::Proxy {
                destination: relay_request.destination,
                upstream,
                payload_crypto,
            }),
        )
    }

    async fn steer<S>(&self, stream: S) -> io::Result<(S, RelayRequest)>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let mut stream = self.negotiate(stream).await?;

        let relay_request = RelayRequest::decode(&mut stream).await?;

        Ok((stream, relay_request))
    }

    async fn negotiate<S>(&self, mut stream: S) -> io::Result<S>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let negotiation_request = NegotiationRequest::decode(&mut stream).await?;

        // Username/password authentication
        if !self.users.is_empty()
            && negotiation_request
                .methods
                .contains(&MethodIdentifier::UsernamePassword)
        {
            let negotiation_response = NegotiationResponse {
                method: Some(MethodIdentifier::UsernamePassword),
            };
            negotiation_response.encode(&mut stream).await?;

            let stream = self.username_password(stream).await?;
            return Ok(stream);
        }

        // No authentication
        let allow_no_auth = self.users.is_empty();
        if !allow_no_auth
            || !negotiation_request
                .methods
                .contains(&MethodIdentifier::NoAuth)
        {
            let negotiation_response = NegotiationResponse { method: None };
            negotiation_response.encode(&mut stream).await?;
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "No auth method supported",
            ));
        }
        let negotiation_response = NegotiationResponse {
            method: Some(MethodIdentifier::NoAuth),
        };
        negotiation_response.encode(&mut stream).await?;

        Ok(stream)
    }

    async fn username_password<S>(&self, mut stream: S) -> io::Result<S>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let request = UsernamePasswordRequest::decode(&mut stream).await?;
        let password = match self.users.get(request.username()) {
            Some(password) => password,
            None => {
                let response = UsernamePasswordResponse {
                    status: UsernamePasswordStatus::Failure(NonZeroU8::new(1).unwrap()),
                };
                response.encode(&mut stream).await?;
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Username not found: {}",
                        String::from_utf8_lossy(request.username())
                    ),
                ));
            }
        };
        if request.password() != password.as_ref() {
            let response = UsernamePasswordResponse {
                status: UsernamePasswordStatus::Failure(NonZeroU8::new(2).unwrap()),
            };
            response.encode(&mut stream).await?;
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Password incorrect: {{ username: {}, password: {} }}",
                    String::from_utf8_lossy(request.username()),
                    String::from_utf8_lossy(request.password()),
                ),
            ));
        }
        let response = UsernamePasswordResponse {
            status: UsernamePasswordStatus::Success,
        };
        response.encode(&mut stream).await?;
        Ok(stream)
    }

    async fn establish_proxy_chain(
        &self,
        destination: InternetAddr,
    ) -> Result<(CreatedStreamAndAddr, Option<XorCrypto>), StreamEstablishError> {
        let proxy_chain = self.proxy_table.choose_chain();
        let res = proxy_client::stream::establish(
            &proxy_chain.chain,
            StreamAddr {
                address: destination,
                stream_type: StreamType::Tcp,
            },
            &self.stream_pool,
        )
        .await?;
        Ok((res, proxy_chain.payload_crypto.clone()))
    }
}

pub enum EstablishResult<S> {
    Blocked {
        destination: InternetAddr,
    },
    Direct {
        downstream: S,
        upstream: tokio::net::TcpStream,
        upstream_addr: InternetAddr,
        upstream_sock_addr: SocketAddr,
    },
    Udp {
        downstream: S,
    },
    Proxy {
        destination: InternetAddr,
        downstream: S,
        upstream: CreatedStreamAndAddr,
        payload_crypto: Option<XorCrypto>,
    },
}

enum RequestResult {
    Blocked {
        destination: InternetAddr,
    },
    Direct {
        upstream: tokio::net::TcpStream,
        upstream_addr: InternetAddr,
        upstream_sock_addr: SocketAddr,
    },
    Udp {},
    Proxy {
        destination: InternetAddr,
        upstream: CreatedStreamAndAddr,
        payload_crypto: Option<XorCrypto>,
    },
}

pub enum ProxyResult {
    Blocked,
    Udp,
    IoCopy,
}

#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("Failed to negotiate: {0}")]
    Negotiate(#[source] io::Error),
    #[error("Failed to connect directly: {source}, {destination}")]
    DirectConnect {
        #[source]
        source: io::Error,
        destination: InternetAddr,
    },
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Command BIND not supported")]
    CmdBindNotSupported,
    #[error("No UDP server available")]
    NoUdpServerAvailable,
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to establish connection: {0}")]
    Establish(#[from] EstablishError),
    #[error("Failed to get downstream address: {0}")]
    DownstreamAddr(#[source] io::Error),
}
