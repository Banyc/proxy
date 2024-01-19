use crate::{stream::concrete::context::StreamContext, udp::context::UdpContext};

#[derive(Debug, Clone)]
pub struct Context {
    pub stream: StreamContext,
    pub udp: UdpContext,
}
