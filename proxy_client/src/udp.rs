use std::{
    io::{self, Write},
    ops::Deref,
};

use common::{
    addr::any_addr,
    crypto::XorCryptoCursor,
    error::ProxyProtocolError,
    header::{convert_proxy_configs_to_header_crypto_pairs, ProxyConfig, ResponseHeader},
    header::{read_header, write_header},
    udp,
};
use tokio::net::UdpSocket;
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct UdpProxySocket {
    upstream: UdpSocket,
    headers_bytes: Vec<u8>,
    proxy_configs: Vec<ProxyConfig<udp::header::RequestHeader>>,
}

impl UdpProxySocket {
    #[instrument(skip_all)]
    pub async fn establish(
        proxy_configs: Vec<ProxyConfig<udp::header::RequestHeader>>,
        destination: udp::header::RequestHeader,
    ) -> Result<UdpProxySocket, ProxyProtocolError> {
        // If there are no proxy configs, just connect to the destination
        if proxy_configs.is_empty() {
            let addr = destination
                .address
                .to_socket_addr()
                .await
                .inspect_err(|e| {
                    error!(
                        ?e,
                        ?destination,
                        "Failed to resolve destination address for UDP proxy"
                    )
                })?;
            let any_addr = any_addr(&addr.ip());
            let upstream = UdpSocket::bind(any_addr)
                .await
                .inspect_err(|e| error!(?e, "Failed to bind to any address for UDP proxy"))?;
            upstream.connect(addr).await.inspect_err(|e| {
                error!(
                    ?e,
                    ?destination,
                    "Failed to connect to upstream address for UDP proxy"
                )
            })?;
            return Ok(UdpProxySocket {
                upstream,
                headers_bytes: Vec::new(),
                proxy_configs,
            });
        }

        // Connect to upstream
        let proxy_addr = &proxy_configs[0].header.address;
        let addr = proxy_addr.to_socket_addr().await.inspect_err(|e| {
            error!(
                ?e,
                ?proxy_addr,
                "Failed to resolve proxy address for UDP proxy"
            )
        })?;
        let any_addr = any_addr(&addr.ip());
        let upstream = UdpSocket::bind(any_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to any address for UDP proxy"))?;
        upstream.connect(addr).await.inspect_err(|e| {
            error!(
                ?e,
                ?proxy_addr,
                "Failed to connect to upstream address for UDP proxy"
            )
        })?;

        // Convert addresses to headers
        let pairs = convert_proxy_configs_to_header_crypto_pairs(&proxy_configs, destination);

        // Save headers to buffer
        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);
        for (header, crypto) in pairs {
            trace!(?header, "Writing header to buffer");
            let mut crypto_cursor = XorCryptoCursor::new(crypto);
            write_header(&mut writer, &header, &mut crypto_cursor)
                .inspect_err(|e| error!(?e, "Failed to write header to buffer for UDP proxy"))?;
        }

        // Return stream
        Ok(UdpProxySocket {
            upstream,
            headers_bytes: buf,
            proxy_configs,
        })
    }

    #[instrument(skip_all)]
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let mut new_buf = Vec::new();
        let mut writer = io::Cursor::new(&mut new_buf);

        // Write header
        writer
            .write_all(&self.headers_bytes)
            .inspect_err(|e| error!(?e, "Failed to write header to buffer for UDP proxy"))?;

        // Write payload
        writer
            .write_all(buf)
            .inspect_err(|e| error!(?e, "Failed to write payload to buffer for UDP proxy"))?;

        // Send data
        self.upstream.send(&new_buf).await.inspect_err(|e| {
            let peer_addr = self.upstream.peer_addr().ok();
            error!(
                ?e,
                ?peer_addr,
                "Failed to send data to upstream for UDP proxy"
            )
        })?;

        Ok(buf.len())
    }

    #[instrument(skip_all)]
    pub async fn recv(&self, buf: &mut [u8]) -> Result<usize, ProxyProtocolError> {
        let mut new_buf = vec![0; self.headers_bytes.len() + buf.len()];

        // Read data
        let n = self.upstream.recv(&mut new_buf).await.inspect_err(|e| {
            let peer_addr = self.upstream.peer_addr().ok();
            error!(
                ?e,
                ?peer_addr,
                "Failed to receive data from upstream for UDP proxy"
            )
        })?;
        let mut new_buf = &mut new_buf[..n];

        let mut reader = io::Cursor::new(&mut new_buf);

        // Decode and check headers
        for node in self.proxy_configs.iter() {
            trace!(?node.header.address, "Reading response");
            let mut crypto_cursor = XorCryptoCursor::new(&node.crypto);
            let resp: ResponseHeader =
                read_header(&mut reader, &mut crypto_cursor).inspect_err(|e| {
                    error!(?e, "Failed to read response from upstream for UDP proxy")
                })?;
            if let Err(mut err) = resp.result {
                err.source = node.header.address.clone();
                error!(?err, "Response was not successful");
                return Err(ProxyProtocolError::Response(err));
            }
        }

        // Read payload
        let payload_size = reader.get_ref().len() - reader.position() as usize;
        buf[..payload_size].copy_from_slice(&reader.get_ref()[reader.position() as usize..]);

        Ok(payload_size)
    }
}

impl Deref for UdpProxySocket {
    type Target = UdpSocket;

    fn deref(&self) -> &Self::Target {
        &self.upstream
    }
}
