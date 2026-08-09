use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use common::{
    connect::{ConnectorConfig, ConnectorResetSignal},
    error::{AnyError, AnyResult},
    proto::connect::{
        stream::{StreamConnect, StreamConnectorTable},
        udp::{UdpConnector, UdpMuxDialer},
    },
};

use super::{
    protos::STREAM_PROTOS,
    streams::{
        kcp::KcpConnector, mptcp::MptcpConnector, mux::MuxConnectorDriver, rtp::RtpConnector,
        rtp_mux::RtpMuxConnector, tcp::proxy_server::TcpConnector, tcp_mux::TcpMuxConnector,
    },
};

/// Build the concrete [`StreamConnectorTable`] and spawn every connector
/// driver into the supplied `drivers` `JoinSet`.
///
/// The `drivers` set is the caller's actively-reaped set — typically the
/// server's `server_tasks`. Spawned connector drivers observe the
/// connector command loops and reset listeners; their completion is
/// surfaced by reaping `drivers`, and dropping it aborts them.
///
/// The mux connectors (`tcpmux`/`rtpmux`/`rtpmuxfec`) double as
/// [`UdpMuxDialer`]s: they are registered into `udp_connector` so UDP proxy
/// chains can open datagram flows over the same mux sessions, using the
/// same wire format as reverse tunneling.
pub fn build_concrete_stream_connector_table(
    config: ConnectorConfig,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
    udp_connector: &UdpConnector,
) -> StreamConnectorTable {
    let config = Arc::new(RwLock::new(config));
    let init: Vec<(&'static str, Arc<dyn StreamConnect>)> = STREAM_PROTOS
        .iter()
        .map(|(_, ty, build)| {
            let (connector, dialer) = build(config.clone(), reset.clone(), drivers);
            if let Some(dialer) = dialer {
                udp_connector.register_dialer((*ty).into(), dialer);
            }
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
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    (Arc::new(TcpConnector::new(config.clone())), None)
}
pub fn build_tcp_mux_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    let (connector, driver) = TcpMuxConnector::new(config.clone(), reset);
    spawn_mux_connector(connector, driver, drivers)
}
pub fn build_kcp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    (Arc::new(KcpConnector::new(config.clone())), None)
}
pub fn build_mptcp_connector(
    _config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    (Arc::new(MptcpConnector), None)
}
pub fn build_rtp_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    _reset: ConnectorResetSignal,
    _drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    (Arc::new(RtpConnector::new(config.clone(), false)), None)
}
pub fn build_rtp_mux_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    build_rtp_mux_connector_with_fec(config, reset, drivers, false)
}
pub fn build_rtp_mux_fec_connector(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    build_rtp_mux_connector_with_fec(config, reset, drivers, true)
}
fn build_rtp_mux_connector_with_fec(
    config: Arc<RwLock<ConnectorConfig>>,
    reset: ConnectorResetSignal,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
    fec: bool,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>) {
    let (connector, driver) = RtpMuxConnector::new(config.clone(), reset, fec);
    spawn_mux_connector(connector, driver, drivers)
}

/// Wrap a mux connector (handle + driver) as the stream connector and UDP
/// dialer pair, spawning the driver into the actively-reaped `drivers` set.
///
/// A connector driver exiting is fatal — the connector is inert. Surface
/// the typed error instead of converting completion to `Ok(())`, so
/// `server_tasks` tears the server down rather than continuing. The single
/// connector instance is shared by both the stream and UDP paths, so both
/// kinds of flows reuse the same mux sessions.
fn spawn_mux_connector<C>(
    connector: C,
    driver: MuxConnectorDriver,
    drivers: &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>)
where
    C: StreamConnect + UdpMuxDialer + 'static,
{
    let connector = Arc::new(connector);
    drivers.spawn(async move {
        let error = driver.await;
        Err(Box::new(error) as AnyError)
    });
    (
        Arc::clone(&connector) as Arc<dyn StreamConnect>,
        Some(connector as Arc<dyn UdpMuxDialer>),
    )
}
