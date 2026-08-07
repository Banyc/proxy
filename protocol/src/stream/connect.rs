use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use common::{
    connect::{ConnectorConfig, ConnectorResetSignal},
    error::AnyResult,
    proto::connect::stream::{StreamConnect, StreamConnectorTable},
};

use super::{
    protos::STREAM_PROTOS,
    streams::{
        kcp::KcpConnector, mptcp::MptcpConnector, rtp::RtpConnector, rtp_mux::RtpMuxConnector,
        tcp::proxy_server::TcpConnector, tcp_mux::TcpMuxConnector,
    },
};

/// Build the concrete [`StreamConnectorTable`] and spawn every connector
/// driver into the supplied `drivers` `JoinSet`.
///
/// The `drivers` set is the caller's actively-reaped set — typically the
/// server's `server_tasks`. Spawned connector drivers observe the
/// connector command loops and reset listeners; their completion is
/// surfaced by reaping `drivers`, and dropping it aborts them.
pub fn build_concrete_stream_connector_table(
    config: ConnectorConfig,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> StreamConnectorTable {
    let config = Arc::new(RwLock::new(config));
    let init: Vec<(&'static str, Arc<dyn StreamConnect>)> = STREAM_PROTOS
        .iter()
        .map(|(_, ty, build)| {
            let connector = build(config.clone(), reset.clone(), drivers);
            (*ty, connector)
        })
        .collect();
    let connectors = HashMap::from_iter(init.into_iter().map(|(k, v)| (k.into(), v)));
    StreamConnectorTable::new(config, connectors)
}

pub fn build_tcp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    Arc::new(TcpConnector::new(config.clone()))
}
pub fn build_tcp_mux_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    let (connector, driver) = TcpMuxConnector::new(config.clone(), reset);
    drivers.spawn(async move {
        driver.await;
        Ok(())
    });
    Arc::new(connector)
}
pub fn build_kcp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    Arc::new(KcpConnector::new(config.clone()))
}
pub fn build_mptcp_connector(
    _config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    Arc::new(MptcpConnector)
}
pub fn build_rtp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    Arc::new(RtpConnector::new(config.clone(), false))
}
pub fn build_rtp_mux_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    build_rtp_mux_connector_with_fec(config, reset, drivers, false)
}
pub fn build_rtp_mux_fec_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> Arc<dyn StreamConnect> {
    build_rtp_mux_connector_with_fec(config, reset, drivers, true)
}
fn build_rtp_mux_connector_with_fec(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
    fec: bool,
) -> Arc<dyn StreamConnect> {
    let (connector, driver) = RtpMuxConnector::new(config.clone(), reset, fec);
    drivers.spawn(async move {
        driver.await;
        Ok(())
    });
    Arc::new(connector)
}
