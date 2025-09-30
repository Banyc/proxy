use std::time::{Duration, Instant};

use crate::{
    error::AnyError,
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        heartbeat::{self, HeartbeatError},
        route::{RouteError, RouteResponse},
    },
    proto::{addr::StreamAddr, conn::stream::ConnAndAddr, context::StreamContext},
    route::{BuildTracer, ConnChain, TraceRtt, convert_proxies_to_header_crypto_pairs},
    stream::pool::{ConnectError, connect_with_pool},
};
use ae::anti_replay::ValidatorRef;
use metrics::counter;
use thiserror::Error;
use tracing::{instrument, trace};

const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[instrument(skip(proxies, stream_context))]
pub async fn establish(
    proxies: &ConnChain<StreamAddr>,
    destination: StreamAddr,
    stream_context: &StreamContext,
) -> Result<ConnAndAddr, StreamEstablishError> {
    // If there are no proxy configs, just connect to the destination
    if proxies.is_empty() {
        let (stream, sock_addr) = connect_with_pool(&destination, stream_context, true, IO_TIMEOUT)
            .await
            .map_err(StreamEstablishError::ConnectDestination)?;
        return Ok(ConnAndAddr {
            stream,
            addr: destination,
            sock_addr,
        });
    }

    // Connect to the first proxy
    let (mut stream, addr, sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) = connect_with_pool(proxy_addr, stream_context, true, IO_TIMEOUT)
            .await
            .map_err(StreamEstablishError::ConnectFirstProxyServer)?;
        (stream, proxy_addr.clone(), sock_addr)
    };

    // Convert addresses to headers
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, Some(destination));

    // Write headers to stream
    for (header, crypto) in &pairs {
        trace!(?header, "Writing headers to stream");
        heartbeat::send_upgrade(&mut stream, IO_TIMEOUT, crypto)
            .await
            .map_err(|e| StreamEstablishError::WriteHeartbeatUpgrade {
                source: e,
                upstream_addr: addr.clone(),
            })?;
        timed_write_header_async(&mut stream, header, *crypto.key(), IO_TIMEOUT)
            .await
            .map_err(|e| StreamEstablishError::WriteStreamRequestHeader {
                source: e,
                upstream_addr: addr.clone(),
            })?;
    }

    // Return stream
    Ok(ConnAndAddr {
        stream,
        addr,
        sock_addr,
    })
}

#[derive(Debug, Error)]
pub enum StreamEstablishError {
    #[error("Failed to connect to destination: {0}")]
    ConnectDestination(#[source] ConnectError),
    #[error("Failed to connect to first proxy server: {0}")]
    ConnectFirstProxyServer(#[source] ConnectError),
    #[error("Failed to write heartbeat upgrade to upstream: {source}, {upstream_addr}")]
    WriteHeartbeatUpgrade {
        #[source]
        source: HeartbeatError,
        upstream_addr: StreamAddr,
    },
    #[error("Failed to read stream request header to upstream: {source}, {upstream_addr}")]
    WriteStreamRequestHeader {
        #[source]
        source: CodecError,
        upstream_addr: StreamAddr,
    },
}

#[derive(Debug, Clone)]
pub struct StreamTracerBuilder {
    stream_context: StreamContext,
}
impl StreamTracerBuilder {
    pub fn new(stream_context: StreamContext) -> Self {
        Self { stream_context }
    }
}
impl BuildTracer for StreamTracerBuilder {
    type Tracer = StreamTracer;

    fn build(&self) -> Self::Tracer {
        StreamTracer::new(self.stream_context.clone())
    }
}

#[derive(Debug, Clone)]
pub struct StreamTracer {
    stream_context: StreamContext,
}
impl StreamTracer {
    pub fn new(stream_context: StreamContext) -> Self {
        Self { stream_context }
    }
}
impl TraceRtt for StreamTracer {
    type Addr = StreamAddr;

    async fn trace_rtt(&self, chain: &ConnChain<StreamAddr>) -> Result<Duration, AnyError> {
        trace_rtt(chain, &self.stream_context)
            .await
            .map_err(|e| e.into())
    }
}
pub async fn trace_rtt(
    proxies: &ConnChain<StreamAddr>,
    stream_context: &StreamContext,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }

    // Connect to the first proxy
    let (mut stream, _addr, _sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) =
            connect_with_pool(proxy_addr, stream_context, true, IO_TIMEOUT).await?;
        (stream, proxy_addr.clone(), sock_addr)
    };

    // Convert addresses to headers
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);

    let start = Instant::now();

    // Write headers to stream
    for (header, crypto) in &pairs {
        heartbeat::send_upgrade(&mut stream, IO_TIMEOUT, crypto).await?;
        timed_write_header_async(&mut stream, header, *crypto.key(), IO_TIMEOUT).await?;
    }

    // Read response
    let validator = ValidatorRef::Replay(&stream_context.replay_validator);
    let resp: RouteResponse = timed_read_header_async(
        &mut stream,
        *pairs.last().unwrap().1.key(),
        &validator,
        IO_TIMEOUT,
    )
    .await?;
    if let Err(err) = resp.result {
        return Err(TraceError::Response { err });
    }

    let end = Instant::now();

    counter!("stream.traces").increment(1);
    Ok(end.duration_since(start))
}
#[derive(Debug, Error)]
pub enum TraceError {
    #[error("Connect error: {0}")]
    ConnectError(#[from] ConnectError),
    #[error("Heartbeat error: {0}")]
    HeartbeatError(#[from] HeartbeatError),
    #[error("Codec error: {0}")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error: {err}")]
    Response { err: RouteError },
}
