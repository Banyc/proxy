use std::{collections::HashMap, io, net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use lazy_static::lazy_static;
use thiserror::Error;

use super::{
    addr::{StreamAddr, StreamType},
    pool::Pool,
    streams::{kcp::KcpConnector, tcp::TcpConnector},
    CreatedStream,
};

pub type ArcStreamConnect = Arc<dyn StreamConnect + Sync + Send>;

lazy_static! {
    pub static ref STREAM_CONNECTOR_TABLE: StreamConnectorTable = StreamConnectorTable::new();
}

#[derive(Debug)]
pub struct StreamConnectorTable {
    inner: HashMap<StreamType, ArcStreamConnect>,
}

impl StreamConnectorTable {
    fn new() -> Self {
        let mut inner = HashMap::new();
        inner.insert(StreamType::Tcp, Arc::new(TcpConnector) as _);
        inner.insert(StreamType::Kcp, Arc::new(KcpConnector) as _);
        Self { inner }
    }

    pub fn get(&self, stream_type: StreamType) -> Option<&ArcStreamConnect> {
        self.inner.get(&stream_type)
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
            let connector = STREAM_CONNECTOR_TABLE.get(addr.stream_type).unwrap();
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
    #[error("Failed to resolve address")]
    ResolveAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
    },
    #[error("Refused to connect to loopback address")]
    Loopback {
        addr: StreamAddr,
        sock_addr: SocketAddr,
    },
    #[error("Failed to connect to address")]
    ConnectAddr {
        #[source]
        source: io::Error,
        addr: StreamAddr,
        sock_addr: SocketAddr,
    },
}
