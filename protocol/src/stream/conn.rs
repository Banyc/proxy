use std::net::SocketAddr;

use common::stream::AsConn;

use super::addr::ConcreteStreamAddr;

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Box<dyn AsConn>,
    pub addr: ConcreteStreamAddr,
    pub sock_addr: SocketAddr,
}
