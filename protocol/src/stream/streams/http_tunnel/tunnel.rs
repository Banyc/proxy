use std::{fmt, sync::Arc};

use crate::stream::{
    addr::ConcreteStreamType,
    streams::{
        http_tunnel::{
            HttpAccessConnContext, HttpFailureReporter, HttpRequestFailure, ReturnType, full,
            respond_with_rejection,
        },
        tcp::proxy_server::TCP_STREAM_TYPE,
    },
};
use async_speed_limit::Limiter;
use bytes::Bytes;
use common::{
    addr::InternetAddr,
    proto::{
        addr::StreamAddr,
        client::stream::{StreamEstablishError, establish},
        connect::stream::StreamConnectorTable,
        context::StreamContext,
        io_copy::stream::{ConnContext, CopyBidirectional},
        log::stream::IoCopyFinished,
        metrics::stream::StreamSessionTable,
        route::StreamRouteGroup,
    },
    route::RouteAction,
    udp::TIMEOUT,
};
use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::{Request, Response, body::Incoming, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tracing::{info, instrument, trace, warn};

use super::TunnelError;

pub struct HttpTunnelLog {
    pub io: IoCopyFinished,
    pub method: String,
    pub uri: String,
}

impl fmt::Display for HttpTunnelLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.io)?;
        if self.method != "CONNECT" {
            write!(f, ",method:{}", self.method)?;
            write!(f, ",uri:{}", self.uri)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum ConnectFailure {
    Upgrade(hyper::Error),
    Resolution(std::io::Error),
    DirectConnect(std::io::Error),
    EstablishProxyChain(StreamEstablishError),
}

#[instrument(skip_all)]
pub async fn run_tunnel_mode(
    ctx: &HttpAccessConnContext,
    req: Request<Incoming>,
    reporter: HttpFailureReporter,
) -> ReturnType {
    let addr = match host_addr(req.uri()) {
        Some(addr) => addr,
        None => {
            let uri = req.uri();
            warn!(%uri, "CONNECT host is not socket addr");
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = hyper::http::StatusCode::BAD_REQUEST;
            return Ok(resp);
        }
    };
    let dst_addr: InternetAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            let err = TunnelError::Address(e);
            reporter.report(&err, None);
            return Err(err);
        }
    };
    dispatch(dst_addr, req, ctx, reporter).await
}

#[instrument(skip_all, fields(addr = ?dst_addr))]
async fn dispatch(
    dst_addr: InternetAddr,
    req: Request<Incoming>,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> ReturnType {
    let method = req.method().to_string();
    let uri = req.uri().to_string();
    let action = ctx.route_table.action(&dst_addr);
    let action = match &action {
        RouteAction::ConnSelector(conn_selector) => {
            let proxy_ctx = ProxyContext {
                conn_selector: Arc::clone(conn_selector),
                speed_limiter: ctx.speed_limiter.clone(),
                stream_context: ctx.stream_context.clone(),
                listen_addr: Arc::clone(&ctx.listen_addr),
                dst_addr: dst_addr.clone(),
                failure: reporter.failure.clone(),
                downstream: reporter.downstream.clone(),
                listener: Arc::clone(&reporter.listener),
                method: method.clone(),
                uri: uri.clone(),
            };
            UpgradeAction::Proxy(proxy_ctx)
        }
        RouteAction::Block => {
            trace!(addr = ?dst_addr, "Blocked CONNECT");
            return Ok(respond_with_rejection());
        }
        RouteAction::Direct => {
            let direct_ctx = DirectContext {
                speed_limiter: ctx.speed_limiter.clone(),
                session_table: ctx.stream_context.session_table.clone(),
                listen_addr: Arc::clone(&ctx.listen_addr),
                connector_table: ctx.stream_context.connector_table.clone(),
                dst_addr: dst_addr.clone(),
                failure: reporter.failure.clone(),
                downstream: reporter.downstream.clone(),
                listener: Arc::clone(&reporter.listener),
                method: method.clone(),
                uri: uri.clone(),
            };
            UpgradeAction::Direct(direct_ctx)
        }
    };
    let (reporter_failure, reporter_downstream, reporter_listener, reporter_dst_addr) =
        match &action {
            UpgradeAction::Proxy(ctx) => (
                ctx.failure.clone(),
                ctx.downstream.clone(),
                Arc::clone(&ctx.listener),
                dst_addr.clone(),
            ),
            UpgradeAction::Direct(ctx) => (
                ctx.failure.clone(),
                ctx.downstream.clone(),
                Arc::clone(&ctx.listener),
                dst_addr.clone(),
            ),
        };
    tokio::task::spawn(async move {
        if let Err(failure) = upgrade(req, action).await {
            let tunnel_err: TunnelError = match failure {
                ConnectFailure::Upgrade(e) => TunnelError::HyperError(e),
                ConnectFailure::Resolution(e) | ConnectFailure::DirectConnect(e) => {
                    TunnelError::Direct(e)
                }
                ConnectFailure::EstablishProxyChain(e) => {
                    TunnelError::EstablishProxyChain(Box::new(e))
                }
            };
            let reporter = HttpFailureReporter {
                failure: reporter_failure,
                downstream: reporter_downstream,
                listener: reporter_listener,
            };
            reporter.report(&tunnel_err, Some(&reporter_dst_addr.to_string()));
        }
    });

    Ok(Response::new(empty()))
}

#[derive(Debug)]
enum UpgradeAction {
    Proxy(ProxyContext),
    Direct(DirectContext),
}
#[instrument(skip_all)]
async fn upgrade(req: Request<Incoming>, action: UpgradeAction) -> Result<(), ConnectFailure> {
    let upgraded = hyper::upgrade::on(req)
        .await
        .map_err(ConnectFailure::Upgrade)?;
    match action {
        UpgradeAction::Direct(ctx) => {
            direct(ctx, upgraded).await?;
        }
        UpgradeAction::Proxy(ctx) => {
            proxy(&ctx, upgraded).await.map_err(|e| match e {
                ProxyError::EstablishProxyChain(inner) => {
                    ConnectFailure::EstablishProxyChain(inner)
                }
            })?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct DirectContext {
    pub speed_limiter: Limiter,
    pub session_table: Option<StreamSessionTable>,
    pub listen_addr: Arc<str>,
    pub connector_table: Arc<StreamConnectorTable>,
    pub dst_addr: InternetAddr,
    pub failure: Arc<HttpRequestFailure>,
    pub downstream: super::HttpDownstreamContext,
    pub listener: Arc<str>,
    pub method: String,
    pub uri: String,
}
#[instrument(skip_all)]
async fn direct(ctx: DirectContext, upgraded: Upgraded) -> Result<(), ConnectFailure> {
    let dst_sock_addrs = ctx
        .dst_addr
        .to_socket_addrs()
        .await
        .map_err(ConnectFailure::Resolution)?;
    let (upstream, dst_sock_addr) = ctx
        .connector_table
        .timed_connect_2(TCP_STREAM_TYPE, dst_sock_addrs.iter().copied(), TIMEOUT)
        .await
        .map_err(ConnectFailure::DirectConnect)?;
    let upstream_addr = StreamAddr {
        stream_type: ConcreteStreamType::Tcp.to_string().into(),
        address: ctx.dst_addr,
    };
    let conn_context = ConnContext {
        start: (std::time::Instant::now(), std::time::SystemTime::now()),
        upstream_remote: upstream_addr.clone(),
        upstream_remote_sock: dst_sock_addr,
        upstream_local: upstream.local_addr().ok(),
        downstream_remote: ctx.downstream.remote,
        downstream_local: ctx.listen_addr,
        session_table: ctx.session_table,
        destination: Some(upstream_addr),
    };
    let (io, res) = CopyBidirectional {
        downstream: TokioIo::new(upgraded),
        upstream,
        payload_crypto: None,
        speed_limiter: ctx.speed_limiter,
        conn_context,
    }
    .serve_as_access_server()
    .await;
    let log = HttpTunnelLog { io, method: ctx.method, uri: ctx.uri };
    match &res {
        Ok(()) => info!(e = %log, "HTTP CONNECT direct: Finished"),
        Err(err) => info!(e = %log, ?err, "HTTP CONNECT direct: Error"),
    }
    Ok(())
}

#[derive(Debug)]
struct ProxyContext {
    pub conn_selector: Arc<StreamRouteGroup>,
    pub speed_limiter: Limiter,
    pub stream_context: StreamContext,
    pub listen_addr: Arc<str>,
    pub dst_addr: InternetAddr,
    pub failure: Arc<HttpRequestFailure>,
    pub downstream: super::HttpDownstreamContext,
    pub listener: Arc<str>,
    pub method: String,
    pub uri: String,
}
#[instrument(skip_all, fields(ctx.dst_addr))]
async fn proxy(ctx: &ProxyContext, upgraded: Upgraded) -> Result<(), ProxyError> {
    let destination = StreamAddr {
        address: ctx.dst_addr.clone(),
        stream_type: ConcreteStreamType::Tcp.to_string().into(),
    };
    let (chain, payload_crypto) = match &ctx.conn_selector.as_ref() {
        common::route::ConnSelector::Empty => ([].into(), None),
        common::route::ConnSelector::Some(conn_selector1) => {
            let proxy_chain = conn_selector1.choose_chain();
            (
                proxy_chain.chain.clone(),
                proxy_chain.payload_crypto.clone(),
            )
        }
    };
    let upstream = establish(&chain, destination.clone(), &ctx.stream_context).await?;

    let conn_context = ConnContext {
        start: (std::time::Instant::now(), std::time::SystemTime::now()),
        upstream_remote: upstream.addr,
        upstream_remote_sock: upstream.sock_addr,
        downstream_remote: ctx.downstream.remote,
        downstream_local: Arc::clone(&ctx.listen_addr),
        upstream_local: upstream.stream.local_addr().ok(),
        session_table: ctx.stream_context.session_table.clone(),
        destination: Some(destination),
    };
    let method = ctx.method.clone();
    let uri = ctx.uri.clone();
    let io_copy = CopyBidirectional {
        downstream: TokioIo::new(upgraded),
        upstream: upstream.stream,
        payload_crypto,
        speed_limiter: ctx.speed_limiter.clone(),
        conn_context,
    }
    .serve_as_access_server();
    tokio::spawn(async move {
        let (io, res) = io_copy.await;
        let log = HttpTunnelLog { io, method, uri };
        match &res {
            Ok(()) => info!(e = %log, "HTTP CONNECT: Finished"),
            Err(err) => info!(e = %log, ?err, "HTTP CONNECT: Error"),
        }
    });
    Ok(())
}
#[derive(Debug, Error)]
enum ProxyError {
    #[error("Failed to establish proxy chain")]
    EstablishProxyChain(#[from] StreamEstablishError),
}

fn host_addr(uri: &hyper::http::Uri) -> Option<String> {
    uri.authority().map(|auth| auth.to_string())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}
