use std::{collections::HashMap, io, net::SocketAddr, sync::Arc};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use common::{
    addr::InternetAddr,
    config::SharableConfig,
    crypto::XorCrypto,
    filter::{self, Filter, FilterBuilder},
    loading::{self, Hook},
    stream::{
        addr::{StreamAddr, StreamType},
        copy_bidirectional_with_payload_crypto,
        pool::Pool,
        proxy_table::StreamProxyTable,
        streams::tcp::TcpServer,
        tokio_io, CreatedStream, FailedStreamMetrics, IoAddr, IoStream, StreamMetrics,
        StreamServerHook,
    },
};
use proxy_client::stream::StreamEstablishError;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tracing::{error, info, trace, warn};

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
    pub udp_server_addr: Option<Arc<str>>,
    #[serde(default)]
    pub users: Vec<User>,
}

impl Socks5ServerTcpAccessServerConfig {
    pub fn into_builder(
        self,
        stream_pool: Pool,
        proxy_tables: &HashMap<Arc<str>, StreamProxyTable>,
        filters: &HashMap<Arc<str>, Filter>,
    ) -> Result<Socks5ServerTcpAccessServerBuilder, BuildError> {
        let proxy_table = match self.proxy_table {
            SharableConfig::SharingKey(key) => proxy_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(&stream_pool)?,
        };
        let filter = match self.filter {
            SharableConfig::SharingKey(key) => filters
                .get(&key)
                .ok_or_else(|| BuildError::FilterKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(filters)?,
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
            udp_server_addr: self.udp_server_addr,
            users,
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
    udp_server_addr: Option<Arc<str>>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
}

#[async_trait]
impl loading::Builder for Socks5ServerTcpAccessServerBuilder {
    type Hook = Socks5ServerTcpAccess;
    type Server = TcpServer<Self::Hook>;

    async fn build_server(self) -> io::Result<TcpServer<Socks5ServerTcpAccess>> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref())
            .await
            .map_err(|e| {
                error!(?e, "Failed to bind to listen address");
                e
            })?;
        Ok(TcpServer::new(tcp_listener, access))
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> io::Result<Socks5ServerTcpAccess> {
        let access = Socks5ServerTcpAccess::new(
            self.proxy_table,
            self.stream_pool,
            self.filter,
            self.speed_limit,
            self.udp_server_addr.map(Into::into),
            self.users,
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
            Ok(Some(metrics)) => {
                info!(%metrics, "Proxy finished");
            }
            Ok(None) => (),
            Err(ProxyError::IoCopy { source: e, metrics }) => {
                info!(?e, %metrics, "Proxy error");
            }
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
    ) -> Self {
        Self {
            proxy_table,
            stream_pool,
            filter,
            speed_limiter: Limiter::new(speed_limit),
            udp_listen_addr,
            users,
        }
    }

    async fn proxy<S>(&self, downstream: S) -> Result<Option<StreamMetrics>, ProxyError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let start = std::time::Instant::now();

        let downstream_addr = downstream.peer_addr().map_err(ProxyError::DownstreamAddr)?;

        let res = self.establish(downstream).await?;
        let (downstream, (upstream, upstream_addr, upstream_sock_addr), payload_crypto) = match res
        {
            EstablishResult::Blocked { destination } => {
                trace!(?destination, "Blocked");
                return Ok(None);
            }
            EstablishResult::Direct {
                downstream,
                upstream,
                upstream_addr,
            } => {
                match tokio_io::timed_copy_bidirectional(
                    downstream,
                    upstream,
                    self.speed_limiter.clone(),
                )
                .await
                {
                    Ok(metrics) => {
                        info!(%upstream_addr, ?metrics, "Direct finished");
                    }
                    Err(e) => {
                        info!(?e, %upstream_addr, "Direct error");
                    }
                }
                return Ok(None);
            }
            EstablishResult::Udp { mut downstream } => {
                // Prevent the UDP association from terminating
                let mut buf = [0; 1];
                let _ = downstream.read_exact(&mut buf).await;
                return Ok(None);
            }
            EstablishResult::Proxy {
                downstream,
                upstream,
                payload_crypto,
            } => (downstream, upstream, payload_crypto),
        };

        let res = copy_bidirectional_with_payload_crypto(
            downstream,
            upstream,
            payload_crypto.as_ref(),
            self.speed_limiter.clone(),
        )
        .await;
        let end = std::time::Instant::now();
        let (bytes_uplink, bytes_downlink) = res.map_err(|e| ProxyError::IoCopy {
            source: e,
            metrics: FailedStreamMetrics {
                start,
                end,
                upstream_addr: upstream_addr.clone(),
                upstream_sock_addr,
                downstream_addr: Some(downstream_addr),
            },
        })?;

        let metrics = StreamMetrics {
            start,
            end,
            bytes_uplink,
            bytes_downlink,
            upstream_addr,
            upstream_sock_addr,
            downstream_addr: Some(downstream_addr),
        };
        Ok(Some(metrics))
    }

    async fn establish<S>(&self, stream: S) -> Result<EstablishResult<S>, EstablishError>
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        let (mut stream, relay_request) = self
            .steer(stream)
            .await
            .map_err(EstablishError::Negotiate)?;

        // Filter
        let destination_str = relay_request.destination.to_string();
        let action = self.filter.filter(&destination_str);
        if matches!(action, filter::Action::Block) {
            let relay_response = RelayResponse {
                reply: Reply::ConnectionNotAllowedByRuleset,
                bind: InternetAddr::zero_ipv4_addr(),
            };
            relay_response.encode(&mut stream).await?;
            return Ok(EstablishResult::Blocked {
                destination: relay_request.destination,
            });
        }

        match relay_request.command {
            Command::Connect => (),
            Command::Bind => {
                let relay_response = RelayResponse {
                    reply: Reply::CommandNotSupported,
                    bind: InternetAddr::zero_ipv4_addr(),
                };
                relay_response.encode(&mut stream).await?;
                return Err(EstablishError::CmdBindNotSupported);
            }
            Command::UdpAssociate => match &self.udp_listen_addr {
                Some(addr) => {
                    let relay_response = RelayResponse {
                        reply: Reply::Succeeded,
                        bind: addr.clone(),
                    };
                    relay_response.encode(&mut stream).await?;
                    return Ok(EstablishResult::Udp { downstream: stream });
                }
                None => {
                    let relay_response = RelayResponse {
                        reply: Reply::CommandNotSupported,
                        bind: InternetAddr::zero_ipv4_addr(),
                    };
                    relay_response.encode(&mut stream).await?;
                    return Err(EstablishError::NoUdpServerAvailable);
                }
            },
        }

        if matches!(action, filter::Action::Direct) {
            let upstream = tokio::net::TcpStream::connect(&destination_str)
                .await
                .map_err(|e| EstablishError::DirectConnect {
                    source: e,
                    destination: relay_request.destination.clone(),
                })?;
            let relay_response = RelayResponse {
                reply: Reply::Succeeded,
                bind: InternetAddr::zero_ipv4_addr(),
            };
            relay_response.encode(&mut stream).await?;
            return Ok(EstablishResult::Direct {
                downstream: stream,
                upstream,
                upstream_addr: relay_request.destination,
            });
        }

        let (upstream, payload_crypto) =
            match self.establish_proxy_chain(relay_request.destination).await {
                Ok(res) => res,
                Err(e) => {
                    let relay_response = RelayResponse {
                        reply: Reply::GeneralSocksServerFailure,
                        bind: InternetAddr::zero_ipv4_addr(),
                    };
                    relay_response.encode(&mut stream).await?;
                    return Err(e.into());
                }
            };
        let relay_response = RelayResponse {
            reply: Reply::Succeeded,
            bind: InternetAddr::zero_ipv4_addr(),
        };
        relay_response.encode(&mut stream).await?;
        Ok(EstablishResult::Proxy {
            downstream: stream,
            upstream,
            payload_crypto,
        })
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
                    status: UsernamePasswordStatus::Failure(1),
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
                status: UsernamePasswordStatus::Failure(2),
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
    ) -> Result<((CreatedStream, StreamAddr, SocketAddr), Option<XorCrypto>), StreamEstablishError>
    {
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
    },
    Udp {
        downstream: S,
    },
    Proxy {
        downstream: S,
        upstream: (CreatedStream, StreamAddr, SocketAddr),
        payload_crypto: Option<XorCrypto>,
    },
}

#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("Failed to negotiate")]
    Negotiate(#[source] io::Error),
    #[error("Failed to connect directly")]
    DirectConnect {
        #[source]
        source: io::Error,
        destination: InternetAddr,
    },
    #[error("IO error")]
    Io(#[from] io::Error),
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Command BIND not supported")]
    CmdBindNotSupported,
    #[error("No UDP server available")]
    NoUdpServerAvailable,
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Failed to establish connection")]
    Establish(#[from] EstablishError),
    #[error("Failed to get downstream address")]
    DownstreamAddr(#[source] io::Error),
    #[error("Failed to copy data between streams")]
    IoCopy {
        #[source]
        source: tokio_io::CopyBiError,
        metrics: FailedStreamMetrics,
    },
}
