use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use common::{
    connect::ConnectorConfig,
    proto::connect::stream::{StreamConnect, StreamConnectorTable},
};

use super::streams::{
    kcp::KcpConnector, mptcp::MptcpConnector, rtp::RtpConnector, rtp_mux::RtpMuxConnector,
    tcp::TcpConnector, tcp_mux::TcpMuxConnector,
};

pub const TCP_STREAM_TYPE: &str = "tcp";

pub fn build_concrete_stream_connector_table(config: ConnectorConfig) -> StreamConnectorTable {
    let config = Arc::new(RwLock::new(config));
    let init: Vec<(&'static str, Arc<dyn StreamConnect>)> = vec![
        (TCP_STREAM_TYPE, Arc::new(TcpConnector::new(config.clone()))),
        ("tcpmux", Arc::new(TcpMuxConnector::new(config.clone()))),
        ("kcp", Arc::new(KcpConnector::new(config.clone()))),
        ("mptcp", Arc::new(MptcpConnector)),
        ("rtp", Arc::new(RtpConnector::new(config.clone(), false))),
        (
            "rtpmux",
            Arc::new(RtpMuxConnector::new(config.clone(), false)),
        ),
        (
            "rtpmuxfec",
            Arc::new(RtpMuxConnector::new(config.clone(), true)),
        ),
    ];
    let connectors = HashMap::from_iter(init.into_iter().map(|(k, v)| (k.into(), v)));
    StreamConnectorTable::new(config, connectors)
}
