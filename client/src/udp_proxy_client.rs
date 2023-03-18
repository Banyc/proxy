use std::{
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    ops::Deref,
};

use models::{read_header, write_header, ProxyProtocolError, RequestHeader, ResponseHeader};
use tokio::net::UdpSocket;
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct UdpProxySocket {
    upstream: UdpSocket,
    header: Vec<u8>,
    addresses: Vec<SocketAddr>,
}

impl UdpProxySocket {
    #[instrument(skip_all)]
    pub async fn establish(
        addresses: Vec<SocketAddr>,
    ) -> Result<UdpProxySocket, ProxyProtocolError> {
        // Connect to upstream
        let any_ip = match addresses[0].ip() {
            IpAddr::V4(_) => IpAddr::V4("0.0.0.0".parse().unwrap()),
            IpAddr::V6(_) => IpAddr::V6("::".parse().unwrap()),
        };
        let any_addr = SocketAddr::new(any_ip, 0);
        let upstream = UdpSocket::bind(any_addr)
            .await
            .inspect_err(|e| error!(?e, "Failed to bind to any address for UDP proxy"))?;
        upstream
            .connect(addresses[0])
            .await
            .inspect_err(|e| error!(?e, "Failed to connect to upstream address for UDP proxy"))?;

        // Convert addresses to headers
        let headers = addresses[1..]
            .iter()
            .map(|addr| RequestHeader { upstream: *addr })
            .collect::<Vec<_>>();

        // Save headers to buffer
        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);
        for header in headers {
            trace!(?header, "Writing header to buffer");
            write_header(&mut writer, &header)
                .inspect_err(|e| error!(?e, "Failed to write header to buffer for UDP proxy"))?;
        }

        // Return stream
        Ok(UdpProxySocket {
            upstream,
            header: buf,
            addresses,
        })
    }

    #[instrument(skip_all)]
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let mut new_buf = Vec::new();
        let mut writer = io::Cursor::new(&mut new_buf);

        // Write header
        writer
            .write_all(&self.header)
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
        let mut new_buf = vec![0; self.header.len() + buf.len()];

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
        for address in self.addresses[..self.addresses.len() - 1].iter() {
            trace!(?address, "Reading response");
            let resp: ResponseHeader = read_header(&mut reader).inspect_err(|e| {
                error!(
                    ?e,
                    "Failed to read response from upstream address for UDP proxy"
                )
            })?;
            if let Err(mut err) = resp.result {
                err.source = *address;
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
