use std::sync::{Arc, RwLock};

use common::{connect::ConnectorConfig, proto::connect::stream::StreamConnect};

use super::{
    addr::ConcreteStreamType,
    connect::{
        build_kcp_connector, build_mptcp_connector, build_rtp_connector, build_rtp_mux_connector,
        build_rtp_mux_fec_connector, build_tcp_connector, build_tcp_mux_connector,
    },
    streams::tcp::proxy_server::TCP_STREAM_TYPE,
};

type StreamProtoTable = [(
    ConcreteStreamType,
    &'static str,
    fn(Arc<RwLock<ConnectorConfig>>) -> Arc<dyn StreamConnect>,
)];
pub const CONCRETE_STREAM_PROTO: &StreamProtoTable = &[
    (
        ConcreteStreamType::Tcp,
        TCP_STREAM_TYPE,
        build_tcp_connector,
    ),
    (
        ConcreteStreamType::TcpMux,
        "tcpmux",
        build_tcp_mux_connector,
    ),
    (ConcreteStreamType::Kcp, "kcp", build_kcp_connector),
    (ConcreteStreamType::Mptcp, "mptcp", build_mptcp_connector),
    (ConcreteStreamType::Rtp, "rtp", build_rtp_connector),
    (
        ConcreteStreamType::RtpMux,
        "rtpmux",
        build_rtp_mux_connector,
    ),
    (
        ConcreteStreamType::RtpMuxFec,
        "rtpmuxfec",
        build_rtp_mux_fec_connector,
    ),
];
