use std::net::SocketAddr;

use common::{
    crypto::XorCryptoCursor,
    error::ProxyProtocolError,
    header::{
        convert_proxy_configs_to_header_crypto_pairs, read_header_async, write_header_async,
        InternetAddr, ProxyConfig, ResponseHeader,
    },
    heartbeat,
    stream::{connect_with_pool, pool::Pool, tcp::TcpConnector, CreatedStream, StreamConnector},
};
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct TcpProxyStream;

impl TcpProxyStream {
    #[instrument(skip(proxy_configs, tcp_pool))]
    pub async fn establish(
        proxy_configs: &[ProxyConfig],
        destination: &InternetAddr,
        tcp_pool: &Pool,
    ) -> Result<(CreatedStream, SocketAddr), ProxyProtocolError> {
        let connector = StreamConnector::Tcp(TcpConnector);

        // If there are no proxy configs, just connect to the destination
        if proxy_configs.is_empty() {
            return connect_with_pool(&connector, destination, tcp_pool, true).await;
        }

        // Connect to the first proxy
        let proxy_addr = &proxy_configs[0].address;
        let (mut stream, sock_addr) =
            connect_with_pool(&connector, proxy_addr, tcp_pool, true).await?;

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
            trace!(?node.address, "Reading response from upstream address");
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
                err.source = node.address.clone();
                error!(?err, ?proxy_addr, "Response was not successful");
                return Err(ProxyProtocolError::Response(err));
            }
        }

        // Return stream
        Ok((stream, sock_addr))
    }
}
