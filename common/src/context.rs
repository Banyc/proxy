use crate::{
    stream::{AsConn, connect::StreamTimedConnect, context::StreamContext},
    udp::context::UdpContext,
};

#[derive(Debug)]
pub struct Context<ConnectorTable> {
    pub stream: StreamContext<ConnectorTable>,
    pub udp: UdpContext,
}
impl<ConnectorTable> Clone for Context<ConnectorTable>
where
    ConnectorTable: StreamTimedConnect<Conn = Box<dyn AsConn>>,
{
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
            udp: self.udp.clone(),
        }
    }
}
