use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use common::{
    crypto::XorCryptoCursor,
    header::{
        codec::{timed_read_header_async, timed_write_header_async, CodecError},
        heartbeat::{self, HeartbeatError},
        route::{RouteError, RouteResponse},
    },
    proxy_table::convert_proxies_to_header_crypto_pairs,
    stream::{
        addr::StreamAddr, connect_with_pool, pool::Pool, proxy_table::StreamProxyConfig,
        ConnectError, CreatedStream,
    },
};
use thiserror::Error;
use tracing::{error, instrument, trace};

const IO_TIMEOUT: Duration = Duration::from_secs(60);

#[instrument(skip(proxies, stream_pool))]
pub async fn establish(
    proxies: &[StreamProxyConfig],
    destination: StreamAddr,
    stream_pool: &Pool,
) -> Result<(CreatedStream, StreamAddr, SocketAddr), StreamEstablishError> {
    // If there are no proxy configs, just connect to the destination
    if proxies.is_empty() {
        let (stream, sock_addr) = connect_with_pool(&destination, stream_pool, true, IO_TIMEOUT)
            .await
            .map_err(StreamEstablishError::ConnectDestination)?;
        return Ok((stream, destination, sock_addr));
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
    for (header, crypto) in pairs.as_ref() {
        trace!(?header, "Writing headers to stream");
        heartbeat::send_upgrade(&mut stream, IO_TIMEOUT)
            .await
            .map_err(|e| StreamEstablishError::WriteHeartbeatUpgrade {
                source: e,
                upstream_addr: addr.clone(),
            })?;
        let mut crypto_cursor = XorCryptoCursor::new(crypto);
        timed_write_header_async(&mut stream, &header, &mut crypto_cursor, IO_TIMEOUT)
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
    Ok((stream, addr, sock_addr))
}

#[derive(Debug, Error)]
pub enum StreamEstablishError {
    #[error("Failed to connect to destination")]
    ConnectDestination(#[source] ConnectError),
    #[error("Failed to connect to first proxy server")]
    ConnectFirstProxyServer(#[source] ConnectError),
    #[error("Failed to write heartbeat upgrade to upstream")]
    WriteHeartbeatUpgrade {
        #[source]
        source: HeartbeatError,
        upstream_addr: StreamAddr,
    },
    #[error("Failed to read stream request header to upstream")]
    WriteStreamRequestHeader {
        #[source]
        source: CodecError,
        upstream_addr: StreamAddr,
    },
}

pub async fn trace_rtt(
    proxies: &[StreamProxyConfig],
    stream_pool: &Pool,
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
    for (header, crypto) in pairs.as_ref() {
        heartbeat::send_upgrade(&mut stream, IO_TIMEOUT).await?;
        let mut crypto_cursor = XorCryptoCursor::new(crypto);
        timed_write_header_async(&mut stream, &header, &mut crypto_cursor, IO_TIMEOUT).await?;
    }

    // Read response
    let mut crypto_cursor = XorCryptoCursor::new(pairs.last().unwrap().1);
    let resp: RouteResponse =
        timed_read_header_async(&mut stream, &mut crypto_cursor, IO_TIMEOUT).await?;
    if let Err(err) = resp.result {
        return Err(TraceError::Response { err });
    }

    let end = Instant::now();

    Ok(end.duration_since(start))
}

#[derive(Debug, Error)]
pub enum TraceError {
    #[error("Connect error")]
    ConnectError(#[from] ConnectError),
    #[error("Heartbeat error")]
    HeartbeatError(#[from] HeartbeatError),
    #[error("Codec error")]
    Header(#[from] CodecError),
    #[error("Upstream responded with an error")]
    Response { err: RouteError },
}
