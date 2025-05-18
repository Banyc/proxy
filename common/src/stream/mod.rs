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

pub trait OwnIoStream:
    std::fmt::Debug + AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static
{
}

pub trait HasIoAddr {
    fn peer_addr(&self) -> io::Result<SocketAddr>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

pub trait AsConn: OwnIoStream + HasIoAddr {}

pub trait StreamServerHandleConn: loading::HandleConn {
    fn handle_stream<Stream>(&self, stream: Stream) -> impl std::future::Future<Output = ()> + Send
    where
        Stream: AsConn + std::fmt::Debug;
}
