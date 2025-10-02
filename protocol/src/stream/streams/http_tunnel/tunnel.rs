use std::sync::Arc;

use crate::stream::{
    addr::ConcreteStreamType,
    streams::{
        http_tunnel::{HttpAccessConnContext, TunnelError, full, respond_with_rejection},
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
        metrics::stream::StreamSessionTable,
        route::StreamRouteGroup,
    },
    route::RouteAction,
    udp::TIMEOUT,
};
use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::{Request, Response, body::Incoming, http, upgrade::Upgraded};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tracing::{instrument, trace, warn};

#[instrument(skip_all, fields(dst_addr))]
pub async fn run_tunnel_mode(
    ctx: &HttpAccessConnContext,
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
    let dst_addr: InternetAddr = addr.parse()?;
    let action = ctx.route_table.action(&dst_addr);
    let action = match &action {
        RouteAction::ConnSelector(conn_selector) => {
            let ctx = ProxyContext {
                conn_selector: Arc::clone(conn_selector),
                speed_limiter: ctx.speed_limiter.clone(),
                stream_context: ctx.stream_context.clone(),
                listen_addr: Arc::clone(&ctx.listen_addr),
                dst_addr: dst_addr.clone(),
            };
            UpgradeAction::Proxy(ctx)
        }
        RouteAction::Block => {
            trace!(addr = ?dst_addr, "Blocked CONNECT");
            return Ok(respond_with_rejection());
        }
        RouteAction::Direct => {
            let ctx = DirectContext {
                speed_limiter: ctx.speed_limiter.clone(),
                session_table: ctx.stream_context.session_table.clone(),
                listen_addr: Arc::clone(&ctx.listen_addr),
                connector_table: ctx.stream_context.connector_table.clone(),
                dst_addr: dst_addr.clone(),
            };
            UpgradeAction::Direct(ctx)
        }
    };
    tokio::task::spawn(async move {
        upgrade(req, action, dst_addr).await;
    });

    // Return STATUS_OK
    Ok(Response::new(empty()))
}

#[derive(Debug)]
enum UpgradeAction {
    Proxy(ProxyContext),
    Direct(DirectContext),
}
#[instrument(skip_all, fields(addr = ?_dst_addr))]
async fn upgrade(req: Request<Incoming>, action: UpgradeAction, _dst_addr: InternetAddr) {
    let upgraded = match hyper::upgrade::on(req).await {
        Ok(upgraded) => upgraded,
        Err(e) => {
            warn!(?e, "Upgrade error");
            return;
        }
    };
    match action {
        UpgradeAction::Direct(ctx) => {
            direct(ctx, upgraded).await;
        }
        UpgradeAction::Proxy(ctx) => {
            match proxy(&ctx, upgraded).await {
                Ok(()) => (),
                Err(e) => warn!(?e, "CONNECT error"),
            };
        }
    }
}

#[derive(Debug)]
struct DirectContext {
    pub speed_limiter: Limiter,
    pub session_table: Option<StreamSessionTable>,
    pub listen_addr: Arc<str>,
    pub connector_table: Arc<StreamConnectorTable>,
    pub dst_addr: InternetAddr,
}
#[instrument(skip_all)]
async fn direct(ctx: DirectContext, upgraded: Upgraded) {
    let dst_sock_addr = match ctx.dst_addr.to_socket_addr().await {
        Ok(sock_addr) => sock_addr,
        Err(e) => {
            warn!(?e, "Failed to resolve address");
            return;
        }
    };
    let upstream = match ctx
        .connector_table
        .timed_connect(TCP_STREAM_TYPE, dst_sock_addr, TIMEOUT)
        .await
    {
        Ok(upstream) => upstream,
        Err(e) => {
            warn!(?e, ?dst_sock_addr, "Failed to connect to upstream directly");
            return;
        }
    };
    let upstream_addr = StreamAddr {
        stream_type: ConcreteStreamType::Tcp.to_string().into(),
        address: ctx.dst_addr,
    };
    let conn_context = ConnContext {
        start: (std::time::Instant::now(), std::time::SystemTime::now()),
        upstream_remote: upstream_addr.clone(),
        upstream_remote_sock: dst_sock_addr,
        upstream_local: upstream.local_addr().ok(),
        downstream_remote: None,
        downstream_local: ctx.listen_addr,
        session_table: ctx.session_table,
        destination: Some(upstream_addr),
    };
    let _ = CopyBidirectional {
        downstream: TokioIo::new(upgraded),
        upstream,
        payload_crypto: None,
        speed_limiter: ctx.speed_limiter,
        conn_context,
    }
    .serve_as_access_server("HTTP CONNECT direct")
    .await;
}

#[derive(Debug)]
struct ProxyContext {
    pub conn_selector: Arc<StreamRouteGroup>,
    pub speed_limiter: Limiter,
    pub stream_context: StreamContext,
    pub listen_addr: Arc<str>,
    pub dst_addr: InternetAddr,
}
// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
#[instrument(skip_all, fields(ctx.dst_addr))]
async fn proxy(ctx: &ProxyContext, upgraded: Upgraded) -> Result<(), ProxyError> {
    // Establish proxy chain
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
        downstream_remote: None,
        downstream_local: Arc::clone(&ctx.listen_addr),
        upstream_local: upstream.stream.local_addr().ok(),
        session_table: ctx.stream_context.session_table.clone(),
        destination: Some(destination),
    };
    let io_copy = CopyBidirectional {
        downstream: TokioIo::new(upgraded),
        upstream: upstream.stream,
        payload_crypto,
        speed_limiter: ctx.speed_limiter.clone(),
        conn_context,
    }
    .serve_as_access_server("HTTP CONNECT");
    tokio::spawn(async move {
        let _ = io_copy.await;
    });
    Ok(())
}
#[derive(Debug, Error)]
enum ProxyError {
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
