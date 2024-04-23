use std::{io, net::SocketAddr, time::Duration};

use common::stream::connect::{StreamConnect, StreamConnectExt, StreamConnectorTable};

use super::{
    addr::ConcreteStreamType,
    connection::Connection,
    streams::{kcp::KcpConnector, mptcp::MptcpConnector, rtp::RtpConnector, tcp::TcpConnector},
};

#[derive(Debug, Clone)]
pub struct ConcreteStreamConnectorTable {
    tcp: TcpConnector,
    kcp: KcpConnector,
    mptcp: MptcpConnector,
    rtp: RtpConnector,
}

impl ConcreteStreamConnectorTable {
    pub fn new() -> Self {
        Self {
            tcp: TcpConnector,
            kcp: KcpConnector,
            mptcp: MptcpConnector,
            rtp: RtpConnector,
        }
    }
}

impl StreamConnectorTable for ConcreteStreamConnectorTable {
    type Connection = Connection;
    type StreamType = ConcreteStreamType;

    async fn connect(
        &self,
        stream_type: &ConcreteStreamType,
        addr: SocketAddr,
    ) -> io::Result<Connection> {
        match stream_type {
            ConcreteStreamType::Tcp => self.tcp.connect(addr).await,
            ConcreteStreamType::Kcp => self.kcp.connect(addr).await,
            ConcreteStreamType::Mptcp => self.mptcp.connect(addr).await,
            ConcreteStreamType::Rtp => self.rtp.connect(addr).await,
        }
    }

    async fn timed_connect(
        &self,
        stream_type: &ConcreteStreamType,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Connection> {
        match stream_type {
            ConcreteStreamType::Tcp => self.tcp.timed_connect(addr, timeout).await,
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
