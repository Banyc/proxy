use std::{io, net::SocketAddr, time::Duration};

use common::stream::connect::{StreamConnectExt, StreamConnectorTable};

use super::{
    addr::ConcreteStreamType,
    connection::Connection,
    streams::{
        kcp::KcpConnector, mptcp::MptcpConnector, rtp::RtpConnector, tcp::TcpConnector,
        tcp_mux::TcpMuxConnector,
    },
};

#[derive(Debug)]
pub struct ConcreteStreamConnectorTable {
    tcp: TcpConnector,
    tcp_mux: TcpMuxConnector,
    kcp: KcpConnector,
    mptcp: MptcpConnector,
    rtp: RtpConnector,
}
impl ConcreteStreamConnectorTable {
    pub fn new() -> Self {
        Self {
            tcp: TcpConnector,
            tcp_mux: TcpMuxConnector::new(),
            kcp: KcpConnector,
            mptcp: MptcpConnector,
            rtp: RtpConnector,
        }
    }
}
impl StreamConnectorTable for ConcreteStreamConnectorTable {
    type Connection = Connection;
    type StreamType = ConcreteStreamType;

    async fn timed_connect(
        &self,
        stream_type: &ConcreteStreamType,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Connection> {
        match stream_type {
            ConcreteStreamType::Tcp => self.tcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::TcpMux => self.tcp_mux.timed_connect(addr, timeout).await,
            ConcreteStreamType::Kcp => self.kcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::Mptcp => self.mptcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::Rtp => self.rtp.timed_connect(addr, timeout).await,
        }
    }
}
impl Default for ConcreteStreamConnectorTable {
    fn default() -> Self {
        Self::new()
    }
}
