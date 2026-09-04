use crate::{
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        preamble::{self, PreambleError},
        route::{RouteError, RouteResponse},
    },
    proxy_runtime::{
        addr::RouteAddr, conn::stream::ConnAndAddr, context::StreamRuntime,
        relay::same_key_nonce_ciphertext,
    },
    route::{HopConfig, ProbeRtt, RouteChain, convert_proxies_to_header_crypto_pairs},
    stream_runtime::{
        HasIoAddr, IoConnection, OwnedIoStream,
        pool::{ConnectError, connect_with_pool},
    },
};
use ae::anti_replay::ValidatorRef;
use metrics::counter;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{instrument, trace};

type PayloadCryptoReader =
    tokio_chacha20::stream::NonceCiphertextReader<tokio::io::ReadHalf<Box<dyn IoConnection>>>;
type PayloadCryptoWriter =
    tokio_chacha20::stream::NonceCiphertextWriter<tokio::io::WriteHalf<Box<dyn IoConnection>>>;
#[derive(Debug)]
struct PayloadCryptoConn {
    stream: tokio_chacha20::stream::DuplexStream<PayloadCryptoReader, PayloadCryptoWriter>,
    local_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
}

impl PayloadCryptoConn {
    fn wrap(
        stream: Box<dyn IoConnection>,
        crypto: &tokio_chacha20::config::Config,
    ) -> Box<dyn IoConnection> {
        let local_addr = stream.local_addr().ok();
        let peer_addr = stream.peer_addr().ok();
        let (reader, writer) = tokio::io::split(stream);
        let (reader, writer) = same_key_nonce_ciphertext(crypto.key(), reader, writer);
        Box::new(Self {
            stream: tokio_chacha20::stream::DuplexStream::new(reader, writer),
            local_addr,
            peer_addr,
        })
    }
}

impl AsyncRead for PayloadCryptoConn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PayloadCryptoConn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl OwnedIoStream for PayloadCryptoConn {}
impl IoConnection for PayloadCryptoConn {}

impl HasIoAddr for PayloadCryptoConn {
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.peer_addr.ok_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "peer address unavailable")
        })
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.local_addr.ok_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "local address unavailable")
        })
    }
}

#[instrument(skip_all)]
pub async fn establish(
    proxies: &RouteChain,
    destination: RouteAddr,
    stream_context: &StreamRuntime,
) -> Result<ConnAndAddr, StreamEstablishError> {
    if proxies.is_empty() {
        let (stream, sock_addr) =
            connect_with_pool(&destination, stream_context, true, crate::STREAM_IO_TIMEOUT)
                .await
                .map_err(|source| StreamEstablishError::ConnectDestination {
                    source: Box::new(source),
                    upstream_addr: destination.clone(),
                })?;
        stream.set_stream_name(&destination.address.to_string());
        return Ok(ConnAndAddr {
            stream,
            addr: destination,
            sock_addr,
        });
    }
    let (mut stream, addr, sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) =
            connect_with_pool(proxy_addr, stream_context, true, crate::STREAM_IO_TIMEOUT)
                .await
                .map_err(|source| StreamEstablishError::ConnectFirstProxyServer {
                    source: Box::new(source),
                    upstream_addr: proxy_addr.clone(),
                })?;
        (stream, proxy_addr.clone(), sock_addr)
    };
    stream.set_stream_name(&destination.address.to_string());
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, Some(destination));
    for ((header, crypto), proxy) in pairs.iter().zip(proxies) {
        trace!(?header, "Writing headers to stream");
        preamble::send_upgrade(&mut stream, crate::STREAM_IO_TIMEOUT, crypto)
            .await
            .map_err(|e| StreamEstablishError::WriteHeartbeatUpgrade {
                source: e,
                upstream_addr: addr.clone(),
            })?;
        timed_write_header_async(&mut stream, header, *crypto.key(), crate::STREAM_IO_TIMEOUT)
            .await
            .map_err(|e| StreamEstablishError::WriteStreamRequestHeader {
                source: e,
                upstream_addr: addr.clone(),
            })?;
        if let Some(payload_crypto) = &proxy.payload_crypto {
            stream = PayloadCryptoConn::wrap(stream, payload_crypto);
        }
    }
    Ok(ConnAndAddr {
        stream,
        addr,
        sock_addr,
    })
}

#[derive(Debug, Error)]
pub enum StreamEstablishError {
    #[error("Failed to connect to destination: {source}, {upstream_addr}")]
    ConnectDestination {
        #[source]
        source: Box<ConnectError>,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to connect to first proxy server: {source}, {upstream_addr}")]
    ConnectFirstProxyServer {
        #[source]
        source: Box<ConnectError>,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to write heartbeat upgrade to upstream: {source}, {upstream_addr}")]
    WriteHeartbeatUpgrade {
        #[source]
        source: PreambleError,
        upstream_addr: RouteAddr,
    },
    #[error("Failed to read stream request header to upstream: {source}, {upstream_addr}")]
    WriteStreamRequestHeader {
        #[source]
        source: CodecError,
        upstream_addr: RouteAddr,
    },
}

#[derive(Debug, Clone)]
pub struct StreamTracer {
    stream_context: StreamRuntime,
}
impl StreamTracer {
    pub fn new(stream_context: StreamRuntime) -> Self {
        Self { stream_context }
    }
}
impl ProbeRtt for StreamTracer {
    fn probe_kind(&self) -> &'static str {
        "stream"
    }
    fn probe_rtt(
        &self,
        chain: &RouteChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::route::ProbeOutcome> + Send>>
    {
        let stream_context = self.stream_context.clone();
        let chain: Vec<HopConfig> = chain.to_vec();
        Box::pin(async move {
            crate::route::ProbeOutcome {
                rtt: probe_rtt(&chain, &stream_context).await.map_err(Into::into),
                // The stream probe has no teardown epilog to observe.
                epilog: None,
            }
        })
    }
    fn recycle(
        &self,
        chain: &RouteChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let Some(first) = chain.first() else {
            return Box::pin(async {});
        };
        let addr = first.address.clone();
        let protocol = first.address.protocol.clone();
        let connector_table = self.stream_context.connector_table.clone();
        Box::pin(async move {
            if addr.reverse_tunnel().is_some() {
                return;
            }
            let Ok(sock_addrs) = addr.address.to_socket_addrs().await else {
                return;
            };
            for sock_addr in sock_addrs.iter() {
                connector_table.reset_addr(protocol.as_ref(), *sock_addr);
            }
        })
    }
    fn reoptimize(
        &self,
        chain: &RouteChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let Some(first) = chain.first() else {
            return Box::pin(async {});
        };
        let addr = first.address.clone();
        let protocol = first.address.protocol.clone();
        let connector_table = self.stream_context.connector_table.clone();
        Box::pin(async move {
            if addr.reverse_tunnel().is_some() {
                return;
            }
            let Ok(sock_addrs) = addr.address.to_socket_addrs().await else {
                return;
            };
            for sock_addr in sock_addrs.iter() {
                connector_table.reoptimize(protocol.as_ref(), *sock_addr);
            }
        })
    }
    fn session_stats(
        &self,
        chain: &RouteChain,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> {
        let Some(first) = chain.first() else {
            return Box::pin(async { None });
        };
        let addr = first.address.clone();
        let stream_type = first.address.protocol.clone();
        let connector_table = self.stream_context.connector_table.clone();
        Box::pin(async move {
            if let Some((_, name)) = addr.reverse_tunnel() {
                return connector_table.named_session_stats(&stream_type, name);
            }
            if !connector_table.reports_session_stats(stream_type.as_ref()) {
                return None;
            }
            let Ok(sock_addrs) = addr.address.to_socket_addrs().await else {
                return None;
            };
            sock_addrs.iter().find_map(|sock_addr| {
                connector_table.session_stats(stream_type.as_ref(), *sock_addr)
            })
        })
    }
}
pub async fn probe_rtt(
    proxies: &RouteChain,
    stream_context: &StreamRuntime,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }
    let (mut stream, _addr, _sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) =
            connect_with_pool(proxy_addr, stream_context, true, crate::STREAM_IO_TIMEOUT).await?;
        (stream, proxy_addr.clone(), sock_addr)
    };
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);
    let start = Instant::now();
    for (index, ((header, crypto), proxy)) in pairs.iter().zip(proxies).enumerate() {
        preamble::send_upgrade(&mut stream, crate::STREAM_IO_TIMEOUT, crypto).await?;
        timed_write_header_async(&mut stream, header, *crypto.key(), crate::STREAM_IO_TIMEOUT)
            .await?;
        if index + 1 < proxies.len()
            && let Some(payload_crypto) = &proxy.payload_crypto
        {
            stream = PayloadCryptoConn::wrap(stream, payload_crypto);
        }
    }
    let validator = ValidatorRef::Replay(&stream_context.replay_validator);
    let resp: RouteResponse = timed_read_header_async(
        &mut stream,
        *pairs.last().unwrap().1.key(),
        &validator,
        crate::STREAM_IO_TIMEOUT,
    )
    .await?;
    if let Err(err) = resp.result {
        return Err(TraceError::Response { err });
    }
    let end = Instant::now();
    counter!("stream.rtt_probes").increment(1);
    Ok(end.duration_since(start))
}
#[derive(Debug, Error)]
pub enum TraceError {
    #[error("Connect error: {0}")]
    ConnectError(#[from] ConnectError),
    #[error("Heartbeat error: {0}")]
    PreambleError(#[from] PreambleError),
    #[error("Codec error: {0}")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error: {err}")]
    Response { err: RouteError },
}
