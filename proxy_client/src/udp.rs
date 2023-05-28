use std::{
    io::{self, Write},
    net::SocketAddr,
    ops::Deref,
};

use common::{
    addr::{any_addr, InternetAddr},
    config::convert_proxy_configs_to_header_crypto_pairs,
    crypto::XorCryptoCursor,
    error::ResponseError,
    header::ResponseHeader,
    header::{read_header, write_header, HeaderError},
    udp::config::UdpProxyConfig,
};
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::{error, instrument, trace};

#[derive(Debug)]
pub struct UdpProxySocket {
    upstream: UdpSocket,
    headers_bytes: Vec<u8>,
    proxy_configs: Vec<UdpProxyConfig>,
}

impl UdpProxySocket {
    #[instrument(skip_all)]
    pub async fn establish(
        proxy_configs: Vec<UdpProxyConfig>,
        destination: InternetAddr,
    ) -> Result<UdpProxySocket, EstablishError> {
        // If there are no proxy configs, just connect to the destination
        if proxy_configs.is_empty() {
            let addr = destination.to_socket_addr().await.map_err(|e| {
                EstablishError::ResolveDestination {
                    source: e,
                    addr: destination.clone(),
                }
            })?;
            let any_addr = any_addr(&addr.ip());
            let upstream = UdpSocket::bind(any_addr)
                .await
                .map_err(EstablishError::ClientBindAny)?;
            upstream
                .connect(addr)
                .await
                .map_err(|e| EstablishError::ConnectDestination {
                    source: e,
                    addr: destination.clone(),
                    sock_addr: addr,
                })?;
            return Ok(UdpProxySocket {
                upstream,
                headers_bytes: Vec::new(),
                proxy_configs,
            });
        }

        // Connect to upstream
        let proxy_addr = &proxy_configs[0].address;
        let addr =
            proxy_addr
                .to_socket_addr()
                .await
                .map_err(|e| EstablishError::ResolveFirstProxy {
                    source: e,
                    addr: proxy_addr.clone(),
                })?;
        let any_addr = any_addr(&addr.ip());
        let upstream = UdpSocket::bind(any_addr)
            .await
            .map_err(EstablishError::ClientBindAny)?;
        upstream
            .connect(addr)
            .await
            .map_err(|e| EstablishError::ConnectFirstProxy {
                source: e,
                addr: proxy_addr.clone(),
                sock_addr: addr,
            })?;

        // Convert addresses to headers
        let pairs = convert_proxy_configs_to_header_crypto_pairs(&proxy_configs, destination);

        // Save headers to buffer
        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);
        for (header, crypto) in pairs {
            trace!(?header, "Writing header to buffer");
            let mut crypto_cursor = XorCryptoCursor::new(crypto);
            write_header(&mut writer, &header, &mut crypto_cursor).unwrap();
        }

        // Return stream
        Ok(UdpProxySocket {
            upstream,
            headers_bytes: buf,
            proxy_configs,
        })
    }

    #[instrument(skip_all)]
    pub async fn send(&self, buf: &[u8]) -> Result<usize, SendError> {
        let mut new_buf = Vec::new();
        let mut writer = io::Cursor::new(&mut new_buf);

        // Write header
        writer.write_all(&self.headers_bytes).unwrap();

        // Write payload
        writer.write_all(buf).unwrap();

        // Send data
        self.upstream.send(&new_buf).await.map_err(|e| {
            let peer_addr = self.upstream.peer_addr().ok();
            SendError {
                source: e,
                sock_addr: peer_addr,
            }
        })?;

        Ok(buf.len())
    }

    #[instrument(skip_all)]
    pub async fn recv(&self, buf: &mut [u8]) -> Result<usize, RecvError> {
        let mut new_buf = vec![0; self.headers_bytes.len() + buf.len()];

        // Read data
        let n = self.upstream.recv(&mut new_buf).await.map_err(|e| {
            let peer_addr = self.upstream.peer_addr().ok();
            RecvError::RecvUpstream {
                source: e,
                sock_addr: peer_addr,
            }
        })?;
        let mut new_buf = &mut new_buf[..n];

        let mut reader = io::Cursor::new(&mut new_buf);

        // Decode and check headers
        for node in self.proxy_configs.iter() {
            trace!(?node.address, "Reading response");
            let mut crypto_cursor = XorCryptoCursor::new(&node.crypto);
            let resp: ResponseHeader = read_header(&mut reader, &mut crypto_cursor)?;
            if let Err(mut err) = resp.result {
                err.source = node.address.clone();
                error!(?err, "Upstream responded with an error");
                return Err(RecvError::Response(err));
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

#[derive(Debug, Error)]
pub enum EstablishError {
    #[error("Failed to resolve destination address")]
    ResolveDestination {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Failed to created a client socket")]
    ClientBindAny(#[source] io::Error),
    #[error("Failed to connect to destination")]
    ConnectDestination {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to resolve first proxy address")]
    ResolveFirstProxy {
        #[source]
        source: io::Error,
        addr: InternetAddr,
    },
    #[error("Failed to connect to first proxy")]
    ConnectFirstProxy {
        #[source]
        source: io::Error,
        addr: InternetAddr,
        sock_addr: SocketAddr,
    },
}

#[derive(Debug, Error)]
#[error("Failed to send to upstream")]
pub struct SendError {
    #[source]
    source: io::Error,
    sock_addr: Option<SocketAddr>,
}

#[derive(Debug, Error)]
pub enum RecvError {
    #[error("Failed to recv from upstream")]
    RecvUpstream {
        #[source]
        source: io::Error,
        sock_addr: Option<SocketAddr>,
    },
    #[error("Failed to read response from upstream")]
    Header(#[from] HeaderError),
    #[error("Upstream responded with an error")]
    Response(ResponseError),
}
