use std::{io, net::SocketAddr, time::Duration};

pub trait StreamConnect: std::fmt::Debug {
    type Conn;
    fn connect(
        &self,
        addr: SocketAddr,
    ) -> impl std::future::Future<Output = io::Result<Self::Conn>> + Send;
}

pub trait StreamConnectExt: StreamConnect {
    fn timed_connect(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl std::future::Future<Output = io::Result<Self::Conn>> + Send
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

pub trait StreamTimedConnect: std::fmt::Debug + Sync + Send + 'static {
    type Conn;
    fn timed_connect(
        &self,
        stream_type: &str,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl std::future::Future<Output = io::Result<Self::Conn>> + Send;
}
