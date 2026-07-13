use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};

use crate::stream::streams::{
    http_tunnel::{proxy::run_proxy_mode, tunnel::run_tunnel_mode},
    tcp::proxy_server::TcpServer,
};
use async_speed_limit::Limiter;
use bytes::Bytes;
use common::{
    addr::ParseInternetAddrError,
    config::SharableConfig,
    loading,
    proto::{
        client::stream::StreamEstablishError,
        context::StreamContext,
        route::{StreamRouteTable, StreamRouteTableBuildContext, StreamRouteTableBuilder},
    },
    route::RouteTableBuildError,
    stream::{HasIoAddr, OwnIoStream, StreamServerHandleConn},
};
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::{Method, Request, Response, service::service_fn};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{instrument, trace, warn};

mod proxy;
mod tunnel;

type ReturnType = Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError>;
type RequestErrorContext = Arc<Mutex<Option<HttpRequestContext>>>;

#[derive(Debug, Clone)]
struct HttpRequestContext {
    method: Method,
    uri: hyper::Uri,
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
            .map(|authority| authority.as_str().to_owned())
            .or_else(|| host.clone());
        Self {
            method: req.method().clone(),
            uri: req.uri().clone(),
            host,
            authority,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub route_table: SharableConfig<StreamRouteTableBuilder>,
    pub speed_limit: Option<f64>,
}
impl HttpAccessServerConfig {
    pub fn into_builder(
        self,
        route_tables: &HashMap<Arc<str>, StreamRouteTable>,
        route_tables_cx: StreamRouteTableBuildContext<'_>,
        stream_context: StreamContext,
    ) -> Result<HttpAccessServerBuilder, BuildError> {
        let route_table = match self.route_table {
            SharableConfig::SharingKey(key) => route_tables
                .get(&key)
                .ok_or_else(|| BuildError::ProxyTableKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(route_tables_cx.clone())?,
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
pub enum BuildError {
    #[error("Proxy table key not found: {0}")]
    ProxyTableKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyTable(#[from] RouteTableBuildError),
}

#[derive(Debug, Clone)]
pub struct HttpAccessServerBuilder {
    listen_addr: Arc<str>,
    route_table: StreamRouteTable,
    speed_limit: f64,
    stream_context: StreamContext,
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
    pub route_table: Arc<StreamRouteTable>,
    pub speed_limiter: Limiter,
    pub stream_context: StreamContext,
    pub listen_addr: Arc<str>,
}

#[derive(Debug)]
pub struct HttpAccessConnHandler {
    ctx: HttpAccessConnContext,
}
impl HttpAccessConnHandler {
    pub fn new(
        route_table: StreamRouteTable,
        speed_limit: f64,
        stream_context: StreamContext,
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
        let dn = stream.peer_addr().ok();
        let dn_local = stream.local_addr().ok();
        let request_error_context = RequestErrorContext::default();
        let res = proxy(&self.ctx, stream, Arc::clone(&request_error_context)).await;
        if let Err(e) = res {
            let request = request_error_context.lock().unwrap().take();
            let method = request.as_ref().map(|request| &request.method);
            let uri = request.as_ref().map(|request| &request.uri);
            let host = request.as_ref().and_then(|request| request.host.as_deref());
            let up = http_failure_upstream(&e, request.as_ref());
            warn!(
                event = "http_tunnel_proxy_failed",
                ?e,
                ?dn,
                ?dn_local,
                ?up,
                ?method,
                ?uri,
                ?host,
                listener = %self.ctx.listen_addr,
                "HTTP tunnel proxy failed"
            );
        }
    }
}

async fn proxy<Downstream>(
    ctx: &HttpAccessConnContext,
    downstream: Downstream,
    request_error_context: RequestErrorContext,
) -> Result<(), TunnelError>
where
    Downstream: OwnIoStream,
{
    hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(
            TokioIo::new(downstream),
            service_fn(|req| run_service(ctx, req, Arc::clone(&request_error_context))),
        )
        .with_upgrades()
        .await?;
    Ok(())
}
#[instrument(skip_all)]
async fn run_service(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
    request_error_context: RequestErrorContext,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
    let request_context = HttpRequestContext::from_request(&req);
    trace!(?request_context, "Received request");
    let res = if Method::CONNECT == req.method() {
        run_tunnel_mode(ctx, req).await
    } else {
        run_proxy_mode(ctx, req).await
    };
    if res.is_err() {
        *request_error_context.lock().unwrap() = Some(request_context);
    }
    res
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error("Failed to establish proxy chain: {0}")]
    EstablishProxyChain(#[from] StreamEstablishError),
    #[error("Hyper error: {0}")]
    HyperError(#[from] hyper::Error),
    #[error("No host in HTTP request")]
    HttpNoHost,
    #[error("No port in HTTP request")]
    HttpNoPort,
    #[error("Direct connection error: {0}")]
    Direct(#[source] io::Error),
    #[error("Invalid address: {0}")]
    Address(#[from] ParseInternetAddrError),
}
impl TunnelError {
    fn upstream_addr(&self) -> Option<&common::proto::addr::StreamAddr> {
        match self {
            Self::EstablishProxyChain(e) => match e {
                StreamEstablishError::WriteHeartbeatUpgrade { upstream_addr, .. }
                | StreamEstablishError::WriteStreamRequestHeader { upstream_addr, .. } => {
                    Some(upstream_addr)
                }
                _ => None,
            },
            _ => None,
        }
    }
}

fn http_failure_upstream(
    error: &TunnelError,
    request: Option<&HttpRequestContext>,
) -> Option<String> {
    error
        .upstream_addr()
        .map(ToString::to_string)
        .or_else(|| request.and_then(|request| request.authority.clone()))
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
    fn request_authority_falls_back_for_http_error_without_upstream() {
        let request = Request::builder()
            .method(Method::GET)
            .uri("/")
            .header(hyper::header::HOST, "api.example.com")
            .body(())
            .unwrap();
        let context = HttpRequestContext::from_request(&request);
        assert_eq!(
            http_failure_upstream(&TunnelError::HttpNoPort, Some(&context)).as_deref(),
            Some("api.example.com")
        );
    }
}
