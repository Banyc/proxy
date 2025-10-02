use std::{borrow::Cow, sync::Arc, time::SystemTime};

use crate::stream::{
    addr::ConcreteStreamType,
    streams::{
        http_tunnel::{HttpAccessConnContext, TunnelError, respond_with_rejection},
        tcp::proxy_server::TCP_STREAM_TYPE,
    },
};
use bytes::Bytes;
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
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::{Request, Response, body::Incoming, http};
use hyper_util::rt::TokioIo;
use monitor_table::table::RowOwnedGuard;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info, instrument, trace, warn};

#[instrument(skip_all)]
pub async fn run_proxy_mode(
    ctx: &HttpAccessConnContext,
    req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
    let start = (std::time::Instant::now(), std::time::SystemTime::now());

    let dst_addr = {
        let host = req.uri().host().ok_or(TunnelError::HttpNoHost)?;
        let port = req.uri().port_u16().ok_or(TunnelError::HttpNoPort)?;
        let addr = InternetAddr::from_host_and_port(host, port)?;
        StreamAddr {
            address: addr,
            stream_type: ConcreteStreamType::Tcp.to_string().into(),
        }
    };
    let action = ctx.route_table.action(&dst_addr.address);
    match action {
        RouteAction::ConnSelector(conn_selector) => {
            proxy(conn_selector, dst_addr, req, start, ctx).await
        }
        RouteAction::Block => {
            trace!(addr = ?dst_addr, "Blocked {method}", method = req.method());
            return Ok(respond_with_rejection());
        }
        RouteAction::Direct => direct(dst_addr, req, ctx).await,
    }
}

#[instrument(skip_all, fields(addr = ?dst_addr))]
async fn direct(
    dst_addr: StreamAddr,
    req: Request<Incoming>,
    ctx: &HttpAccessConnContext,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
    let sock_addr = dst_addr
        .address
        .to_socket_addr()
        .await
        .map_err(TunnelError::Direct)?;
    let upstream = ctx
        .stream_context
        .connector_table
        .timed_connect(TCP_STREAM_TYPE, sock_addr, TIMEOUT)
        .await
        .map_err(TunnelError::Direct)?;
    let session_guard = ctx.stream_context.session_table.as_ref().map(|s| {
        s.set_scope_owned(Session {
            start: SystemTime::now(),
            end: None,
            destination: Some(dst_addr.clone()),
            upstream_local: upstream.local_addr().ok(),
            upstream_remote: dst_addr.clone(),
            downstream_local: Arc::clone(&ctx.listen_addr),
            downstream_remote: None,
            up_gauge: None,
            dn_gauge: None,
        })
    });
    let req = {
        let mut req = req;
        let mut uri = core::mem::take(req.uri_mut());
        let mut headers = core::mem::take(req.headers_mut());
        transform_absolute_form_req(&mut uri, &mut headers);
        *req.uri_mut() = uri;
        *req.headers_mut() = headers;
        req
    };
    let method = req.method().clone();
    let res = tls_http(upstream, req, session_guard).await;
    info!("Direct {method} finished");
    return res;
}

#[instrument(skip_all, fields(addr = ?dst_addr))]
async fn proxy(
    conn_selector: &ConnSelector<StreamAddr>,
    dst_addr: StreamAddr,
    req: Request<Incoming>,
    start: (std::time::Instant, std::time::SystemTime),
    ctx: &HttpAccessConnContext,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError> {
    // Establish proxy chain
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
    let upstream = establish(&chain, dst_addr.clone(), &ctx.stream_context).await?;

    let session_guard = ctx.stream_context.session_table.as_ref().map(|s| {
        s.set_scope_owned(Session {
            start: SystemTime::now(),
            end: None,
            destination: Some(dst_addr.clone()),
            upstream_local: upstream.stream.local_addr().ok(),
            upstream_remote: upstream.addr.clone(),
            downstream_local: Arc::clone(&ctx.listen_addr),
            downstream_remote: None,
            up_gauge: None,
            dn_gauge: None,
        })
    });
    let method = req.method().clone();
    let res = match &payload_crypto {
        Some(crypto) => {
            // Establish encrypted stream
            let (r, w) = tokio::io::split(upstream.stream);
            let (r, w) = same_key_nonce_ciphertext(crypto.key(), r, w);
            let upstream = tokio_chacha20::stream::DuplexStream::new(r, w);
            tls_http(upstream, req, session_guard).await
        }
        None => tls_http(upstream.stream, req, session_guard).await,
    };

    let end = std::time::Instant::now();
    let timing = Timing { start, end };
    let log = SimplifiedStreamProxyLog {
        stream: SimplifiedStreamLog {
            timing,
            upstream_addr: upstream.addr,
            upstream_sock_addr: upstream.sock_addr,
            downstream_addr: None,
        },
        destination: dst_addr.address,
    };
    info!(%log, "{method} finished");

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
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, TunnelError>
where
    Upstream: AsyncWrite + AsyncRead + Send + Unpin + 'static,
{
    // Establish TLS connection
    let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(TokioIo::new(upstream))
        .await
        .map_err(|e| {
            warn!(?e, "Failed to establish HTTP/1 handshake to upstream");
            e
        })?;
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            warn!(?err, "Connection failed");
        }
    });

    // Send HTTP/1 request
    let resp = sender.send_request(req).await.map_err(|e| {
        warn!(?e, "Failed to send HTTP/1 request to upstream");
        e
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

/// ref: <https://datatracker.ietf.org/doc/html/rfc9112#name-absolute-form>
fn transform_absolute_form_req(uri: &mut http::Uri, headers: &mut http::HeaderMap) {
    let Some(auth) = uri.authority() else {
        return;
    };
    if uri.scheme().is_none() {
        return;
    }
    // `uri` is in absolute-form

    let new_host_value: Cow<str> = match auth.port() {
        Some(port) => format!("{}:{port}", auth.host()).into(),
        None => auth.host().into(),
    };
    let new_host_value = new_host_value.parse().unwrap();
    headers.insert(http::header::HOST, new_host_value);

    // in case origin fails to parse absolute-form
    let relative_ref = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    *uri = relative_ref.parse().unwrap();
}
#[cfg(test)]
#[test]
fn test_transform_absolute_form_req() {
    let absolute_form: http::Uri = "http://www.example.org/pub/WWW/TheProject.html"
        .parse()
        .unwrap();
    let mut uri = absolute_form;
    let mut headers = http::HeaderMap::new();

    transform_absolute_form_req(&mut uri, &mut headers);
    assert_eq!(headers.get(http::header::HOST).unwrap(), "www.example.org");
    assert_eq!(uri.to_string(), "/pub/WWW/TheProject.html");

    transform_absolute_form_req(&mut uri, &mut headers);
    assert_eq!(headers.get(http::header::HOST).unwrap(), "www.example.org");
    assert_eq!(uri.to_string(), "/pub/WWW/TheProject.html");
}
