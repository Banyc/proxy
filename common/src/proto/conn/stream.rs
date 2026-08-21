use std::net::SocketAddr;

use crate::{proto::addr::RouteAddr, stream::IoConnection};

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Box<dyn IoConnection>,
    pub addr: RouteAddr,
    pub sock_addr: SocketAddr,
}
