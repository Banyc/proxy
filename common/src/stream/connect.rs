use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use thiserror::Error;

use crate::stream::streams::mptcp::MptcpConnector;

use super::{
    addr::{StreamAddr, StreamType},
    pool::Pool,
    streams::{kcp::KcpConnector, tcp::TcpConnector},
    CreatedStream,
};

pub type ArcStreamConnect = Arc<dyn StreamConnect + Sync + Send>;

pub static STREAM_CONNECTOR_TABLE: Lazy<StreamConnectorTable> =
    Lazy::new(StreamConnectorTable::new);

#[derive(Debug)]
pub struct StreamConnectorTable {
    inner: enum_map::EnumMap<StreamType, ArcStreamConnect>,
}

impl StreamConnectorTable {
    fn new() -> Self {
        let inner = enum_map::enum_map! {
            StreamType::Tcp => Arc::new(TcpConnector) as _,
            StreamType::Kcp => Arc::new(KcpConnector) as _,
            StreamType::Mptcp => Arc::new(MptcpConnector) as _,
        };
        Self { inner }
    }

    pub fn get(&self, stream_type: StreamType) -> &ArcStreamConnect {
        &self.inner[stream_type]
    }
}

impl Default for StreamConnectorTable {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
pub trait StreamConnect: std::fmt::Debug {
    async fn connect(&self, addr: SocketAddr) -> io::Result<CreatedStream>;
    async fn timed_connect(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<CreatedStream> {
        let res = tokio::time::timeout(timeout, self.connect(addr)).await;
        match res {
            Ok(res) => res,
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "Timed out")),
        }
    }
}

pub async fn connect_with_pool(
    addr: &StreamAddr,
    stream_pool: &Pool,
    allow_loopback: bool,
    timeout: Duration,
) -> Result<(CreatedStream, SocketAddr), ConnectError> {
    let stream = stream_pool.open_stream(addr).await;
    let ret = match stream {
        Some((stream, sock_addr)) => (stream, sock_addr),
        None => {
            let connector = STREAM_CONNECTOR_TABLE.get(addr.stream_type);
            let sock_addr =
                addr.address
                    .to_socket_addr()
                    .await
                    .map_err(|e| ConnectError::ResolveAddr {
                        source: e,
                        addr: addr.clone(),
                    })?;
            if !allow_loopback && sock_addr.ip().is_loopback() {
                // Prevent connections to localhost
                return Err(ConnectError::Loopback {
                    addr: addr.clone(),
                    sock_addr,
                });
            }
            let stream = connector
                .timed_connect(sock_addr, timeout)
                .await
                .map_err(|e| ConnectError::ConnectAddr {
                    source: e,
                    addr: addr.clone(),
                    sock_addr,
                })?;
            (stream, sock_addr)
        }
    };
    Ok(ret)
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("Failed to resolve address: {source}, {addr}")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
    },
    #[error("Refused to connect to loopback address: {addr}, {sock_addr}")]
    Loopback {
        addr: StreamAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address: {source}, {addr}, {sock_addr}")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
        sock_addr: SocketAddr,
    },
}
