use std::{
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};

use common::{
    connect::ConnectorConfig,
    stream::connect::{StreamConnectExt, StreamTimedConnect},
};

use super::{
    addr::ConcreteStreamType,
    connection::Conn,
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
impl StreamTimedConnect for ConcreteStreamConnectorTable {
    type Conn = Conn;
    async fn timed_connect(
        &self,
        stream_type: &str,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Conn> {
        let stream_type: ConcreteStreamType = stream_type.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid stream type: `{stream_type}`"),
            )
        })?;
        match stream_type {
            ConcreteStreamType::Tcp => Ok(Conn::Tcp(IoTcpStream(
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
