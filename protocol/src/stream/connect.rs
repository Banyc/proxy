use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use common::{
    connect::ConnectorConfig,
    stream::connect::{StreamConnectExt, StreamTypedConnect},
};

use super::{
    addr::ConcreteStreamType,
    connection::Connection,
    streams::{
        kcp::KcpConnector,
        mptcp::MptcpConnector,
        rtp::RtpConnector,
        rtp_mux::RtpMuxConnector,
        tcp::{IoTcpStream, TcpConnector},
        tcp_mux::TcpMuxConnector,
    },
};

#[derive(Debug)]
pub struct ConcreteStreamConnectorTable {
    config: Arc<RwLock<ConnectorConfig>>,
    tcp: TcpConnector,
    tcp_mux: TcpMuxConnector,
    kcp: KcpConnector,
    mptcp: MptcpConnector,
    rtp: RtpConnector,
    rtp_mux: RtpMuxConnector,
    rtp_mux_fec: RtpMuxConnector,
}
impl ConcreteStreamConnectorTable {
    pub fn new(config: ConnectorConfig) -> Self {
        let config = Arc::new(RwLock::new(config));
        Self {
            tcp: TcpConnector::new(config.clone()),
            tcp_mux: TcpMuxConnector::new(config.clone()),
            kcp: KcpConnector::new(config.clone()),
            mptcp: MptcpConnector,
            rtp: RtpConnector::new(config.clone(), false),
            rtp_mux: RtpMuxConnector::new(config.clone(), false),
            rtp_mux_fec: RtpMuxConnector::new(config.clone(), true),
            config,
        }
    }
    pub fn tcp(&self) -> &TcpConnector {
        &self.tcp
    }
    pub fn replaced_by(&self, config: ConnectorConfig) {
        *self.config.write().unwrap() = config;
    }
}
impl StreamTypedConnect for ConcreteStreamConnectorTable {
    type Connection = Connection;
    type StreamType = ConcreteStreamType;

    async fn timed_connect(
        &self,
        stream_type: &ConcreteStreamType,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Connection> {
        match stream_type {
            ConcreteStreamType::Tcp => Ok(Connection::Tcp(IoTcpStream(
                self.tcp.timed_connect(addr, timeout).await?,
            ))),
            ConcreteStreamType::TcpMux => self.tcp_mux.timed_connect(addr, timeout).await,
            ConcreteStreamType::Kcp => self.kcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::Mptcp => self.mptcp.timed_connect(addr, timeout).await,
            ConcreteStreamType::Rtp => self.rtp.timed_connect(addr, timeout).await,
            ConcreteStreamType::RtpMux => self.rtp_mux.timed_connect(addr, timeout).await,
            ConcreteStreamType::RtpMuxFec => self.rtp_mux_fec.timed_connect(addr, timeout).await,
        }
    }
}
