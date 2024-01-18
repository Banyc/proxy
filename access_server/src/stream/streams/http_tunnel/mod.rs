use std::{collections::HashMap, io, sync::Arc, time::SystemTime};

use async_speed_limit::Limiter;
use bytes::Bytes;
use common::{
    addr::{InternetAddr, ParseInternetAddrError},
    config::SharableConfig,
    filter::{self, Filter, FilterBuilder},
    loading,
    stream::{
        addr::StreamAddr,
        concrete::{addr::ConcreteStreamType, pool::ConcreteConnPool, streams::tcp::TcpServer},
        io_copy::{CopyBidirectional, DEAD_SESSION_RETENTION_DURATION},
        metrics::{SimplifiedStreamMetrics, SimplifiedStreamProxyMetrics, StreamRecord},
        proxy_table::StreamProxyTable,
        session_table::{Session, StreamSessionTable},
        IoAddr, IoStream, StreamServerHook,
    },
};
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::{
    body::Incoming, http, service::service_fn, upgrade::Upgraded, Method, Request, Response,
};
use hyper_util::rt::TokioIo;
use monitor_table::table::RowOwnedGuard;
use proxy_client::stream::{establish, StreamEstablishError};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::ToSocketAddrs,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, trace, warn};

use crate::stream::proxy_table::{StreamProxyTableBuildError, StreamProxyTableBuilder};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub proxy_table: SharableConfig<StreamProxyTableBuilder>,
    pub filter: SharableConfig<FilterBuilder>,
    pub speed_limit: Option<f64>,
}

impl HttpAccessServerConfig {
    pub fn into_builder(
        self,
        stream_pool: ConcreteConnPool,
        proxy_tables: &HashMap<Arc<str>, StreamProxyTable<ConcreteStreamType>>,
        filters: &HashMap<Arc<str>, Filter>,
        cancellation: CancellationToken,
        session_table: Option<StreamSessionTable<ConcreteStreamType>>,
    ) -> Result<HttpAccessServerBuilder, BuildError> {
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

        Ok(HttpAccessServerBuilder {
            listen_addr: self.listen_addr,
            proxy_table,
            stream_pool,
            filter,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            session_table,
        })
    }
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
pub struct HttpAccessServerBuilder {
    listen_addr: Arc<str>,
    proxy_table: StreamProxyTable<ConcreteStreamType>,
    stream_pool: ConcreteConnPool,
    filter: Filter,
    speed_limit: f64,
    session_table: Option<StreamSessionTable<ConcreteStreamType>>,
}

impl loading::Builder for HttpAccessServerBuilder {
    type Hook = HttpAccess;
    type Server = TcpServer<Self::Hook>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_hook()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_hook(self) -> Result<Self::Hook, Self::Err> {
        let access = HttpAccess::new(
            self.proxy_table,
            self.stream_pool,
            self.filter,
            self.speed_limit,
            self.session_table,
        );
        Ok(access)
    }
}

#[derive(Debug)]
pub struct HttpAccess {
    proxy_table: Arc<StreamProxyTable<ConcreteStreamType>>,
    stream_pool: ConcreteConnPool,
    filter: Filter,
    speed_limiter: Limiter,
    session_table: Option<StreamSessionTable<ConcreteStreamType>>,
}

impl HttpAccess {
    pub fn new(
        proxy_table: StreamProxyTable<ConcreteStreamType>,
        stream_pool: ConcreteConnPool,
        filter: Filter,
        speed_limit: f64,
        session_table: Option<StreamSessionTable<ConcreteStreamType>>,
    ) -> Self {
        Self {
            proxy_table: Arc::new(proxy_table),
            stream_pool,
            filter,
            speed_limiter: Limiter::new(speed_limit),
            session_table,
        }
    }

    #[instrument(skip(self, listen_addr))]
    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<TcpServer<Self>> {
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr).await?;
        Ok(TcpServer::new(tcp_listener, self))
    }

    async fn proxy<S>(&self, downstream: S) -> Result<(), TunnelError>
    where
        S: IoStream,
    {
        hyper::server::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .serve_connection(
                TokioIo::new(downstream),
                service_fn(|req| self.proxy_svc(req)),
            )
            .with_upgrades()
            .await?;
        Ok(())
    }

    #[instrument(skip(self, req))]
    async fn proxy_svc(
        &self,
        req: Request<hyper::body::Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
        trace!(?req, "Received request");

        if Method::CONNECT == req.method() {
            return self.proxy_connect(req).await;
        }

        let start = (std::time::Instant::now(), std::time::SystemTime::now());

        let method = req.method().clone();
        let host = req.uri().host().ok_or(TunnelError::HttpNoHost)?;
        let port = req.uri().port_u16().unwrap_or(80);
        let addr = InternetAddr::from_host_and_port(host, port)?;
        let addr = StreamAddr {
            address: addr,
            stream_type: ConcreteStreamType::Tcp,
        };

        let action = self.filter.filter(&addr.address);
        match action {
            filter::Action::Proxy => (),
            filter::Action::Block => {
                trace!(?addr, "Blocked {}", method);
                return Ok(respond_with_rejection());
            }
            filter::Action::Direct => {
                let sock_addr = addr
                    .address
                    .to_socket_addr()
                    .await
                    .map_err(TunnelError::Direct)?;

                let upstream = tokio::net::TcpStream::connect(sock_addr)
                    .await
                    .map_err(TunnelError::Direct)?;
                let session_guard = self.session_table.as_ref().map(|s| {
                    s.set_scope_owned(Session {
                        start: SystemTime::now(),
                        end: None,
                        destination: Some(addr.clone()),
                        upstream_local: upstream.local_addr().ok(),
                        upstream_remote: addr.clone(),
                        downstream_remote: None,
                        up_gauge: None,
                        dn_gauge: None,
                    })
                });
                let res = tls_http(upstream, req, session_guard).await;
                info!(%addr, "Direct {} finished", method);
                return res;
            }
        }

        // Establish proxy chain
        let proxy_chain = self.proxy_table.choose_chain();
        let upstream = establish(&proxy_chain.chain, addr.clone(), &self.stream_pool).await?;

        let session_guard = self.session_table.as_ref().map(|s| {
            s.set_scope_owned(Session {
                start: SystemTime::now(),
                end: None,
                destination: Some(addr.clone()),
                upstream_local: upstream.stream.local_addr().ok(),
                upstream_remote: upstream.addr.clone(),
                downstream_remote: None,
                up_gauge: None,
                dn_gauge: None,
            })
        });
        let res = match &proxy_chain.payload_crypto {
            Some(crypto) => {
                // Establish encrypted stream
                let (r, w) = tokio::io::split(upstream.stream);
                let upstream =
                    tokio_chacha20::stream::WholeStream::from_key_halves(*crypto.key(), r, w);

                tls_http(upstream, req, session_guard).await
            }
            None => tls_http(upstream.stream, req, session_guard).await,
        };

        let end = std::time::Instant::now();
        let metrics = SimplifiedStreamProxyMetrics {
            stream: SimplifiedStreamMetrics {
                start,
                end,
                upstream_addr: upstream.addr,
                upstream_sock_addr: upstream.sock_addr,
                downstream_addr: None,
            },
            destination: addr.address,
        };
        info!(%metrics, "{} finished", method);

        table_log::log!(&StreamRecord::SimplifiedProxyMetrics(&metrics));

        res
    }

    async fn proxy_connect(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
        // Received an HTTP request like:
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // When HTTP method is CONNECT we should return an empty body
        // then we can eventually upgrade the connection and talk a new protocol.
        //
        // Note: only after client received an empty body with STATUS_OK can the
        // connection be upgraded, so we can't return a response inside
        // `on_upgrade` future.
        let addr = match host_addr(req.uri()) {
            Some(addr) => addr,
            None => {
                let uri = req.uri().to_string();
                warn!(?uri, "CONNECT host is not socket addr");
                let mut resp = Response::new(full("CONNECT must be to a socket address"));
                *resp.status_mut() = http::StatusCode::BAD_REQUEST;

                return Ok(resp);
            }
        };
        let addr = addr.parse()?;
        let action = self.filter.filter(&addr);
        let http_connect = match action {
            filter::Action::Proxy => Some(HttpConnect::new(
                Arc::clone(&self.proxy_table),
                self.stream_pool.clone(),
                self.speed_limiter.clone(),
                self.session_table.clone(),
            )),
            filter::Action::Block => {
                trace!(?addr, "Blocked CONNECT");
                return Ok(respond_with_rejection());
            }
            filter::Action::Direct => None,
        };

        let speed_limiter = self.speed_limiter.clone();
        let session_table = self.session_table.clone();
        tokio::task::spawn(async move {
            upgrade(req, addr, http_connect, speed_limiter, session_table).await;
        });

        // Return STATUS_OK
        Ok(Response::new(empty()))
    }
}

async fn tls_http<S>(
    upstream: S,
    req: Request<Incoming>,
    session_guard: Option<RowOwnedGuard<Session<ConcreteStreamType>>>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError>
where
    S: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
    // Establish TLS connection
    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(TokioIo::new(upstream))
        .await
        .map_err(|e| {
            warn!(?e, "Failed to establish HTTP/1 handshake to upstream");
            e
        })?;
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            warn!(?err, "Connection failed");
        }
    });

    // Send HTTP/1 request
    let resp = sender.send_request(req).await.map_err(|e| {
        warn!(?e, "Failed to send HTTP/1 request to upstream");
        e
    })?;

    if let Some(s) = &session_guard {
        s.inspect_mut(|session| session.end = Some(SystemTime::now()));
    }
    tokio::spawn(async move {
        let _session_guard = session_guard;
        tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
    });

    Ok(resp.map(|b| b.boxed()))
}

async fn upgrade(
    req: Request<Incoming>,
    addr: InternetAddr,
    http_connect: Option<HttpConnect>,
    speed_limiter: Limiter,
    session_table: Option<StreamSessionTable<ConcreteStreamType>>,
) {
    let upgraded = match hyper::upgrade::on(req).await {
        Ok(upgraded) => upgraded,
        Err(e) => {
            warn!(?e, ?addr, "Upgrade error");
            return;
        }
    };

    let start = (std::time::Instant::now(), std::time::SystemTime::now());

    // Proxy
    if let Some(http_connect) = http_connect {
        match http_connect.proxy(upgraded, addr).await {
            Ok(()) => (),
            Err(e) => warn!(?e, "CONNECT error"),
        };
        return;
    }

    // Direct
    let sock_addr = match addr.to_socket_addr().await {
        Ok(sock_addr) => sock_addr,
        Err(e) => {
            warn!(?e, ?addr, "Failed to resolve address");
            return;
        }
    };
    let upstream = match tokio::net::TcpStream::connect(sock_addr).await {
        Ok(upstream) => upstream,
        Err(e) => {
            warn!(
                ?e,
                ?addr,
                ?sock_addr,
                "Failed to connect to upstream directly"
            );
            return;
        }
    };

    let upstream_local = upstream.local_addr().ok();
    let io_copy = CopyBidirectional {
        downstream: TokioIo::new(upgraded),
        upstream,
        payload_crypto: None,
        speed_limiter,
        start,
        upstream_addr: StreamAddr {
            stream_type: ConcreteStreamType::Tcp,
            address: addr.clone(),
        },
        upstream_sock_addr: sock_addr,
        downstream_addr: None,
    };
    let _ = io_copy
        .serve_as_access_server(
            StreamAddr {
                stream_type: ConcreteStreamType::Tcp,
                address: addr,
            },
            session_table,
            upstream_local,
            "HTTP CONNECT direct",
        )
        .await;
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Hyper error: {0}")]
    HyperError(#[from] hyper::Error),
    #[error("No host in HTTP request")]
    HttpNoHost,
    #[error("Direct connection error: {0}")]
    Direct(#[source] io::Error),
    #[error("Invalid address: {0}")]
    Address(#[from] ParseInternetAddrError),
}

impl loading::Hook for HttpAccess {}

impl StreamServerHook for HttpAccess {
    #[instrument(skip(self, stream))]
    async fn handle_stream<S>(&self, stream: S)
    where
        S: IoStream,
    {
        let res = self.proxy(stream).await;
        if let Err(e) = res {
            warn!(?e, "Failed to proxy");
        }
    }
}

struct HttpConnect {
    proxy_table: Arc<StreamProxyTable<ConcreteStreamType>>,
    stream_pool: ConcreteConnPool,
    speed_limiter: Limiter,
    session_table: Option<StreamSessionTable<ConcreteStreamType>>,
}

impl HttpConnect {
    pub fn new(
        proxy_table: Arc<StreamProxyTable<ConcreteStreamType>>,
        stream_pool: ConcreteConnPool,
        speed_limiter: Limiter,
        session_table: Option<StreamSessionTable<ConcreteStreamType>>,
    ) -> Self {
        Self {
            proxy_table,
            stream_pool,
            speed_limiter,
            session_table,
        }
    }

    // Create a TCP connection to host:port, build a tunnel between the connection and
    // the upgraded connection
    #[instrument(skip(self, upgraded))]
    pub async fn proxy(
        &self,
        upgraded: Upgraded,
        address: InternetAddr,
    ) -> Result<(), HttpConnectError> {
        let start = (std::time::Instant::now(), std::time::SystemTime::now());

        // Establish proxy chain
        let destination = StreamAddr {
            address: address.clone(),
            stream_type: ConcreteStreamType::Tcp,
        };
        let proxy_chain = self.proxy_table.choose_chain();
        let upstream = establish(&proxy_chain.chain, destination, &self.stream_pool).await?;

        let speed_limiter = self.speed_limiter.clone();
        let session_table = self.session_table.clone();
        let payload_crypto = proxy_chain.payload_crypto.clone();
        tokio::spawn(async move {
            let upstream_local = upstream.stream.local_addr().ok();
            let io_copy = CopyBidirectional {
                downstream: TokioIo::new(upgraded),
                upstream: upstream.stream,
                payload_crypto,
                speed_limiter,
                start,
                upstream_addr: upstream.addr,
                upstream_sock_addr: upstream.sock_addr,
                downstream_addr: None,
            };
            let _ = io_copy
                .serve_as_access_server(
                    StreamAddr {
                        address: address.clone(),
                        stream_type: ConcreteStreamType::Tcp,
                    },
                    session_table,
                    upstream_local,
                    "HTTP CONNECT",
                )
                .await;
        });
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum HttpConnectError {
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

fn respond_with_rejection() -> Response<BoxBody<Bytes, hyper::Error>> {
    Response::builder()
        .status(503)
        .body(full("Blocked"))
        .unwrap()
}
