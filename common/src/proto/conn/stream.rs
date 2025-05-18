use std::net::SocketAddr;

use crate::{proto::addr::StreamAddr, stream::AsConn};

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Box<dyn AsConn>,
    pub addr: StreamAddr,
    pub sock_addr: SocketAddr,
}
