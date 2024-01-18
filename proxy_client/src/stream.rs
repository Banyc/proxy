use std::time::{Duration, Instant};

use common::{
    error::AnyError,
    header::{
        codec::{timed_read_header_async, timed_write_header_async, CodecError},
        heartbeat::{self, HeartbeatError},
        route::{RouteError, RouteResponse},
    },
    proxy_table::{convert_proxies_to_header_crypto_pairs, Tracer},
    stream::{
        concrete::{
            addr::{ConcreteStreamAddr, ConcreteStreamType},
            created_stream::CreatedStreamAndAddr,
            pool::{connect_with_pool, ConcreteConnPool, ConnectError},
        },
        proxy_table::StreamProxyChain,
    },
};
use metrics::counter;
use thiserror::Error;
use tracing::{error, instrument, trace};

const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[instrument(skip(proxies, stream_pool))]
pub async fn establish(
    proxies: &StreamProxyChain<ConcreteStreamType>,
    destination: ConcreteStreamAddr,
    stream_pool: &ConcreteConnPool,
) -> Result<CreatedStreamAndAddr, StreamEstablishError> {
    // If there are no proxy configs, just connect to the destination
    if proxies.is_empty() {
        let (stream, sock_addr) = connect_with_pool(&destination, stream_pool, true, IO_TIMEOUT)
            .await
            .map_err(StreamEstablishError::ConnectDestination)?;
        return Ok(CreatedStreamAndAddr {
            stream,
            addr: destination,
            sock_addr,
        });
    }

    // Connect to the first proxy
    let (mut stream, addr, sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) = connect_with_pool(proxy_addr, stream_pool, true, IO_TIMEOUT)
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
        let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new(*crypto.key());
        timed_write_header_async(&mut stream, header, &mut crypto_cursor, IO_TIMEOUT)
            .await
            .map_err(|e| StreamEstablishError::WriteStreamRequestHeader {
                source: e,
                upstream_addr: addr.clone(),
            })?;
    }

    // // Read response
    // for node in proxies {
    //     trace!(?node.address.address, "Reading response from upstream address");
    //     let mut crypto_cursor = XorCryptoCursor::new(&node.crypto);
    //     let resp: ResponseHeader = read_header_async(&mut stream, &mut crypto_cursor)
    //         .await
    //         .inspect_err(|e| {
    //             error!(
    //                 ?e,
    //                 ?proxy_addr,
    //                 "Failed to read response from upstream address"
    //             )
    //         })?;
    //     if let Err(mut err) = resp.result {
    //         err.source = node.address.address.clone();
    //         error!(?err, ?proxy_addr, "Response was not successful");
    //         return Err(ProxyProtocolError::Response(err));
    //     }
    // }

    // Return stream
    Ok(CreatedStreamAndAddr {
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
        upstream_addr: ConcreteStreamAddr,
    },
    #[error("Failed to read stream request header to upstream: {source}, {upstream_addr}")]
    WriteStreamRequestHeader {
        #[source]
        source: CodecError,
        upstream_addr: ConcreteStreamAddr,
    },
}

#[derive(Debug, Clone)]
pub struct StreamTracer {
    stream_pool: ConcreteConnPool,
}

impl StreamTracer {
    pub fn new(stream_pool: ConcreteConnPool) -> Self {
        Self { stream_pool }
    }
}

impl Tracer for StreamTracer {
    type Address = ConcreteStreamAddr;

    async fn trace_rtt(
        &self,
        chain: &StreamProxyChain<ConcreteStreamType>,
    ) -> Result<Duration, AnyError> {
        trace_rtt(chain, &self.stream_pool)
            .await
            .map_err(|e| e.into())
    }
}

pub async fn trace_rtt(
    proxies: &StreamProxyChain<ConcreteStreamType>,
    stream_pool: &ConcreteConnPool,
) -> Result<Duration, TraceError> {
    if proxies.is_empty() {
        return Ok(Duration::from_secs(0));
    }

    // Connect to the first proxy
    let (mut stream, _addr, _sock_addr) = {
        let proxy_addr = &proxies[0].address;
        let (stream, sock_addr) =
            connect_with_pool(proxy_addr, stream_pool, true, IO_TIMEOUT).await?;
        (stream, proxy_addr.clone(), sock_addr)
    };

    // Convert addresses to headers
    let pairs = convert_proxies_to_header_crypto_pairs(proxies, None);

    let start = Instant::now();

    // Write headers to stream
    for (header, crypto) in &pairs {
        heartbeat::send_upgrade(&mut stream, IO_TIMEOUT, crypto).await?;
        let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new(*crypto.key());
        timed_write_header_async(&mut stream, header, &mut crypto_cursor, IO_TIMEOUT).await?;
    }

    // Read response
    let mut crypto_cursor =
        tokio_chacha20::cursor::DecryptCursor::new(*pairs.last().unwrap().1.key());
    let resp: RouteResponse =
        timed_read_header_async(&mut stream, &mut crypto_cursor, IO_TIMEOUT).await?;
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
