use std::{
    collections::HashMap,
    io,
    sync::OnceLock,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use crate::stream::streams::{
    http_tunnel::{proxy::dispatch_proxy, tunnel::dispatch_tunnel},
    tcp::listener::TcpServer,
};
use async_speed_limit::Limiter;
use bytes::Bytes;
use common::{
    addr::ParseInternetAddrError,
    config::SharableConfig,
    loading,
    proto::{client::stream::StreamEstablishError, context::StreamRuntime},
    route::{Registries, RouteTable, RouteTableBuildError, RouteTableBuilder},
    stream::{HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Method, Request, Response, service::service_fn};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{instrument, trace, warn};

mod authority;
mod failure;
mod proxy;
mod tunnel;

use authority::{host_and_port, redacted_uri};
use failure::{
    HttpDownstreamContext, HttpFailureReporter, HttpRequestFailure, RequestErrorContext,
};

type HttpResult = Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError>;

fn retain_failed_request_context(
    request_error_context: &RequestErrorContext,
    reporter: HttpFailureReporter,
    result: HttpResult,
) -> HttpResult {
    if let Err(error) = &result {
        *request_error_context.lock().unwrap() = Some(reporter.clone());
        reporter.report(error, None);
    }
    result
}

#[derive(Debug, Clone)]
struct HttpRequestContext {
    method: Method,
    uri: String,
    host: Option<String>,
    authority: Option<String>,
}
impl HttpRequestContext {
    fn from_request<B>(req: &Request<B>) -> Self {
        let host = req.headers().get(hyper::header::HOST).map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .unwrap_or_else(|_| format!("{value:?}"))
        });
        let authority = req
            .uri()
            .authority()
            .map(|a| host_and_port(a).to_owned())
            .or_else(|| host.clone());
        Self {
            method: req.method().clone(),
            uri: redacted_uri(req.uri()),
            host,
            authority,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub route_table: SharableConfig<RouteTableBuilder>,
    pub speed_limit: Option<f64>,
}
impl HttpAccessServerConfig {
    pub fn into_builder(
        self,
        route_tables: &HashMap<Arc<str>, RouteTable>,
        registries: &Registries<'_>,
        stream_context: StreamRuntime,
    ) -> Result<HttpAccessServerBuilder, HttpBuildError> {
        let route_table = match self.route_table {
            SharableConfig::SharingKey(key) => route_tables
                .get(&key)
                .ok_or_else(|| HttpBuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.resolve(registries)?,
        };

        Ok(HttpAccessServerBuilder {
            listen_addr: self.listen_addr,
            route_table,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            stream_context,
        })
    }
}
#[derive(Debug, Error)]
pub enum HttpBuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyTable(#[from] RouteTableBuildError),
}

#[derive(Debug, Clone)]
pub struct HttpAccessServerBuilder {
    listen_addr: Arc<str>,
    route_table: RouteTable,
    speed_limit: f64,
    stream_context: StreamRuntime,
}
impl loading::Build for HttpAccessServerBuilder {
    type ConnHandler = HttpAccessConnHandler;
    type Server = TcpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_conn_handler()?;
        let tcp_listener = tokio::net::TcpListener::bind(listen_addr.as_ref()).await?;
        let server = TcpServer::new(tcp_listener, access);
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let access = HttpAccessConnHandler::new(
            self.route_table,
            self.speed_limit,
            self.stream_context,
            Arc::clone(&self.listen_addr),
        );
        Ok(access)
    }
}

#[derive(Debug, Clone)]
struct HttpAccessConnContext {
    pub route_table: Arc<RouteTable>,
    pub speed_limiter: Limiter,
    pub stream_context: StreamRuntime,
    pub listen_addr: Arc<str>,
}

#[derive(Debug)]
pub struct HttpAccessConnHandler {
    ctx: HttpAccessConnContext,
}
impl HttpAccessConnHandler {
    pub fn new(
        route_table: RouteTable,
        speed_limit: f64,
        stream_context: StreamRuntime,
        listen_addr: Arc<str>,
    ) -> Self {
        let ctx = HttpAccessConnContext {
            route_table: Arc::new(route_table),
            speed_limiter: Limiter::new(speed_limit),
            stream_context,
            listen_addr,
        };
        Self { ctx }
    }
}
impl loading::HandleConn for HttpAccessConnHandler {}
impl StreamServerHandleConn for HttpAccessConnHandler {
    #[instrument(skip_all)]
    async fn handle_stream<Stream>(&self, stream: Stream)
    where
        Stream: OwnIoStream + HasIoAddr,
    {
        let dn_remote = stream.peer_addr().ok();
        let dn_local = stream.local_addr().ok();
        let downstream = HttpDownstreamContext {
            remote: dn_remote,
            local: dn_local,
        };
        let listener = Arc::clone(&self.ctx.listen_addr);
        if let Err(_e) = proxy(&self.ctx, stream, downstream, listener).await {}
    }
}

async fn proxy<Downstream>(
    ctx: &HttpAccessConnContext,
    downstream: Downstream,
    downstream_ctx: HttpDownstreamContext,
    listener: Arc<str>,
) -> Result<(), TunnelError>
where
    Downstream: OwnIoStream,
{
    let downstream_ctx = Arc::new(downstream_ctx);
    let listener = Arc::clone(&listener);
    let request_error_context: RequestErrorContext = Arc::new(Mutex::new(None));
    let result = hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(
            TokioIo::new(downstream),
            service_fn(|req| {
                run_service(
                    ctx,
                    req,
                    Arc::clone(&downstream_ctx),
                    Arc::clone(&listener),
                    &request_error_context,
                )
            }),
        )
        .with_upgrades()
        .await
        .map_err(TunnelError::HyperError);
    if let Err(ref e) = result {
        if let Some(reporter) = request_error_context.lock().unwrap().take() {
            reporter.report(e, None);
        } else {
            warn!(
                event = "http_tunnel_proxy_failed",
                error = %e,
                dn = ?downstream_ctx.remote,
                dn_local = ?downstream_ctx.local,
                listener = %ctx.listen_addr,
                "HTTP tunnel proxy top-level failure"
            );
        }
    }
    result
}
#[instrument(skip_all)]
async fn run_service(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
    downstream_ctx: Arc<HttpDownstreamContext>,
    listener: Arc<str>,
    request_error_context: &RequestErrorContext,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
    let request_context = HttpRequestContext::from_request(&req);
    trace!(?request_context, "Received request");
    let failure = Arc::new(HttpRequestFailure {
        request: request_context,
        destination: OnceLock::new(),
        reported: AtomicBool::new(false),
    });
    let reporter = HttpFailureReporter {
        failure: Arc::clone(&failure),
        downstream: (*downstream_ctx).clone(),
        listener,
    };
    let res = if Method::CONNECT == req.method() {
        dispatch_tunnel(ctx, req, reporter.clone()).await
    } else {
        dispatch_proxy(ctx, req, reporter.clone()).await
    };
    retain_failed_request_context(request_error_context, reporter, res)
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(Box<StreamEstablishError>),
    #[error("Hyper error: {0}")]
    HyperError(#[from] hyper::Error),
    #[error("No host in HTTP request")]
    HttpNoHost,
    #[error("No port in HTTP request")]
    HttpNoPort,
    #[error("Invalid port in HTTP request: {0}")]
    HttpInvalidPort(String),
    #[error("Invalid HTTP host: {0}")]
    HttpInvalidHost(String),
    #[error("Direct connection error: {0}")]
    Direct(#[source] io::Error),
    #[error("Invalid address: {0}")]
    Address(#[from] ParseInternetAddrError),
    #[error("Upstream HTTP/1 handshake failed: {0}")]
    UpstreamHandshake(#[source] hyper::Error),
    #[error("Upstream HTTP request send failed: {0}")]
    UpstreamRequestSend(#[source] hyper::Error),
    #[error("Upstream background connection failed: {0}")]
    BackgroundConnection(#[source] hyper::Error),
}
impl From<StreamEstablishError> for TunnelError {
    fn from(e: StreamEstablishError) -> Self {
        TunnelError::EstablishProxyChain(Box::new(e))
    }
}
impl TunnelError {
    fn upstream_addr(&self) -> Option<&common::proto::addr::RouteAddr> {
        match self {
            Self::EstablishProxyChain(e) => match e.as_ref() {
                StreamEstablishError::ConnectDestination { upstream_addr, .. }
                | StreamEstablishError::ConnectFirstProxyServer { upstream_addr, .. }
                | StreamEstablishError::WriteHeartbeatUpgrade { upstream_addr, .. }
                | StreamEstablishError::WriteStreamRequestHeader { upstream_addr, .. } => {
                    Some(upstream_addr)
                }
            },
            _ => None,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_context_preserves_authority_when_port_is_missing() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(hyper::header::HOST, "api.example.com")
            .body(())
            .unwrap();
        let context = HttpRequestContext::from_request(&request);
        assert_eq!(context.method, Method::GET);
        assert_eq!(context.uri, "/");
        assert_eq!(context.host.as_deref(), Some("api.example.com"));
        assert_eq!(context.authority.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn connect_error_exposes_attempted_upstream_address() {
        let addr = common::proto::addr::RouteAddr {
            address: "127.0.0.1:8080".parse().unwrap(),
            protocol: "tcp".into(),
        };
        let source = common::stream::pool::ConnectError::ConnectAddr {
            source: io::Error::other("test"),
            addr: addr.clone(),
            sock_addrs: vec![],
        };
        let err = StreamEstablishError::ConnectDestination {
            source,
            upstream_addr: addr.clone(),
        };
        let tunnel_err = TunnelError::EstablishProxyChain(Box::new(err));
        assert!(tunnel_err.upstream_addr().is_some());
        let up = tunnel_err.upstream_addr().unwrap();
        assert_eq!(up.address.to_string(), "127.0.0.1:8080");
        assert_eq!(up.protocol.as_ref(), "tcp");
    }

    #[test]
    fn establish_error_exposes_structured_upstream_address() {
        let addr = common::proto::addr::RouteAddr {
            address: "10.0.0.1:9090".parse().unwrap(),
            protocol: "rtp-mux".into(),
        };
        let source = common::stream::pool::ConnectError::ConnectAddr {
            source: io::Error::other("test"),
            addr: addr.clone(),
            sock_addrs: vec![],
        };
        let err = StreamEstablishError::ConnectFirstProxyServer {
            source,
            upstream_addr: addr.clone(),
        };
        let tunnel_err = TunnelError::from(err);
        let up = tunnel_err.upstream_addr().unwrap();
        assert_eq!(up.protocol.as_ref(), "rtp-mux");
        assert_eq!(up.address.to_string(), "10.0.0.1:9090");
    }
}
