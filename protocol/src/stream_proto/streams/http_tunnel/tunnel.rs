use std::{fmt, net::SocketAddr};

use super::upstream;
use crate::stream_proto::{
    addr::ConcreteStreamType,
    streams::http_tunnel::{
        HttpAccessConnContext, HttpFailureReporter, HttpResult, TunnelError, full, host_and_port,
        redacted_uri, respond_with_rejection,
    },
};
use bytes::Bytes;
use common::{
    addr::InternetAddr,
    proxy_runtime::{
        addr::RouteAddr, log::stream::IoCopyFinished, relay::stream::CopyBidirectional,
    },
    session::log_rejection,
};
use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::{Request, Response, body::Incoming};
use hyper_util::rt::TokioIo;
use tracing::{instrument, trace, warn};

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

#[instrument(skip_all)]
pub async fn dispatch_tunnel(
    ctx: &HttpAccessConnContext,
    req: Request<Incoming>,
    reporter: HttpFailureReporter,
) -> HttpResult {
    let addr = match host_addr(req.uri()) {
        Some(addr) => addr,
        None => {
            let uri = redacted_uri(req.uri());
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
) -> HttpResult {
    let method = req.method().to_string();
    let uri = redacted_uri(req.uri());
    let dst_route = RouteAddr {
        address: dst_addr.clone(),
        protocol: ConcreteStreamType::Tcp.to_string().into(),
    };
    let action = ctx.route_table.action(&dst_addr);
    let Some(plan) = upstream::plan(dst_route.clone(), action) else {
        trace!(addr = ?dst_addr, "Blocked CONNECT");
        return Ok(respond_with_rejection());
    };
    let router = upstream::Router::from_ctx(ctx);
    let reporter_dst_addr = dst_addr;
    let downstream_remote = reporter.downstream.remote;
    let session_spawner = ctx.stream_context.session_spawner.clone();
    if let Err(error) = session_spawner
        .spawn(async move {
            if let Err(tunnel_err) =
                upgrade(req, plan, router, dst_route, downstream_remote, method, uri).await
            {
                reporter.report(&tunnel_err, Some(&reporter_dst_addr.to_string()));
            }
            Ok(())
        })
        .await
    {
        log_rejection("http_upgrade", error);
    }
    Ok(Response::new(empty()))
}

#[instrument(skip_all)]
async fn upgrade(
    req: Request<Incoming>,
    plan: upstream::RoutePlan,
    router: upstream::Router,
    destination: RouteAddr,
    downstream_remote: Option<SocketAddr>,
    method: String,
    uri: String,
) -> Result<(), TunnelError> {
    let upgraded = hyper::upgrade::on(req)
        .await
        .map_err(TunnelError::HyperError)?;
    let upstream = upstream::connect(plan.clone(), &router).await?;
    let conn_context = upstream::conn_context(&upstream, destination, downstream_remote, &router);
    let retention = router.stream.retention.clone();
    let (io, res) = CopyBidirectional {
        downstream: TokioIo::new(upgraded),
        upstream: upstream.stream,
        payload_crypto: None,
        speed_limiter: router.speed_limiter,
        conn_context,
        retention,
    }
    .serve_as_access_server()
    .await;
    let log = HttpTunnelLog { io, method, uri };
    let tag = match &plan {
        upstream::RoutePlan::Direct(_) => "HTTP CONNECT direct",
        upstream::RoutePlan::Chain { .. } => "HTTP CONNECT",
    };
    match &res {
        Ok(()) => common::info_println!("{tag}: Finished {log}"),
        Err(err) => common::info_println!("{tag}: Error {log}: {err}"),
    }
    Ok(())
}

fn host_addr(uri: &hyper::http::Uri) -> Option<String> {
    uri.authority().map(|auth| host_and_port(auth).to_owned())
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}
