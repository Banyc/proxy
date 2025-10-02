use std::{collections::HashMap, io, sync::Arc};

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
    stream::{OwnIoStream, StreamServerHandleConn},
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
        Stream: OwnIoStream,
    {
        let res = proxy(&self.ctx, stream).await;
        if let Err(e) = res {
            warn!(?e, "Failed to proxy");
        }
    }
}

async fn proxy<Downstream>(
    ctx: &HttpAccessConnContext,
    downstream: Downstream,
) -> Result<(), TunnelError>
where
    Downstream: OwnIoStream,
{
    hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(
            TokioIo::new(downstream),
            service_fn(|req| run_service(ctx, req)),
        )
        .with_upgrades()
        .await?;
    Ok(())
}
#[instrument(skip_all)]
async fn run_service(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
    trace!(?req, "Received request");
    if Method::CONNECT == req.method() {
        run_tunnel_mode(ctx, req).await
    } else {
        run_proxy_mode(ctx, req).await
    }
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
