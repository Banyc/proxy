use std::{
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    ops::Deref,
};

use models::{write_header, ProxyProtocolError, RequestHeader};
use tokio::net::UdpSocket;
use tracing::{instrument, trace};

#[derive(Debug)]
pub struct UdpProxySocket {
    upstream: UdpSocket,
    header: Vec<u8>,
}

impl UdpProxySocket {
    #[instrument(skip_all)]
    pub async fn establish(addresses: &[SocketAddr]) -> Result<UdpProxySocket, ProxyProtocolError> {
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
        })
    }

    #[instrument(skip_all)]
    pub async fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut new_buf = Vec::new();
        let mut writer = io::Cursor::new(&mut new_buf);

        // Write header
        writer.write(&self.header)?;

        // Write data
        writer.write(buf)?;

        // Send data
        self.upstream.send(&new_buf).await?;

        Ok(buf.len())
    }
}

impl Deref for UdpProxySocket {
    type Target = UdpSocket;

    fn deref(&self) -> &Self::Target {
        &self.upstream
    }
}
