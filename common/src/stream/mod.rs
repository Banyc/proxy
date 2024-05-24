use std::{io, net::SocketAddr};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::loading;

use self::addr::StreamAddr;

pub mod addr;
pub mod connect;
pub mod context;
pub mod header;
pub mod io_copy;
pub mod log;
pub mod metrics;
pub mod pool;
pub mod steer;
pub mod xor;

pub trait IoStream:
    std::fmt::Debug + AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static
{
}

pub trait IoAddr {
    fn peer_addr(&self) -> io::Result<SocketAddr>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

pub trait StreamServerHook: loading::Hook {
    fn handle_stream<S>(&self, stream: S) -> impl std::future::Future<Output = ()> + Send
    where
        S: IoStream + IoAddr + std::fmt::Debug;
}
