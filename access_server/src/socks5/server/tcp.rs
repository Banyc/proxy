use std::{collections::HashMap, io, net::SocketAddr, num::NonZeroU8, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    loading::{self, HandleConn},
    proxy_table::{ProxyAction, ProxyTableBuildError},
    stream::{
        HasIoAddr, OwnIoStream, StreamServerHandleConn,
        addr::StreamAddr,
        conn::ConnAndAddr,
        connect::StreamConnectExt,
        io_copy::{ConnContext, CopyBidirectional},
    },
    udp::TIMEOUT,
};
use protocol::stream::{
    addr::ConcreteStreamType,
    context::ConcreteStreamContext,
    proxy_table::{StreamProxyGroup, StreamProxyTable, StreamProxyTableBuilder},
    streams::tcp::TcpServer,
};
use proxy_client::stream::StreamEstablishError;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tracing::{error, trace, warn};

use crate::{
    socks5::messages::{
        Command, MethodIdentifier, NegotiationRequest, NegotiationResponse, RelayRequest,
        RelayResponse, Reply,
        sub_negotiations::{
            UsernamePasswordRequest, UsernamePasswordResponse, UsernamePasswordStatus,
        },
    },
    stream::proxy_table::StreamProxyTableBuildContext,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Socks5ServerTcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub proxy_table: SharableConfig<StreamProxyTableBuilder>,
    pub speed_limit: Option<f64>,
    pub udp_server_addr: Option<InternetAddrStr>,
    #[serde(default)]
    pub users: Vec<User>,
}
impl Socks5ServerTcpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_table: &HashMap<Arc<str>, StreamProxyTable>,
        proxy_table_cx: StreamProxyTableBuildContext<'_>,
        stream_context: ConcreteStreamContext,
    ) -> Result<Socks5ServerTcpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_table
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(proxy_table_cx)?,
        };
        let users = self
            .users
            .into_iter()
            .map(|u| (u.username.as_bytes().into(), u.password.as_bytes().into()))
            .collect();

        Ok(Socks5ServerTcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            proxy_table,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_server_addr: self.udp_server_addr.map(|a| a.0),
            users,
            stream_context,
        })
    }
}
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("Filter key not found: {0}")]
    FilterKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyTable(#[from] ProxyTableBuildError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct Socks5ServerTcpAccessServerBuilder {
    listen_addr: Arc<str>,
    proxy_table: StreamProxyTable,
    speed_limit: f64,
    udp_server_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    stream_context: ConcreteStreamContext,
}
impl loading::Build for Socks5ServerTcpAccessServerBuilder {
    type ConnHandler = Socks5ServerTcpAccessConnHandler;
    type Server = TcpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_conn_handler()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref()).await?;
        Ok(TcpServer::new(tcp_listener, access))
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let access = Socks5ServerTcpAccessConnHandler::new(
            self.proxy_table,
            self.speed_limit,
            self.udp_server_addr,
            self.users,
            self.stream_context,
            Arc::clone(&self.listen_addr),
        );
        Ok(access)
    }
}

#[derive(Debug)]
pub struct Socks5ServerTcpAccessConnHandler {
    proxy_table: StreamProxyTable,
    speed_limiter: Limiter,
    udp_listen_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    stream_context: ConcreteStreamContext,
    listen_addr: Arc<str>,
}
impl HandleConn for Socks5ServerTcpAccessConnHandler {}
impl StreamServerHandleConn for Socks5ServerTcpAccessConnHandler {
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: OwnIoStream + HasIoAddr + std::fmt::Debug,
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
impl Socks5ServerTcpAccessConnHandler {
    pub fn new(
        proxy_table: StreamProxyTable,
        speed_limit: f64,
        udp_listen_addr: Option<InternetAddr>,
        users: HashMap<Arc<[u8]>, Arc<[u8]>>,
        stream_context: ConcreteStreamContext,
        listen_addr: Arc<str>,
    ) -> Self {
        Self {
            proxy_table,
            speed_limiter: Limiter::new(speed_limit),
            udp_listen_addr,
            users,
            stream_context,
            listen_addr,
        }
    }

    async fn proxy<Downstream>(&self, downstream: Downstream) -> Result<ProxyResult, ProxyError>
    where
        Downstream: OwnIoStream + HasIoAddr + std::fmt::Debug,
    {
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
                let upstream_addr = StreamAddr {
                    stream_type: ConcreteStreamType::Tcp.to_string().into(),
                    address: upstream_addr.clone(),
                };
                let conn_context = ConnContext {
                    start: (std::time::Instant::now(), std::time::SystemTime::now()),
                    upstream_remote: upstream_addr.clone(),
                    upstream_remote_sock: upstream_sock_addr,
                    upstream_local: upstream.local_addr().ok(),
                    downstream_remote: downstream.peer_addr().ok(),
                    downstream_local: Arc::clone(&self.listen_addr),
                    session_table: self.stream_context.session_table.clone(),
                    destination: Some(upstream_addr),
                };
                let io_copy = CopyBidirectional {
                    downstream,
                    upstream,
                    payload_crypto: None,
                    speed_limiter: self.speed_limiter.clone(),
                    conn_context,
                }
                .serve_as_access_server("SOCKS5 TCP direct");
                tokio::spawn(async move {
                    let _ = io_copy.await;
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

        let conn_context = ConnContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_remote: upstream.addr,
            upstream_remote_sock: upstream.sock_addr,
            upstream_local: upstream.stream.local_addr().ok(),
            downstream_remote: downstream.peer_addr().ok(),
            downstream_local: Arc::clone(&self.listen_addr),
            session_table: self.stream_context.session_table.clone(),
            destination: Some(StreamAddr {
                stream_type: ConcreteStreamType::Tcp.to_string().into(),
                address: destination.clone(),
            }),
        };
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto,
            speed_limiter: self.speed_limiter.clone(),
            conn_context,
        }
        .serve_as_access_server("SOCKS5 TCP");
        tokio::spawn(async move {
            let _ = io_copy.await;
        });
        Ok(ProxyResult::IoCopy)
    }

    async fn establish<Stream>(
        &self,
        stream: Stream,
    ) -> Result<EstablishResult<Stream>, EstablishError>
    where
        Stream: OwnIoStream + HasIoAddr + std::fmt::Debug,
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

        // Filter
        let action = self.proxy_table.action(&relay_request.destination);
        let proxy_group = match action {
            ProxyAction::Block => {
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
            ProxyAction::Direct => {
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
                let upstream = match self
                    .stream_context
                    .connector_table
                    .tcp()
                    .timed_connect(sock_addr, TIMEOUT)
                    .await
                {
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
            ProxyAction::ProxyGroup(proxy_group) => proxy_group,
        };

        let (upstream, payload_crypto) = match self
            .establish_proxy_chain(proxy_group, relay_request.destination.clone())
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
        return (
            relay_response,
            Ok(RequestResult::Proxy {
                destination: relay_request.destination,
                upstream,
                payload_crypto,
            }),
        );

        fn general_socks_server_failure() -> RelayResponse {
            RelayResponse {
                reply: Reply::GeneralSocksServerFailure,
                bind: InternetAddr::zero_ipv4_addr(),
            }
        }
    }

    async fn steer<Stream>(&self, stream: Stream) -> io::Result<(Stream, RelayRequest)>
    where
        Stream: OwnIoStream + HasIoAddr + std::fmt::Debug,
    {
        let mut stream = self.negotiate(stream).await?;

        let relay_request = RelayRequest::decode(&mut stream).await?;

        Ok((stream, relay_request))
    }

    async fn negotiate<Stream>(&self, mut stream: Stream) -> io::Result<Stream>
    where
        Stream: OwnIoStream + HasIoAddr + std::fmt::Debug,
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
            return Err(io::Error::other("No auth method supported"));
        }
        let negotiation_response = NegotiationResponse {
            method: Some(MethodIdentifier::NoAuth),
        };
        negotiation_response.encode(&mut stream).await?;

        Ok(stream)
    }

    async fn username_password<Stream>(&self, mut stream: Stream) -> io::Result<Stream>
    where
        Stream: OwnIoStream + HasIoAddr + std::fmt::Debug,
    {
        let request = UsernamePasswordRequest::decode(&mut stream).await?;
        let password = match self.users.get(request.username()) {
            Some(password) => password,
            None => {
                let response = UsernamePasswordResponse {
                    status: UsernamePasswordStatus::Failure(NonZeroU8::new(1).unwrap()),
                };
                response.encode(&mut stream).await?;
                return Err(io::Error::other(format!(
                    "Username not found: {}",
                    String::from_utf8_lossy(request.username())
                )));
            }
        };
        if request.password() != password.as_ref() {
            let response = UsernamePasswordResponse {
                status: UsernamePasswordStatus::Failure(NonZeroU8::new(2).unwrap()),
            };
            response.encode(&mut stream).await?;
            return Err(io::Error::other(format!(
                "Password incorrect: {{ username: {}, password: {} }}",
                String::from_utf8_lossy(request.username()),
                String::from_utf8_lossy(request.password()),
            )));
        }
        let response = UsernamePasswordResponse {
            status: UsernamePasswordStatus::Success,
        };
        response.encode(&mut stream).await?;
        Ok(stream)
    }

    async fn establish_proxy_chain(
        &self,
        proxy_group: &StreamProxyGroup,
        destination: InternetAddr,
    ) -> Result<(ConnAndAddr, Option<tokio_chacha20::config::Config>), EstablishProxyChainError>
    {
        let proxy_chain = proxy_group.choose_chain();
        let res = proxy_client::stream::establish(
            &proxy_chain.chain,
            StreamAddr {
                address: destination,
                stream_type: ConcreteStreamType::Tcp.to_string().into(),
            },
            &self.stream_context,
        )
        .await?;
        Ok((res, proxy_chain.payload_crypto.clone()))
    }
}
#[derive(Debug, Error)]
pub enum EstablishProxyChainError {
    #[error("{0}")]
    StreamEstablish(#[from] StreamEstablishError),
}
#[allow(clippy::large_enum_variant)]
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
        upstream: ConnAndAddr,
        payload_crypto: Option<tokio_chacha20::config::Config>,
    },
}
#[allow(clippy::large_enum_variant)]
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
        upstream: ConnAndAddr,
        payload_crypto: Option<tokio_chacha20::config::Config>,
    },
}
pub enum ProxyResult {
    Blocked,
    Udp,
    IoCopy,
}
#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to establish connection: {0}")]
    Establish(#[from] EstablishError),
    #[error("Failed to get downstream address: {0}")]
    DownstreamAddr(#[source] io::Error),
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
    EstablishProxyChain(#[from] EstablishProxyChainError),
    #[error("Command BIND not supported")]
    CmdBindNotSupported,
    #[error("No UDP server available")]
    NoUdpServerAvailable,
}
