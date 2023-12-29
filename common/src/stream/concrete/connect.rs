use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use super::created_stream::CreatedStream;

pub type ArcStreamConnect = Arc<dyn StreamConnect + Sync + Send>;

pub trait StreamConnect: std::fmt::Debug {
    fn connect(
        &self,
        addr: SocketAddr,
    ) -> impl std::future::Future<Output = io::Result<CreatedStream>> + Send;
}

pub trait StreamConnectExt: StreamConnect {
    fn timed_connect(
        &self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> impl std::future::Future<Output = io::Result<CreatedStream>> + Send
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
