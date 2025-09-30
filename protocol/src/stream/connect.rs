use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use common::{
    connect::{ConnectorConfig, ConnectorReset},
    proto::connect::stream::{StreamConnect, StreamConnectorTable},
};

use super::{
    registry::CONCRETE_STREAM_PROTO,
    streams::{
        kcp::KcpConnector, mptcp::MptcpConnector, rtp::RtpConnector, rtp_mux::RtpMuxConnector,
        tcp::proxy_server::TcpConnector, tcp_mux::TcpMuxConnector,
    },
};

pub fn build_concrete_stream_connector_table(
    config: ConnectorConfig,
    reset: ConnectorReset,
) -> StreamConnectorTable {
    let config = Arc::new(RwLock::new(config));
    let init: Vec<(&'static str, Arc<dyn StreamConnect>)> = CONCRETE_STREAM_PROTO
        .iter()
        .map(|(_, ty, build)| {
            let connector = build(config.clone(), reset.clone());
            (*ty, connector)
        })
        .collect();
    let connectors = HashMap::from_iter(init.into_iter().map(|(k, v)| (k.into(), v)));
    StreamConnectorTable::new(config, connectors)
}

pub fn build_tcp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(TcpConnector::new(config.clone()))
}
pub fn build_tcp_mux_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(TcpMuxConnector::new(config.clone(), reset))
}
pub fn build_kcp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(KcpConnector::new(config.clone()))
}
pub fn build_mptcp_connector(
    _config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(MptcpConnector)
}
pub fn build_rtp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(RtpConnector::new(config.clone(), false))
}
pub fn build_rtp_mux_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(RtpMuxConnector::new(config.clone(), reset, false))
}
pub fn build_rtp_mux_fec_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorReset,
) -> Arc<dyn StreamConnect> {
    Arc::new(RtpMuxConnector::new(config.clone(), reset, true))
}
