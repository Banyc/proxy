use std::{io, net::SocketAddr, time::Duration};

pub trait StreamConnect: std::fmt::Debug {
    type Connection;

    fn connect(
        &self,
        addr: SocketAddr,
    ) -> impl std::future::Future<Output = io::Result<Self::Connection>> + Send;
}

pub trait StreamConnectExt: StreamConnect {
    fn timed_connect(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl std::future::Future<Output = io::Result<Self::Connection>> + Send
    where
        Self: Sync,
    {
        async move {
            let res = tokio::time::timeout(timeout, self.connect(addr)).await;
            match res {
                Ok(res) => res,
                Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "Timed out")),
            }
        }
    }
}
impl<T: StreamConnect> StreamConnectExt for T {}

pub trait StreamConnectorTable: std::fmt::Debug + Clone + Sync + Send + 'static {
    type Connection;
    type StreamType;

    fn timed_connect(
        &self,
        stream_type: &Self::StreamType,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl std::future::Future<Output = io::Result<Self::Connection>> + Send;
}
