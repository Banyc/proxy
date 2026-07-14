use std::{sync::Arc, time::SystemTime};

use crate::stream::{
    addr::ConcreteStreamType,
    streams::{
        http_tunnel::{
            HttpAccessConnContext, HttpFailureReporter, ReturnType, TunnelError,
            respond_with_rejection,
        },
        tcp::proxy_server::TCP_STREAM_TYPE,
    },
};
use common::{
    addr::InternetAddr,
    log::Timing,
    proto::{
        addr::StreamAddr,
        client::stream::establish,
        io_copy::{same_key_nonce_ciphertext, stream::DEAD_SESSION_RETENTION_DURATION},
        log::stream::{LOGGER, SimplifiedStreamLog, SimplifiedStreamProxyLog},
        metrics::stream::Session,
    },
    route::{ConnSelector, RouteAction},
    udp::TIMEOUT,
};
use http_body_util::BodyExt;
use hyper::{Request, body::Incoming, http::uri::Authority};
use hyper_util::rt::TokioIo;
use monitor_table::table::RowOwnedGuard;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, instrument, trace};

const DEFAULT_PORT_HTTP: u16 = 80;
const DEFAULT_PORT_HTTPS: u16 = 443;

fn get_authority_from_req<T>(req: &Request<T>) -> Result<InternetAddr, TunnelError> {
    let scheme = req.uri().scheme_str();

    if let Some(auth) = req.uri().authority() {
        return authority_to_internet_addr(auth, scheme);
    }

    let host_header = req
        .headers()
        .get(hyper::header::HOST)
        .ok_or(TunnelError::HttpNoHost)?;
    let host_str = host_header
        .to_str()
        .map_err(|_| TunnelError::HttpInvalidHost("non-ascii host header".into()))?;
    let authority = Authority::try_from(host_str)
        .map_err(|e| TunnelError::HttpInvalidHost(e.to_string()))?;

    authority_to_internet_addr(&authority, scheme)
}

fn authority_to_internet_addr(
    authority: &Authority,
    scheme: Option<&str>,
) -> Result<InternetAddr, TunnelError> {
    let host = authority.host();
    let port = authority.port_u16().or_else(|| match scheme {
        Some("http") => Some(DEFAULT_PORT_HTTP),
        Some("https") => Some(DEFAULT_PORT_HTTPS),
        None => Some(DEFAULT_PORT_HTTP),
        _ => None,
    });
    let port = port.ok_or(TunnelError::HttpNoPort)?;
    Ok(InternetAddr::from_host_and_port(host, port)?)
}

#[cfg(test)]
pub(crate) fn get_authority_from_req_for_test<T>(
    req: &Request<T>,
) -> Result<InternetAddr, TunnelError> {
    get_authority_from_req(req)
}

#[instrument(skip_all, fields(method = %req.method()))]
pub async fn run_proxy_mode(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
    reporter: HttpFailureReporter,
) -> ReturnType {
    let dst_addr = match get_authority_from_req(&req) {
        Ok(addr) => addr,
        Err(e) => {
            reporter.report(&e, None);
            return Err(e);
        }
    };
    let dst_addr_stream = StreamAddr {
        address: dst_addr,
        stream_type: ConcreteStreamType::Tcp.to_string().into(),
    };
    let req = req_modify_path(req);
    dispatch(dst_addr_stream, req, ctx, reporter).await
}

#[instrument(skip_all, fields(addr = ?dst_addr))]
async fn dispatch(
    dst_addr: StreamAddr,
    req: Request<Incoming>,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> ReturnType {
    let action = ctx.route_table.action(&dst_addr.address);
    match action {
        RouteAction::ConnSelector(conn_selector) => {
            proxy(conn_selector, dst_addr, req, ctx, reporter).await
        }
        RouteAction::Block => {
            trace!("Blocked");
            Ok(respond_with_rejection())
        }
        RouteAction::Direct => direct(dst_addr, req, ctx, reporter).await,
    }
}

#[instrument(skip_all)]
async fn direct(
    dst_addr: StreamAddr,
    req: Request<Incoming>,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> ReturnType {
    let sock_addrs = dst_addr
        .address
        .to_socket_addrs()
        .await
        .map_err(TunnelError::Direct)?;
    let (upstream, _upstream_sock) = ctx
        .stream_context
        .connector_table
        .timed_connect_2(TCP_STREAM_TYPE, sock_addrs, TIMEOUT)
        .await
        .map_err(TunnelError::Direct)?;
    let dn_remote = reporter.downstream.remote;
    let session_guard = ctx.stream_context.session_table.as_ref().map(|s| {
        s.set_scope_owned(Session {
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
    let res = tls_http(upstream, req, session_guard, &reporter).await;
    info!("Direct finished");
    res
}

#[instrument(skip_all)]
async fn proxy(
    conn_selector: &ConnSelector<StreamAddr>,
    dst_addr: StreamAddr,
    req: Request<Incoming>,
    ctx: &HttpAccessConnContext,
    reporter: HttpFailureReporter,
) -> ReturnType {
    let start = (std::time::Instant::now(), std::time::SystemTime::now());

    let (chain, payload_crypto) = match conn_selector {
        common::route::ConnSelector::Empty => ([].into(), None),
        common::route::ConnSelector::Some(conn_selector1) => {
            let proxy_chain = conn_selector1.choose_chain();
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
            let destination = tunnel_err
                .upstream_addr()
                .map(|a| a.address.to_string());
            reporter.report(&tunnel_err, destination.as_deref());
            return Err(tunnel_err);
        }
    };
    let upstream_addr = upstream.addr.clone();

    let dn_remote = reporter.downstream.remote;
    let session_guard = ctx.stream_context.session_table.as_ref().map(|s| {
        s.set_scope_owned(Session {
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
            tls_http(upstream, req, session_guard, &reporter).await
        }
        None => tls_http(upstream.stream, req, session_guard, &reporter).await,
    };

    let end = std::time::Instant::now();
    let timing = Timing { start, end };
    let log = SimplifiedStreamProxyLog {
        stream: SimplifiedStreamLog {
            timing,
            upstream_addr,
            upstream_sock_addr: upstream.sock_addr,
            downstream_addr: reporter.downstream.remote,
        },
        destination: dst_addr.address,
    };
    info!(%log, "Finished");

    let record = (&log).into();
    if let Some(x) = LOGGER.lock().unwrap().as_ref() {
        x.write(&record);
    }

    res
}

#[instrument(skip_all)]
async fn tls_http<Upstream>(
    upstream: Upstream,
    req: Request<Incoming>,
    session_guard: Option<RowOwnedGuard<Session>>,
    reporter: &HttpFailureReporter,
) -> ReturnType
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

    let bg_reporter_failure = Arc::clone(&reporter.failure);
    let bg_downstream = reporter.downstream.clone();
    let bg_listener = Arc::clone(&reporter.listener);
    tokio::task::spawn(async move {
        if let Err(e) = conn.await {
            let err = TunnelError::BackgroundConnection(e);
            let bg_reporter = HttpFailureReporter {
                failure: bg_reporter_failure,
                downstream: bg_downstream,
                listener: bg_listener,
            };
            bg_reporter.report(&err, None);
        }
    });

    let resp = sender.send_request(req).await.map_err(|e| {
        let err = TunnelError::UpstreamRequestSend(e);
        reporter.report(&err, None);
        err
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
        assert_eq!(headers.get(hyper::http::header::HOST).unwrap(), "www.example.org");
        assert_eq!(uri.to_string(), "/pub/WWW/TheProject.html");

        super::transform_absolute_form_req(&mut uri, &mut headers, &method);
        assert_eq!(headers.get(hyper::http::header::HOST).unwrap(), "www.example.org");
        assert_eq!(uri.to_string(), "/pub/WWW/TheProject.html");
    }
}
