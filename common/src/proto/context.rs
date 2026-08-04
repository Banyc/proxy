use std::sync::Arc;

use ae::anti_replay::{ReplayValidator, TimeValidator};
use swap::Swap;
use tokio_conn_pool::ConnPool;

use crate::stream::ConnParts;

use super::{
    addr::RouteAddr,
    connect::{stream::StreamConnectorTable, udp::UdpConnector},
    metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
};

#[derive(Debug, Clone)]
pub struct Runtime {
    pub stream: StreamRuntime,
    pub udp: UdpRuntime,
}

#[derive(Debug, Clone)]
pub struct StreamRuntime {
    pub session_table: Option<StreamSessionTable>,
    pub pool: Swap<ConnPool<RouteAddr, Box<dyn ConnParts>>>,
    pub connector_table: Arc<StreamConnectorTable>,
    pub replay_validator: Arc<ReplayValidator>,
}

#[derive(Debug, Clone)]
pub struct UdpRuntime {
    pub session_table: Option<UdpSessionTable>,
    pub time_validator: Arc<TimeValidator>,
    pub connector: Arc<UdpConnector>,
}
