use std::net::SocketAddr;

use common::{
    crypto::XorCryptoCursor,
    error::ProxyProtocolError,
    header::{
        convert_proxy_configs_to_header_crypto_pairs, read_header_async, write_header_async,
        ProxyConfig, ResponseHeader,
    },
    heartbeat,
    stream::{self, connect_with_pool, pool::Pool, CreatedStream, StreamConnector},
};
use tracing::{error, instrument, trace};

#[instrument(skip(proxy_configs, stream_pool))]
pub async fn establish(
    proxy_configs: &[ProxyConfig<stream::header::RequestHeader>],
    destination: stream::header::RequestHeader,
    stream_pool: &Pool,
) -> Result<(CreatedStream, SocketAddr), ProxyProtocolError> {
    // If there are no proxy configs, just connect to the destination
    if proxy_configs.is_empty() {
        let connector: StreamConnector = destination.stream_type.into();
        return connect_with_pool(&connector, &destination.address, stream_pool, true).await;
    }

    // Connect to the first proxy
    let ((mut stream, sock_addr), proxy_addr) = {
        let proxy_addr = &proxy_configs[0].header.address;
        let connector: StreamConnector = proxy_configs[0].header.stream_type.into();
        (
            connect_with_pool(&connector, proxy_addr, stream_pool, true).await?,
            proxy_addr,
        )
    };

    // Convert addresses to headers
    let pairs = convert_proxy_configs_to_header_crypto_pairs(proxy_configs, destination);

    // Write headers to stream
    for (header, crypto) in pairs {
        trace!(?header, "Writing headers to stream");
        heartbeat::send_upgrade(&mut stream)
            .await
            .inspect_err(|e| {
                error!(
                    ?e,
                    ?proxy_addr,
                    "Failed to write heartbeat upgrade to stream"
                )
            })?;
        let mut crypto_cursor = XorCryptoCursor::new(crypto);
        write_header_async(&mut stream, &header, &mut crypto_cursor)
            .await
            .inspect_err(|e| error!(?e, ?proxy_addr, "Failed to write header to stream"))?;
    }

    // Read response
    for node in proxy_configs {
        trace!(?node.header.address, "Reading response from upstream address");
        let mut crypto_cursor = XorCryptoCursor::new(&node.crypto);
        let resp: ResponseHeader = read_header_async(&mut stream, &mut crypto_cursor)
            .await
            .inspect_err(|e| {
                error!(
                    ?e,
                    ?proxy_addr,
                    "Failed to read response from upstream address"
                )
            })?;
        if let Err(mut err) = resp.result {
            err.source = node.header.address.clone();
            error!(?err, ?proxy_addr, "Response was not successful");
            return Err(ProxyProtocolError::Response(err));
        }
    }

    // Return stream
    Ok((stream, sock_addr))
}
