use crate::{
    stream::{connect::StreamTypedConnect, context::StreamContext},
    udp::context::UdpContext,
};

#[derive(Debug)]
pub struct Context<Conn, ConnectorTable> {
    pub stream: StreamContext<Conn, ConnectorTable>,
    pub udp: UdpContext,
}
impl<Conn, ConnectorTable> Clone for Context<Conn, ConnectorTable>
where
    ConnectorTable: StreamTypedConnect<Conn = Conn>,
{
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
            udp: self.udp.clone(),
        }
    }
}
