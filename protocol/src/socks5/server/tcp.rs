use std::{collections::HashMap, fmt, io, net::SocketAddr, num::NonZeroU8, sync::Arc};

use crate::stream::{
    addr::ConcreteStreamType,
    streams::tcp::proxy_server::{TCP_STREAM_TYPE, TcpServer},
};
use async_speed_limit::Limiter;
use common::{
    addr::{InternetAddr, InternetAddrStr},
    config::SharableConfig,
    loading::{self, HandleConn},
    proto::{
        addr::StreamAddr,
        client::{self, stream::StreamEstablishError},
        conn::stream::ConnAndAddr,
        context::StreamContext,
        io_copy::stream::{ConnContext, CopyBidirectional},
        log::stream::IoCopyFinished,
        route::{
            StreamRouteGroup, StreamRouteTable, StreamRouteTableBuildContext,
            StreamRouteTableBuilder,
        },
    },
    route::{RouteAction, RouteTableBuildError},
    stream::{AsConn, HasIoAddr, OwnIoStream, StreamServerHandleConn},
    udp::TIMEOUT,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tracing::{trace, warn};

use crate::socks5::messages::{
    Command, MethodIdentifier, NegotiationRequest, NegotiationResponse, RelayRequest,
    RelayResponse, Reply,
    sub_negotiations::{UsernamePasswordRequest, UsernamePasswordResponse, UsernamePasswordStatus},
};

const AUTH_FAILURE: NonZeroU8 = NonZeroU8::MIN;

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
    pub route_table: SharableConfig<StreamRouteTableBuilder>,
    pub speed_limit: Option<f64>,
    pub udp_server_addr: Option<InternetAddrStr>,
    #[serde(default)]
    pub users: Vec<User>,
}
impl Socks5ServerTcpAccessServerConfig {
    pub fn into_builder(
        self,
        route_table: &HashMap<Arc<str>, StreamRouteTable>,
        route_table_cx: StreamRouteTableBuildContext<'_>,
        stream_context: StreamContext,
    ) -> Result<Socks5ServerTcpAccessServerBuilder, BuildError> {
        let route_table = match self.route_table {
            SharableConfig::SharingKey(key) => route_table
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(route_table_cx)?,
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
pub enum BuildError {
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
    route_table: StreamRouteTable,
    speed_limit: f64,
    udp_server_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    stream_context: StreamContext,
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
    route_table: StreamRouteTable,
    speed_limiter: Limiter,
    udp_listen_addr: Option<InternetAddr>,
    users: HashMap<Arc<[u8]>, Arc<[u8]>>,
    stream_context: StreamContext,
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
        route_table: StreamRouteTable,
        speed_limit: f64,
        udp_listen_addr: Option<InternetAddr>,
        users: HashMap<Arc<[u8]>, Arc<[u8]>>,
        stream_context: StreamContext,
        listen_addr: Arc<str>,
    ) -> Self {
        Self {
            route_table,
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
                let cmd = "CONNECT".to_string();
                let dst = upstream_addr.clone();
                let upstream_addr = StreamAddr {
                    stream_type: ConcreteStreamType::Tcp.to_string().into(),
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
                let io_copy = CopyBidirectional {
                    downstream,
                    upstream,
                    payload_crypto: None,
                    speed_limiter: self.speed_limiter.clone(),
                    conn_context,
                }
                .serve_as_access_server();
                tokio::spawn(async move {
                    let (io, res) = io_copy.await;
                    let log = Socks5TcpLog { io, cmd, dst };
                    match &res {
                        Ok(()) => common::info_println!("SOCKS5 TCP direct: Finished {log}"),
                        Err(err) => common::info_println!("SOCKS5 TCP direct: Error {log}: {err}"),
                    }
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
            destination: Some(StreamAddr {
                stream_type: ConcreteStreamType::Tcp.to_string().into(),
                address: destination,
            }),
        };
        let io_copy = CopyBidirectional {
            downstream,
            upstream: upstream.stream,
            payload_crypto,
            speed_limiter: self.speed_limiter.clone(),
            conn_context,
        }
        .serve_as_access_server();
        tokio::spawn(async move {
            let (io, res) = io_copy.await;
            let log = Socks5TcpLog { io, cmd, dst };
            match &res {
                Ok(()) => common::info_println!("SOCKS5 TCP: Finished {log}"),
                Err(err) => common::info_println!("SOCKS5 TCP: Error {log}: {err}"),
            }
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
                    .timed_connect_2(TCP_STREAM_TYPE, sock_addrs, TIMEOUT)
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
            RouteAction::ConnSelector(conn_selector) => conn_selector,
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
        Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let request = UsernamePasswordRequest::decode(&mut stream).await?;
        if let Err(e) = self.authenticate(&request) {
            let response = UsernamePasswordResponse {
                status: UsernamePasswordStatus::Failure(AUTH_FAILURE),
            };
            response.encode(&mut stream).await?;
            return Err(e);
        }
        let response = UsernamePasswordResponse {
            status: UsernamePasswordStatus::Success,
        };
        response.encode(&mut stream).await?;
        Ok(stream)
    }

    fn authenticate(&self, request: &UsernamePasswordRequest) -> io::Result<()> {
        let expected = self.users.get(request.username());
        let filler = vec![0; request.password().len()];
        let matches = Self::password_matches(
            request.password(),
            expected.map_or(filler.as_slice(), |password| password),
        );
        if expected.is_none() {
            return Err(io::Error::other(format!(
                "Username not found: {}",
                String::from_utf8_lossy(request.username())
            )));
        }
        if !matches {
            return Err(Self::password_incorrect_error(request));
        }
        Ok(())
    }

    fn password_matches(offered: &[u8], expected: &[u8]) -> bool {
        constant_time_eq::constant_time_eq(offered, expected)
    }

    fn password_incorrect_error(request: &UsernamePasswordRequest) -> io::Error {
        io::Error::other(format!(
            "Password incorrect: {{ username: {} }}",
            String::from_utf8_lossy(request.username())
        ))
    }

    async fn establish_proxy_chain(
        &self,
        conn_selector: &StreamRouteGroup,
        destination: InternetAddr,
    ) -> Result<(ConnAndAddr, Option<tokio_chacha20::config::Config>), EstablishProxyChainError>
    {
        let (chain, payload_crypto) = match &conn_selector {
            common::route::ConnSelector::Empty => ([].into(), None),
            common::route::ConnSelector::Some(conn_selector1) => {
                let proxy_chain = conn_selector1.choose_chain();
                (
                    proxy_chain.chain.clone(),
                    proxy_chain.payload_crypto.clone(),
                )
            }
        };
        let res = client::stream::establish(
            &chain,
            StreamAddr {
                address: destination,
                stream_type: ConcreteStreamType::Tcp.to_string().into(),
            },
            &self.stream_context,
        )
        .await?;
        Ok((res, payload_crypto))
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
        upstream: Box<dyn AsConn>,
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
        upstream: Box<dyn AsConn>,
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::socks5::messages::sub_negotiations::USERNAME_PASSWORD_VERSION;
    use crate::stream::connect::build_concrete_stream_connector_table;
    use ae::anti_replay::ReplayValidator;
    use common::{
        anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        connect::{ConnectorConfig, ConnectorReset},
        notify::Notify,
        route::RouteTable,
        stream::pool::StreamConnPool,
    };
    use swap::Swap;
    use tokio::io::AsyncWriteExt;

    fn handler(users: &[(&[u8], &[u8])]) -> Socks5ServerTcpAccessConnHandler {
        let stream_context = StreamContext {
            session_table: None,
            pool: Swap::new(StreamConnPool::empty()),
            connector_table: Arc::new(build_concrete_stream_connector_table(
                ConnectorConfig {
                    bind: common::addr::BothVerIp { v4: None, v6: None },
                },
                ConnectorReset(Notify::new()),
            )),
            replay_validator: Arc::new(ReplayValidator::new(
                VALIDATOR_TIME_FRAME,
                VALIDATOR_CAPACITY,
            )),
        };
        Socks5ServerTcpAccessConnHandler::new(
            RouteTable::new(vec![], Arc::new(HashMap::new())),
            f64::INFINITY,
            None,
            users
                .iter()
                .map(|(u, p)| ((*u).into(), (*p).into()))
                .collect(),
            stream_context,
            "127.0.0.1:0".into(),
        )
    }

    async fn login(
        handler: &Socks5ServerTcpAccessConnHandler,
        username: &[u8],
        password: &[u8],
    ) -> Vec<u8> {
        let (server, mut client) = tokio::io::duplex(1024);
        let request = UsernamePasswordRequest::new(username, password).unwrap();
        request.encode(&mut client).await.unwrap();
        let _ = handler.username_password(server).await;
        client.shutdown().await.unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn a_rejected_login_never_says_whether_the_username_exists() {
        let handler = handler(&[(b"alice", b"hunter2")]);
        let no_such_user = login(&handler, b"bob", b"hunter2").await;
        let wrong_password = login(&handler, b"alice", b"hunter3").await;
        assert_eq!(
            no_such_user, wrong_password,
            "the reply tells the two failures apart"
        );
        let ok = login(&handler, b"alice", b"hunter2").await;
        assert_ne!(ok, no_such_user);
        assert_eq!(
            ok,
            vec![
                USERNAME_PASSWORD_VERSION,
                UsernamePasswordStatus::Success.into()
            ]
        );
    }

    #[test]
    fn only_the_exact_password_matches() {
        let matches = Socks5ServerTcpAccessConnHandler::password_matches;
        assert!(matches(b"hunter2", b"hunter2"));
        assert!(!matches(b"hunter3", b"hunter2"));
        assert!(!matches(b"Hunter2", b"hunter2"));
        assert!(!matches(b"hunter", b"hunter2"));
        assert!(!matches(b"hunter22", b"hunter2"));
        assert!(!matches(b"", b"hunter2"));
        assert!(matches(b"", b""));
    }

    #[test]
    fn a_rejected_login_never_logs_the_password() {
        let request = UsernamePasswordRequest::new(b"alice", b"hunter2").unwrap();
        let err = Socks5ServerTcpAccessConnHandler::password_incorrect_error(&request);
        for rendered in [format!("{err}"), format!("{err:?}")] {
            assert!(
                !rendered.contains("hunter2"),
                "the password reached the log: {rendered}"
            );
            assert!(
                rendered.contains("alice"),
                "the username is what makes the log useful: {rendered}"
            );
        }
    }
}
