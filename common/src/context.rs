use crate::{
    stream::{connect::StreamTypedConnect, context::StreamContext},
    udp::context::UdpContext,
};

#[derive(Debug)]
pub struct Context<C, CT, ST> {
    pub stream: StreamContext<C, CT, ST>,
    pub udp: UdpContext,
}
impl<C, CT, ST> Clone for Context<C, CT, ST>
where
    CT: StreamTypedConnect<Connection = C, StreamType = ST>,
{
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
            udp: self.udp.clone(),
        }
    }
}
