use std::{
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    ops::Deref,
};

use models::{read_header, write_header, ProxyProtocolError, RequestHeader, ResponseHeader};
use tokio::net::UdpSocket;
use tracing::{instrument, trace};

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
        let upstream = UdpSocket::bind(any_addr).await?;
        upstream.connect(addresses[0]).await?;

        // Convert addresses to headers
        let headers = addresses[1..]
            .iter()
            .map(|addr| RequestHeader { upstream: *addr })
            .collect::<Vec<_>>();

        // Save headers to buffer
        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);
        for header in headers {
            write_header(&mut writer, &header)?;
            trace!(?header, "Wrote header");
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
        writer.write_all(&self.header)?;

        // Write payload
        writer.write_all(buf)?;

        // Send data
        self.upstream.send(&new_buf).await?;

        Ok(buf.len())
    }

    #[instrument(skip_all)]
    pub async fn recv(&self, buf: &mut [u8]) -> Result<usize, ProxyProtocolError> {
        let mut new_buf = vec![0; self.header.len() + buf.len()];

        // Read data
        let n = self.upstream.recv(&mut new_buf).await?;
        let mut new_buf = &mut new_buf[..n];

        let mut reader = io::Cursor::new(&mut new_buf);

        // Decode and check headers
        for address in self.addresses[..self.addresses.len() - 1].iter() {
            let resp: ResponseHeader = read_header(&mut reader)?;
            trace!(?resp, "Read response");
            if let Err(mut err) = resp.result {
                err.source = *address;
                return Err(ProxyProtocolError::Response(err));
            }
            trace!(?address, "Response was successful");
        }
        trace!("All responses were successful");

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
