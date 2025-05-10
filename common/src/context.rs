use crate::{
    stream::{connect::StreamTypedConnect, context::StreamContext},
    udp::context::UdpContext,
};

#[derive(Debug)]
pub struct Context<Conn, ConnectorTable, StreamType> {
    pub stream: StreamContext<Conn, ConnectorTable, StreamType>,
    pub udp: UdpContext,
}
impl<Conn, ConnectorTable, StreamType> Clone for Context<Conn, ConnectorTable, StreamType>
where
    ConnectorTable: StreamTypedConnect<Conn = Conn, StreamType = StreamType>,
{
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
            udp: self.udp.clone(),
        }
    }
}
