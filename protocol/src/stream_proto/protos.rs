use std::sync::Arc;

use common::{
    connect::{ConnectorConfigReader, ConnectorResetSignal},
    error::AnyResult,
    proxy_runtime::connect::{stream::StreamConnect, udp::UdpMuxDialer},
};

use super::{
    addr::ConcreteStreamType,
    connect::{
        build_kcp_connector, build_mptcp_connector, build_rtp_connector, build_rtp_mux_connector,
        build_tcp_connector, build_tcp_mux_connector,
    },
    streams::tcp::listener::TCP_STREAM_TYPE,
};

type StreamConnectorBuilder = fn(
    ConnectorConfigReader,
    ConnectorResetSignal,
    &mut tokio::task::JoinSet<AnyResult>,
) -> (Arc<dyn StreamConnect>, Option<Arc<dyn UdpMuxDialer>>);
type StreamProtoTable = [(ConcreteStreamType, &'static str, StreamConnectorBuilder)];
pub const STREAM_PROTOS: &StreamProtoTable = &[
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
];

#[cfg(test)]
mod tests {
    use super::*;

    /// `STREAM_PROTOS` and [`ConcreteStreamType::as_str`] are two authorities
    /// for the same wire/config name: the connector table is keyed by the
    /// former while every parsed route address carries the latter, so a
    /// drift between them leaves a configured stream protocol resolving to
    /// no connector at runtime. Each row's name is pinned literally here,
    /// and against `as_str`, so neither table can move alone. Do not
    /// "deduplicate" the literals by comparing `wire` to `ty.as_str()`
    /// alone -- a change applied to both tables together would vanish.
    #[test]
    fn every_stream_proto_row_names_its_concrete_stream_type() {
        let cases = [
            (ConcreteStreamType::Tcp, "tcp"),
            (ConcreteStreamType::TcpMux, "tcpmux"),
            (ConcreteStreamType::Kcp, "kcp"),
            (ConcreteStreamType::Mptcp, "mptcp"),
            (ConcreteStreamType::Rtp, "rtp"),
            (ConcreteStreamType::RtpMux, "rtpmux"),
        ];
        assert_eq!(STREAM_PROTOS.len(), cases.len());
        for ((ty, wire, _), (expected_ty, expected_wire)) in STREAM_PROTOS.iter().zip(cases) {
            assert_eq!(*ty, expected_ty, "row order");
            assert_eq!(*wire, expected_wire, "{ty:?} wire name");
            assert_eq!(ty.as_str(), expected_wire, "{ty:?} as_str drift");
        }
    }
}
