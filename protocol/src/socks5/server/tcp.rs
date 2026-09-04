use std::{collections::HashMap, fmt, io, net::SocketAddr, sync::Arc};

use super::auth::Users;
use crate::stream_proto::{
    addr::ConcreteStreamType,
    streams::tcp::listener::{TCP_STREAM_TYPE, TcpServer},
};
use async_speed_limit::Limiter;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    loading::{self, HandleConn},
    proxy_runtime::{
        addr::RouteAddr,
        client::{self, stream::StreamEstablishError},
        conn::stream::ConnAndAddr,
        context::StreamRuntime,
        log::stream::IoCopyFinished,
        relay::stream::{ConnContext, CopyBidirectional},
    },
    route::{
        ProbeFutures, Registries, RouteAction, RouteSelector, RouteTable, RouteTableBuildError,
        RouteTableBuilder,
    },
    stream_runtime::{HasIoAddr, IoConnection, OwnedIoStream, StreamServerHandleConn},
    udp_runtime::UDP_FLOW_TIMEOUT,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tracing::{info, trace, warn};

use crate::socks5::messages::{Command, RelayRequest, RelayResponse, Reply};

pub struct Socks5TcpLog {
    pub io: IoCopyFinished,
    pub cmd: String,
    pub dst: InternetAddr,
}

impl fmt::Display for Socks5TcpLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.io)?;
        write!(f, ",cmd:{}", self.cmd)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Socks5ServerTcpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub route_table: SharableConfig<RouteTableBuilder>,
    pub speed_limit: Option<f64>,
    pub udp_server_addr: Option<InternetAddrStr>,
    #[serde(default)]
    pub users: Vec<User>,
}
impl Socks5ServerTcpAccessServerConfig {
    pub fn into_builder(
        self,
        route_table: &HashMap<Arc<str>, RouteTable>,
        registries: &Registries<'_>,
        stream_context: StreamRuntime,
        probes: &mut ProbeFutures,
    ) -> Result<Socks5ServerTcpAccessServerBuilder, Socks5TcpBuildError> {
        let route_table = match self.route_table {
            SharableConfig::SharingKey(key) => route_table
                .get(&key)
                .ok_or_else(|| Socks5TcpBuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.resolve(registries, probes)?,
        };
        let users = self
            .users
            .into_iter()
            .map(|u| (u.username.as_bytes().into(), u.password.as_bytes().into()))
            .collect();

        Ok(Socks5ServerTcpAccessServerBuilder {
            listen_addr: self.listen_addr,
            route_table,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_server_addr: self.udp_server_addr.map(|a| a.0),
            users,
            stream_context,
        })
    }
}
#[derive(Debug, Error)]
pub enum Socks5TcpBuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("Filter key not found: {0}")]
    FilterKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyTable(#[from] RouteTableBuildError),
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
    route_table: RouteTable,
    speed_limit: f64,
    udp_server_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    stream_context: StreamRuntime,
}
impl loading::Build for Socks5ServerTcpAccessServerBuilder {
    type ConnHandler = Socks5ServerTcpAccessConnHandler;
    type Server = TcpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.stream_context.session_spawner.clone();
        let access = self.build_conn_handler()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref()).await?;
        Ok(TcpServer::new(tcp_listener, access, session_spawner))
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let access = Socks5ServerTcpAccessConnHandler::new(
            self.route_table,
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
    route_table: RouteTable,
    speed_limiter: Limiter,
    udp_listen_addr: Option<InternetAddr>,
    users: Users,
    stream_context: StreamRuntime,
    listen_addr: Arc<str>,
}
impl HandleConn for Socks5ServerTcpAccessConnHandler {}
impl StreamServerHandleConn for Socks5ServerTcpAccessConnHandler {
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: OwnedIoStream + HasIoAddr + std::fmt::Debug,
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
        route_table: RouteTable,
        speed_limit: f64,
        udp_listen_addr: Option<InternetAddr>,
        users: HashMap<Arc<[u8]>, Arc<[u8]>>,
        stream_context: StreamRuntime,
        listen_addr: Arc<str>,
    ) -> Self {
        Self {
            route_table,
            speed_limiter: Limiter::new(speed_limit),
            udp_listen_addr,
            users: Users::new(users),
            stream_context,
            listen_addr,
        }
    }

    async fn proxy<Downstream>(
        &self,
        downstream: Downstream,
    ) -> Result<ProxyResult, Socks5ProxyError>
    where
        Downstream: OwnedIoStream + HasIoAddr + std::fmt::Debug,
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
                let cmd = "CONNECT".to_string();
                let dst = upstream_addr.clone();
                let upstream_addr = RouteAddr {
                    protocol: ConcreteStreamType::Tcp.to_string().into(),
                    address: upstream_addr,
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
                let retention = self.stream_context.retention.clone();
                let io_copy = CopyBidirectional {
                    downstream,
                    upstream,
                    payload_crypto: None,
                    speed_limiter: self.speed_limiter.clone(),
                    conn_context,
                    retention,
                }
                .serve_as_access_server();
                let (io, res) = io_copy.await;
                let log = Socks5TcpLog { io, cmd, dst };
                match &res {
                    Ok(()) => info!("SOCKS5 TCP direct: Finished {log}"),
                    Err(err) => info!("SOCKS5 TCP direct: Error {log}: {err}"),
                }
                return Ok(ProxyResult::IoCopy);
            }
            EstablishResult::Udp { mut downstream } => {
                // Prevent the UDP association from terminating
                let mut buf = [0; 1];
                let _ = downstream.read_exact(&mut buf).await;
                return Ok(ProxyResult::Udp);
            }
            EstablishResult::Proxy {
                destination,
                downstream,
                upstream,
                payload_crypto,
            } => (destination, downstream, upstream, payload_crypto),
        };

        let cmd = "CONNECT".to_string();
        let dst = destination.clone();
        let conn_context = ConnContext {
            start: (std::time::Instant::now(), std::time::SystemTime::now()),
            upstream_remote: upstream.addr,
            upstream_remote_sock: upstream.sock_addr,
            upstream_local: upstream.stream.local_addr().ok(),
            downstream_remote: downstream.peer_addr().ok(),
            downstream_local: Arc::clone(&self.listen_addr),
            session_table: self.stream_context.session_table.clone(),
            destination: Some(RouteAddr {
                protocol: ConcreteStreamType::Tcp.to_string().into(),
                address: destination,
            }),
        };
        let retention = self.stream_context.retention.clone();
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto,
            speed_limiter: self.speed_limiter.clone(),
            conn_context,
            retention,
        }
        .serve_as_access_server();
        let (io, res) = io_copy.await;
        let log = Socks5TcpLog { io, cmd, dst };
        match &res {
            Ok(()) => info!("SOCKS5 TCP: Finished {log}"),
            Err(err) => info!("SOCKS5 TCP: Error {log}: {err}"),
        }
        Ok(ProxyResult::IoCopy)
    }

    async fn establish<Stream>(
        &self,
        stream: Stream,
    ) -> Result<EstablishResult<Stream>, EstablishError>
    where
        Stream: OwnedIoStream + HasIoAddr + std::fmt::Debug,
    {
        let (mut stream, relay_request) = self
            .negotiate_request(stream)
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
        let action = self.route_table.action(&relay_request.destination);
        let conn_selector = match action {
            RouteAction::Block => {
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
            RouteAction::Direct => {
                let sock_addrs = match relay_request.destination.to_socket_addrs().await {
                    Ok(sock_addrs) => sock_addrs,
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
                let (upstream, sock_addr) = match self
                    .stream_context
                    .connector_table
                    .timed_connect_any(TCP_STREAM_TYPE, sock_addrs, UDP_FLOW_TIMEOUT)
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
            RouteAction::RouteSelector(conn_selector) => conn_selector,
        };

        let (upstream, payload_crypto) = match self
            .establish_proxy_chain(conn_selector, relay_request.destination.clone())
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

    async fn negotiate_request<Stream>(&self, stream: Stream) -> io::Result<(Stream, RelayRequest)>
    where
        Stream: OwnedIoStream + HasIoAddr + std::fmt::Debug,
    {
        let mut stream = self.users.negotiate(stream).await?;

        let relay_request = RelayRequest::decode(&mut stream).await?;

        Ok((stream, relay_request))
    }

    async fn establish_proxy_chain(
        &self,
        conn_selector: &RouteSelector,
        destination: InternetAddr,
    ) -> Result<(ConnAndAddr, Option<tokio_chacha20::config::Config>), EstablishProxyChainError>
    {
        let chain = match &conn_selector {
            common::route::RouteSelector::Empty => [].into(),
            common::route::RouteSelector::Some(non_empty_conn_selector) => {
                non_empty_conn_selector.choose_chain().chain.clone()
            }
        };
        let res = client::stream::establish(
            &chain,
            RouteAddr {
                address: destination,
                protocol: ConcreteStreamType::Tcp.to_string().into(),
            },
            &self.stream_context,
        )
        .await?;
        Ok((res, None))
    }
}
#[derive(Debug, Error)]
pub enum EstablishProxyChainError {
    #[error("{0}")]
    StreamEstablish(#[from] StreamEstablishError),
}
pub enum EstablishResult<S> {
    Blocked {
        destination: InternetAddr,
    },
    Direct {
        downstream: S,
        upstream: Box<dyn IoConnection>,
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
enum RequestResult {
    Blocked {
        destination: InternetAddr,
    },
    Direct {
        upstream: Box<dyn IoConnection>,
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
pub enum Socks5ProxyError {
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
