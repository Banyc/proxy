use std::net::SocketAddr;

use crate::{proto::addr::RouteAddr, stream::ConnParts};

#[derive(Debug)]
pub struct ConnAndAddr {
    pub stream: Box<dyn ConnParts>,
    pub addr: RouteAddr,
    pub sock_addr: SocketAddr,
}
