//! Shared upstream connection establishment for the CONNECT and non-CONNECT
//! HTTP proxy paths: resolving a routed destination into a connection plan
//! and establishing the upstream stream, plus the per-request router and
//! the relay `ConnContext`.

use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Instant, SystemTime},
};

use async_speed_limit::Limiter;
use common::{
    proxy_runtime::{
        addr::RouteAddr, client::stream::establish, context::StreamRuntime,
        relay::stream::ConnContext,
    },
    route::{RouteAction, RouteSelector, RouteTable},
    stream_runtime::IoConnection,
    udp_runtime::UDP_FLOW_TIMEOUT,
};

use crate::stream_proto::streams::{
    http_tunnel::{HttpAccessConnContext, TunnelError},
    tcp::listener::TCP_STREAM_TYPE,
};

/// The routed destination, resolved into an establishable connection plan.
#[derive(Clone)]
pub(super) enum RoutePlan {
    Direct(RouteAddr),
    Chain {
        conn_selector: Arc<RouteSelector>,
        destination: RouteAddr,
    },
}

/// The connection plan for a routed destination, or `None` when the route
/// blocks.
pub(super) fn plan(dst_addr: RouteAddr, action: &RouteAction) -> Option<RoutePlan> {
    match action {
        RouteAction::Direct => Some(RoutePlan::Direct(dst_addr)),
        RouteAction::RouteSelector(conn_selector) => Some(RoutePlan::Chain {
            conn_selector: Arc::clone(conn_selector),
            destination: dst_addr,
        }),
        RouteAction::Block => None,
    }
}

/// An established upstream connection: the stream plus the facts needed for
/// the session/relay `ConnContext`.
pub(super) struct Upstream {
    pub stream: Box<dyn IoConnection>,
    pub start: (Instant, SystemTime),
    pub addr: RouteAddr,
    pub sock_addr: SocketAddr,
}

/// Establish the upstream for `plan`. `Err(TunnelError)` on DNS failure,
/// connect failure, or chain-establishment failure.
pub(super) async fn connect(plan: RoutePlan, router: &Router) -> Result<Upstream, TunnelError> {
    let start = (Instant::now(), SystemTime::now());
    match plan {
        RoutePlan::Direct(dst_addr) => {
            let sock_addrs = dst_addr
                .address
                .to_socket_addrs()
                .await
                .map_err(TunnelError::Direct)?;
            let (stream, sock_addr) = router
                .stream
                .connector_table
                .timed_connect_any(TCP_STREAM_TYPE, sock_addrs, None, UDP_FLOW_TIMEOUT)
                .await
                .map_err(TunnelError::Direct)?;
            Ok(Upstream {
                stream,
                start,
                addr: dst_addr,
                sock_addr,
            })
        }
        RoutePlan::Chain {
            conn_selector,
            destination,
        } => {
            let chain = match conn_selector.as_ref() {
                RouteSelector::Empty => [].into(),
                RouteSelector::Some(non_empty) => non_empty.choose_chain().chain.clone(),
            };
            let conn = establish(&chain, destination.clone(), &router.stream)
                .await
                .map_err(TunnelError::from)?;
            Ok(Upstream {
                stream: conn.stream,
                start,
                addr: conn.addr,
                sock_addr: conn.sock_addr,
            })
        }
    }
}

/// Cloneable per-request snapshot of the access-server context: everything a
/// spawned task needs to connect upstream, build the relay context, and copy
/// bytes, without borrowing the connection handler.
#[derive(Clone)]
pub(super) struct Router {
    pub route_table: Arc<RouteTable>,
    pub stream: StreamRuntime,
    pub speed_limiter: Limiter,
    pub listen_addr: Arc<str>,
}

impl Router {
    pub(super) fn from_ctx(ctx: &HttpAccessConnContext) -> Self {
        Self {
            route_table: Arc::clone(&ctx.route_table),
            stream: ctx.stream_context.clone(),
            speed_limiter: ctx.speed_limiter.clone(),
            listen_addr: Arc::clone(&ctx.listen_addr),
        }
    }
}

/// Build the relay `ConnContext` shared by every tunneled path.
pub(super) fn conn_context(
    upstream: &Upstream,
    destination: RouteAddr,
    downstream_remote: Option<SocketAddr>,
    router: &Router,
) -> ConnContext {
    ConnContext {
        start: upstream.start,
        upstream_remote: upstream.addr.clone(),
        upstream_remote_sock: upstream.sock_addr,
        upstream_local: upstream.stream.local_addr().ok(),
        downstream_remote,
        downstream_local: Arc::clone(&router.listen_addr),
        session_table: router.stream.session_table.clone(),
        destination: Some(destination),
    }
}
