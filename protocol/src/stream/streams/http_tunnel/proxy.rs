use std::{
    fmt,
    net::SocketAddr,
    ops::Deref,
    sync::Arc,
    time::{Instant, SystemTime},
};

use super::authority::get_authority_from_req;
use crate::stream::{
    addr::ConcreteStreamType,
    streams::{
        http_tunnel::{
            HttpAccessConnContext, HttpFailureReporter, HttpResult, TunnelError, redacted_uri,
            respond_with_rejection,
        },
        tcp::listener::TCP_STREAM_TYPE,
    },
};
use common::{
    addr::{InternetAddr, InternetAddrKind},
    log::Timing,
    proto::{
        addr::RouteAddr,
        client::stream::establish,
        log::stream::{LOGGER, StreamLogWithoutByteCounts, StreamProxyLogWithoutByteCounts},
        metrics::stream::StreamSession,
        relay::{DEAD_SESSION_RETENTION_DURATION, same_key_nonce_ciphertext},
    },
    retention::RetentionActorSender,
    route::{ConnSelector, RouteAction},
    session::{SessionSpawner, log_rejection},
    udp::UDP_FLOW_TIMEOUT,
};
use http_body_util::BodyExt;
use hyper::{Request, body::Incoming};
use hyper_util::rt::TokioIo;
use monitor_table::table::RowOwnedGuard;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{instrument, trace};

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

#[instrument(skip_all, fields(method = %req.method()))]
pub async fn dispatch_proxy(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
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
    dispatch(dst_addr_stream, req, method, uri, ctx, reporter).await
}

#[instrument(skip_all, fields(addr = ?dst_addr))]
async fn dispatch(
    dst_addr: RouteAddr,
    req: Request<Incoming>,
    method: String,
    uri: String,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> HttpResult {
    let action = ctx.route_table.action(&dst_addr.address);
    match action {
        RouteAction::ConnSelector(conn_selector) => {
            proxy(conn_selector, dst_addr, req, method, uri, ctx, reporter).await
        }
        RouteAction::Block => {
            trace!("Blocked");
            Ok(respond_with_rejection())
        }
        RouteAction::Direct => direct(dst_addr, req, method, uri, ctx, reporter).await,
    }
}

#[instrument(skip_all)]
async fn direct(
    dst_addr: RouteAddr,
    req: Request<Incoming>,
    method: String,
    uri: String,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> HttpResult {
    let start = (std::time::Instant::now(), std::time::SystemTime::now());
    let destination = dst_addr.address.to_string();
    let sock_addrs = dst_addr.address.to_socket_addrs().await.map_err(|e| {
        let err = TunnelError::Direct(e);
        reporter.report(&err, Some(&destination));
        err
    })?;
    let (upstream, upstream_sock_addr) = ctx
        .stream_context
        .connector_table
        .timed_connect_any(TCP_STREAM_TYPE, sock_addrs, UDP_FLOW_TIMEOUT)
        .await
        .map_err(|e| {
            let err = TunnelError::Direct(e);
            reporter.report(&err, Some(&destination));
            err
        })?;
    let dn_remote = reporter.downstream.remote;
    let session_guard = ctx.stream_context.session_table.as_ref().map(|s| {
        s.set_scope_owned(StreamSession {
            start: SystemTime::now(),
            end: None,
            destination: Some(dst_addr.clone()),
            upstream_local: upstream.local_addr().ok(),
            upstream_remote: dst_addr.clone(),
            downstream_local: Arc::clone(&ctx.listen_addr),
            downstream_remote: dn_remote,
            up_gauge: None,
            dn_gauge: None,
        })
    });
    let res = tls_http(
        upstream,
        req,
        session_guard,
        &reporter,
        &ctx.stream_context.session_spawner,
        &ctx.stream_context.retention,
    )
    .await;
    let end = std::time::Instant::now();
    let timing = Timing { start, end };
    let log = HttpProxyLog {
        timing,
        upstream_addr: RouteAddr {
            address: dst_addr.address.clone(),
            protocol: ConcreteStreamType::Tcp.to_string().into(),
        },
        upstream_sock_addr,
        downstream_addr: dn_remote,
        destination: Some(dst_addr.address),
        method,
        uri,
    };
    common::info_println!("HTTP direct: Finished {log}");
    res
}

#[instrument(skip_all)]
async fn proxy(
    conn_selector: &ConnSelector,
    dst_addr: RouteAddr,
    req: Request<Incoming>,
    method: String,
    uri: String,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> HttpResult {
    let start = (std::time::Instant::now(), std::time::SystemTime::now());

    let (chain, payload_crypto) = match conn_selector {
        common::route::ConnSelector::Empty => ([].into(), None),
        common::route::ConnSelector::Some(non_empty_conn_selector) => {
            let proxy_chain = non_empty_conn_selector.choose_chain();
            (
                proxy_chain.chain.clone(),
                proxy_chain.payload_crypto.clone(),
            )
        }
    };
    let upstream = match establish(&chain, dst_addr.clone(), &ctx.stream_context).await {
        Ok(u) => u,
        Err(e) => {
            let tunnel_err = TunnelError::from(e);
            let destination = tunnel_err.upstream_addr().map(|a| a.address.to_string());
            reporter.report(&tunnel_err, destination.as_deref());
            return Err(tunnel_err);
        }
    };
    let upstream_addr = upstream.addr.clone();

    let dn_remote = reporter.downstream.remote;
    let session_guard = ctx.stream_context.session_table.as_ref().map(|s| {
        s.set_scope_owned(StreamSession {
            start: SystemTime::now(),
            end: None,
            destination: Some(dst_addr.clone()),
            upstream_local: upstream.stream.local_addr().ok(),
            upstream_remote: upstream.addr.clone(),
            downstream_local: Arc::clone(&ctx.listen_addr),
            downstream_remote: dn_remote,
            up_gauge: None,
            dn_gauge: None,
        })
    });
    let res = match &payload_crypto {
        Some(crypto) => {
            let (r, w) = tokio::io::split(upstream.stream);
            let (r, w) = same_key_nonce_ciphertext(crypto.key(), r, w);
            let upstream = tokio_chacha20::stream::DuplexStream::new(r, w);
            tls_http(
                upstream,
                req,
                session_guard,
                &reporter,
                &ctx.stream_context.session_spawner,
                &ctx.stream_context.retention,
            )
            .await
        }
        None => {
            tls_http(
                upstream.stream,
                req,
                session_guard,
                &reporter,
                &ctx.stream_context.session_spawner,
                &ctx.stream_context.retention,
            )
            .await
        }
    };

    let end = std::time::Instant::now();
    let timing = Timing { start, end };
    let log = HttpProxyLog {
        timing: timing.clone(),
        upstream_addr: upstream_addr.clone(),
        upstream_sock_addr: upstream.sock_addr,
        downstream_addr: reporter.downstream.remote,
        destination: Some(dst_addr.address.clone()),
        method,
        uri,
    };
    common::info_println!("HTTP proxy: Finished {log}");

    let record = (&StreamProxyLogWithoutByteCounts {
        stream: StreamLogWithoutByteCounts {
            timing: timing.clone(),
            upstream_addr,
            upstream_sock_addr: upstream.sock_addr,
            downstream_addr: reporter.downstream.remote,
        },
        destination: dst_addr.address,
    })
        .into();
    if let Some(x) = LOGGER.lock().unwrap().as_ref() {
        x.write(&record);
    }

    res
}

#[instrument(skip_all)]
async fn tls_http<Upstream>(
    upstream: Upstream,
    req: Request<Incoming>,
    session_guard: Option<RowOwnedGuard<StreamSession>>,
    reporter: &HttpFailureReporter,
    session_spawner: &SessionSpawner,
    retention: &RetentionActorSender,
) -> HttpResult
where
    Upstream: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
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

    let resp = sender.send_request(req).await.map_err(|e| {
        let err = TunnelError::UpstreamRequestSend(e);
        reporter.report(&err, None);
        err
    })?;

    if let Some(s) = &session_guard {
        s.inspect_mut(|session| session.end = Some(SystemTime::now()));
    }
    retention
        .retain(
            Box::new(session_guard),
            Instant::now() + DEAD_SESSION_RETENTION_DURATION,
        )
        .await;

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
