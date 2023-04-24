use std::{
    io::{self, Write},
    ops::Deref,
};

use common::{
    addr::any_addr,
    error::ProxyProtocolError,
    header::{
        convert_proxy_configs_to_header_crypto_pairs, InternetAddr, ProxyConfig, ResponseHeader,
    },
    header::{read_header, write_header},
};
use tokio::net::UdpSocket;
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct UdpProxySocket {
    upstream: UdpSocket,
    headers_bytes: Vec<u8>,
    proxy_configs: Vec<ProxyConfig>,
}

impl UdpProxySocket {
    #[instrument(skip_all)]
    pub async fn establish(
        proxy_configs: Vec<ProxyConfig>,
        destination: &InternetAddr,
    ) -> Result<UdpProxySocket, ProxyProtocolError> {
        // If there are no proxy configs, just connect to the destination
        if proxy_configs.is_empty() {
            let destination = destination.to_socket_addr()?;
            let any_addr = any_addr(&destination.ip());
            let upstream = UdpSocket::bind(any_addr)
                .await
                .inspect_err(|e| error!(?e, "Failed to bind to any address for UDP proxy"))?;
            upstream.connect(destination).await.inspect_err(|e| {
                error!(?e, "Failed to connect to upstream address for UDP proxy")
            })?;
            return Ok(UdpProxySocket {
                upstream,
                headers_bytes: Vec::new(),
                proxy_configs,
            });
        }

        // Connect to upstream
        let address = proxy_configs[0].address.to_socket_addr()?;
        let any_addr = any_addr(&address.ip());
        let upstream = UdpSocket::bind(any_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to any address for UDP proxy"))?;
        upstream
            .connect(address)
            .await
            .inspect_err(|e| error!(?e, "Failed to connect to upstream address for UDP proxy"))?;

        // Convert addresses to headers
        let pairs = convert_proxy_configs_to_header_crypto_pairs(&proxy_configs, destination);

        // Save headers to buffer
        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);
        for (header, crypto) in pairs {
            trace!(?header, "Writing header to buffer");
            write_header(&mut writer, &header, crypto)
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
        self.upstream
            .send(&new_buf)
            .await
            .inspect_err(|e| error!(?e, "Failed to send data to upstream address for UDP proxy"))?;

        Ok(buf.len())
    }

    #[instrument(skip_all)]
    pub async fn recv(&self, buf: &mut [u8]) -> Result<usize, ProxyProtocolError> {
        let mut new_buf = vec![0; self.headers_bytes.len() + buf.len()];

        // Read data
        let n = self.upstream.recv(&mut new_buf).await.inspect_err(|e| {
            error!(
                ?e,
                "Failed to receive data from upstream address for UDP proxy"
            )
        })?;
        let mut new_buf = &mut new_buf[..n];

        let mut reader = io::Cursor::new(&mut new_buf);

        // Decode and check headers
        for node in self.proxy_configs.iter() {
            trace!(?node.address, "Reading response");
            let resp: ResponseHeader = read_header(&mut reader, &node.crypto).inspect_err(|e| {
                error!(
                    ?e,
                    "Failed to read response from upstream address for UDP proxy"
                )
            })?;
            if let Err(mut err) = resp.result {
                err.source = node.address.clone();
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
