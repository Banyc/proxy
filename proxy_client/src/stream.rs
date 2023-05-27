use std::net::SocketAddr;

use common::{
    crypto::XorCryptoCursor,
    error::ProxyProtocolError,
    header::{convert_proxy_configs_to_header_crypto_pairs, write_header_async},
    heartbeat,
    stream::{
        connect_with_pool, header::StreamProxyConfig, pool::Pool, ConnectError, CreatedStream,
        StreamAddr,
    },
};
use thiserror::Error;
use tracing::{error, instrument, trace};

#[instrument(skip(proxy_configs, stream_pool))]
pub async fn establish(
    proxy_configs: &[StreamProxyConfig],
    destination: StreamAddr,
    stream_pool: &Pool,
) -> Result<(CreatedStream, StreamAddr, SocketAddr), StreamEstablishError> {
    // If there are no proxy configs, just connect to the destination
    if proxy_configs.is_empty() {
        let (stream, sock_addr) = connect_with_pool(&destination, stream_pool, true)
            .await
            .map_err(StreamEstablishError::ConnectDestination)?;
        return Ok((stream, destination, sock_addr));
    }

    // Connect to the first proxy
    let (mut stream, addr, sock_addr) = {
        let proxy_addr = &proxy_configs[0].address;
        let (stream, sock_addr) = connect_with_pool(proxy_addr, stream_pool, true)
            .await
            .map_err(StreamEstablishError::ConnectFirstProxyServer)?;
        (stream, proxy_addr.clone(), sock_addr)
    };

    // Convert addresses to headers
    let pairs = convert_proxy_configs_to_header_crypto_pairs(proxy_configs, destination);

    // Write headers to stream
    for (header, crypto) in pairs {
        trace!(?header, "Writing headers to stream");
        heartbeat::send_upgrade(&mut stream).await.map_err(|e| {
            StreamEstablishError::WriteHeartbeatUpgrade {
                source: e,
                upstream_addr: addr.clone(),
            }
        })?;
        let mut crypto_cursor = XorCryptoCursor::new(crypto);
        write_header_async(&mut stream, &header, &mut crypto_cursor)
            .await
            .map_err(|e| StreamEstablishError::WriteStreamRequestHeader {
                source: e,
                upstream_addr: addr.clone(),
            })?;
    }

    // // Read response
    // for node in proxy_configs {
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
        source: ProxyProtocolError,
        upstream_addr: StreamAddr,
    },
    #[error("Failed to read stream request header to upstream")]
    WriteStreamRequestHeader {
        #[source]
        source: ProxyProtocolError,
        upstream_addr: StreamAddr,
    },
}
