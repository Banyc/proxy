use std::net::SocketAddr;

use super::{AsConn, addr::StreamAddr};

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Box<dyn AsConn>,
    pub addr: StreamAddr,
    pub sock_addr: SocketAddr,
}
