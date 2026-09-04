use std::{
    fmt,
    net::SocketAddr,
    ops::Deref,
    sync::Arc,
    time::{Instant, SystemTime},
};

use super::authority::get_authority_from_req;
use super::upstream;
use crate::stream_proto::{
    addr::ConcreteStreamType,
    streams::http_tunnel::{
        HttpAccessConnContext, HttpFailureReporter, HttpResult, TunnelError, redacted_uri,
        respond_with_rejection,
    },
};
use common::{
    addr::{InternetAddr, InternetAddrKind},
    log::Timing,
    proxy_runtime::{
        addr::RouteAddr,
        log::stream::{LOGGER, StreamLogWithoutByteCounts, StreamProxyLogWithoutByteCounts},
        metrics::stream::StreamSession,
        relay::DEAD_SESSION_RETENTION_DURATION,
        relay::stream::{ConnContext, CopyBidirectional},
    },
    session::log_rejection,
};
use http_body_util::BodyExt;
use hyper::{Request, StatusCode, body::Incoming, upgrade::OnUpgrade};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, instrument, trace};

struct HttpProxyLog {
    timing: Timing,
    upstream_addr: RouteAddr,
    upstream_sock_addr: SocketAddr,
    downstream_addr: Option<SocketAddr>,
    destination: Option<InternetAddr>,
    method: String,
    uri: String,
}

impl fmt::Display for HttpProxyLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let duration = self.timing.duration().as_secs_f64();
        let upstream_addrs = match self.upstream_addr.address.deref() {
            InternetAddrKind::SocketAddr(_) => self.upstream_addr.to_string(),
            InternetAddrKind::DomainName { .. } => {
                format!("{},{}", self.upstream_addr, self.upstream_sock_addr.ip())
            }
        };
        write!(f, "{duration:.1}s,up{{{upstream_addrs}}}")?;
        if let Some(downstream_addr) = self.downstream_addr {
            write!(f, ",dn:{downstream_addr}")?;
        }
        if let Some(destination) = &self.destination {
            write!(f, ",dt:{destination}")?;
        }
        write!(f, ",method:{}", self.method)?;
        write!(f, ",uri:{}", self.uri)?;
        Ok(())
    }
}

/// Per-request context for the HTTP proxy (non-CONNECT) path: the per-request
/// failure reporter and the request's log fields (captured before the
/// request-target is rewritten to origin-form).
struct HttpProxyContext {
    reporter: HttpFailureReporter,
    method: String,
    uri: String,
}

#[instrument(skip_all, fields(method = %req.method()))]
pub async fn dispatch_proxy(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
    downstream_upgrade: Option<OnUpgrade>,
    reporter: HttpFailureReporter,
) -> HttpResult {
    let method = req.method().to_string();
    let uri = redacted_uri(req.uri());
    let dst_addr = match get_authority_from_req(&req) {
        Ok(addr) => addr,
        Err(e) => {
            reporter.report(&e, None);
            return Err(e);
        }
    };
    reporter.set_destination(dst_addr.to_string());
    let dst_addr_stream = RouteAddr {
        address: dst_addr,
        protocol: ConcreteStreamType::Tcp.to_string().into(),
    };
    let req = req_modify_path(req);
    let router = upstream::Router::from_ctx(ctx);
    let proxy_ctx = HttpProxyContext {
        reporter,
        method,
        uri,
    };
    dispatch(router, dst_addr_stream, req, downstream_upgrade, proxy_ctx).await
}

#[instrument(skip_all, fields(addr = ?dst_addr))]
async fn dispatch(
    router: upstream::Router,
    dst_addr: RouteAddr,
    req: Request<Incoming>,
    downstream_upgrade: Option<OnUpgrade>,
    proxy_ctx: HttpProxyContext,
) -> HttpResult {
    let action = router.route_table.action(&dst_addr.address);
    let Some(plan) = upstream::plan(dst_addr.clone(), action) else {
        trace!("Blocked");
        return Ok(respond_with_rejection());
    };
    relay(plan, dst_addr, req, downstream_upgrade, router, proxy_ctx).await
}

#[instrument(skip_all)]
async fn relay(
    plan: upstream::RoutePlan,
    dst_addr: RouteAddr,
    req: Request<Incoming>,
    downstream_upgrade: Option<OnUpgrade>,
    router: upstream::Router,
    proxy_ctx: HttpProxyContext,
) -> HttpResult {
    let upstream = match upstream::connect(plan.clone(), &router).await {
        Ok(upstream) => upstream,
        Err(e) => {
            // The attempted upstream for the failure log: the chain error's
            // own upstream address, or the direct destination.
            let attempted =
                e.upstream_addr()
                    .map(|a| a.address.to_string())
                    .or_else(|| match &plan {
                        upstream::RoutePlan::Direct(dst_addr) => Some(dst_addr.address.to_string()),
                        upstream::RoutePlan::Chain { .. } => None,
                    });
            proxy_ctx.reporter.report(&e, attempted.as_deref());
            return Err(e);
        }
    };
    let dn_remote = proxy_ctx.reporter.downstream.remote;
    let conn_context = upstream::conn_context(&upstream, dst_addr.clone(), dn_remote, &router);
    let res = tls_http(
        upstream.stream,
        req,
        conn_context,
        downstream_upgrade,
        &router,
        &proxy_ctx,
    )
    .await;
    let end = std::time::Instant::now();
    let timing = Timing {
        start: upstream.start,
        end,
    };
    match &plan {
        upstream::RoutePlan::Direct(_) => {
            let log = HttpProxyLog {
                timing,
                upstream_addr: upstream.addr.clone(),
                upstream_sock_addr: upstream.sock_addr,
                downstream_addr: dn_remote,
                destination: Some(dst_addr.address.clone()),
                method: proxy_ctx.method,
                uri: proxy_ctx.uri,
            };
            info!("HTTP direct: Finished {log}");
        }
        upstream::RoutePlan::Chain { .. } => {
            let log = HttpProxyLog {
                timing: timing.clone(),
                upstream_addr: upstream.addr.clone(),
                upstream_sock_addr: upstream.sock_addr,
                downstream_addr: proxy_ctx.reporter.downstream.remote,
                destination: Some(dst_addr.address.clone()),
                method: proxy_ctx.method,
                uri: proxy_ctx.uri,
            };
            info!("HTTP proxy: Finished {log}");
            let record = (&StreamProxyLogWithoutByteCounts {
                stream: StreamLogWithoutByteCounts {
                    timing: timing.clone(),
                    upstream_addr: upstream.addr.clone(),
                    upstream_sock_addr: upstream.sock_addr,
                    downstream_addr: proxy_ctx.reporter.downstream.remote,
                },
                destination: dst_addr.address,
            })
                .into();
            if let Some(x) = LOGGER.lock().unwrap().as_ref() {
                x.write(&record);
            }
        }
    }
    res
}

#[instrument(skip_all)]
async fn tls_http<Upstream>(
    upstream: Upstream,
    req: Request<Incoming>,
    conn_context: ConnContext,
    downstream_upgrade: Option<OnUpgrade>,
    router: &upstream::Router,
    proxy_ctx: &HttpProxyContext,
) -> HttpResult
where
    Upstream: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
    let speed_limiter = router.speed_limiter.clone();
    let reporter = proxy_ctx.reporter.clone();
    let session_spawner = router.stream.session_spawner.clone();
    let retention = router.stream.retention.clone();

    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(TokioIo::new(upstream))
        .await
        .map_err(|e| {
            let err = TunnelError::UpstreamHandshake(e);
            reporter.report(&err, None);
            err
        })?;
    let conn = conn.with_upgrades();

    let bg_reporter = reporter.clone();
    let session_spawner = session_spawner.clone();
    if let Err(error) = session_spawner
        .spawn(async move {
            if let Err(e) = conn.await {
                let err = TunnelError::BackgroundConnection(e);
                bg_reporter.report(&err, None);
            }
            Ok(())
        })
        .await
    {
        log_rejection("http_connection", error);
    }

    let mut resp = sender.send_request(req).await.map_err(|e| {
        let err = TunnelError::UpstreamRequestSend(e);
        reporter.report(&err, None);
        err
    })?;

    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        // The origin accepted an upgrade (e.g. WebSocket). Hand the upgraded
        // connection to the client and tunnel bytes in both directions.
        if let Some(downstream_upgrade) = downstream_upgrade {
            let upstream_upgrade = hyper::upgrade::on(&mut resp);
            let reporter = reporter.clone();
            let retention = retention.clone();
            if let Err(error) = session_spawner
                .spawn(async move {
                    let (downstream, upstream) = tokio::join!(downstream_upgrade, upstream_upgrade);
                    match (downstream, upstream) {
                        (Ok(downstream), Ok(upstream)) => {
                            let (io, res) = CopyBidirectional {
                                downstream: TokioIo::new(downstream),
                                upstream: TokioIo::new(upstream),
                                payload_crypto: None,
                                speed_limiter,
                                conn_context,
                                retention,
                            }
                            .serve_as_access_server()
                            .await;
                            match &res {
                                Ok(()) => info!("HTTP upgrade: Finished {io}"),
                                Err(err) => {
                                    info!("HTTP upgrade: Error {io}: {err}")
                                }
                            }
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            let err = TunnelError::HyperError(e);
                            reporter.report(&err, None);
                        }
                    }
                    Ok(())
                })
                .await
            {
                log_rejection("http_upgrade", error);
            }
        }
    } else {
        let session_guard = conn_context.session_table.as_ref().map(|s| {
            s.set_scope_owned(StreamSession {
                start: conn_context.start.1,
                end: None,
                destination: conn_context.destination.clone(),
                upstream_local: conn_context.upstream_local,
                upstream_remote: conn_context.upstream_remote.clone(),
                downstream_local: Arc::clone(&conn_context.downstream_local),
                downstream_remote: conn_context.downstream_remote,
                up_gauge: None,
                dn_gauge: None,
            })
        });
        if let Some(s) = &session_guard {
            s.inspect_mut(|session| session.end = Some(SystemTime::now()));
        }
        retention
            .retain(
                Box::new(session_guard),
                Instant::now() + DEAD_SESSION_RETENTION_DURATION,
            )
            .await;
    }

    Ok(resp.map(|b| b.boxed()))
}

fn req_modify_path<T>(req: Request<T>) -> Request<T> {
    let mut req = req;
    let mut uri = core::mem::take(req.uri_mut());
    let mut headers = core::mem::take(req.headers_mut());
    transform_absolute_form_req(&mut uri, &mut headers, req.method());
    *req.uri_mut() = uri;
    *req.headers_mut() = headers;
    req
}
fn transform_absolute_form_req(
    uri: &mut hyper::http::Uri,
    headers: &mut hyper::http::HeaderMap,
    method: &hyper::http::Method,
) {
    let Some(auth) = uri.authority() else {
        return;
    };
    if uri.scheme().is_none() {
        return;
    }

    let host = auth.host();
    let new_host_value: std::borrow::Cow<str> = match auth.port() {
        Some(port) => format!("{host}:{port}").into(),
        None => host.into(),
    };
    let new_host_value = new_host_value.parse().unwrap();
    headers.insert(hyper::http::header::HOST, new_host_value);

    let default_origin_form = if method == hyper::http::Method::OPTIONS {
        "*"
    } else {
        "/"
    };
    let relative_ref = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(default_origin_form);
    *uri = relative_ref.parse().unwrap();
}
#[cfg(test)]
mod address_tests {
    #[test]
    fn test_transform_absolute_form_req() {
        let absolute_form: hyper::http::Uri = "http://www.example.org/pub/WWW/TheProject.html"
            .parse()
            .unwrap();
        let mut uri = absolute_form;
        let mut headers = hyper::http::HeaderMap::new();
        let method = hyper::http::Method::GET;

        super::transform_absolute_form_req(&mut uri, &mut headers, &method);
        assert_eq!(
            headers.get(hyper::http::header::HOST).unwrap(),
            "www.example.org"
        );
        assert_eq!(uri.to_string(), "/pub/WWW/TheProject.html");

        super::transform_absolute_form_req(&mut uri, &mut headers, &method);
        assert_eq!(
            headers.get(hyper::http::header::HOST).unwrap(),
            "www.example.org"
        );
        assert_eq!(uri.to_string(), "/pub/WWW/TheProject.html");
    }
}
