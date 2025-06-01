use std::sync::Arc;

use ae::anti_replay::{ReplayValidator, TimeValidator};
use swap::Swap;
use tokio_conn_pool::ConnPool;

use crate::stream::AsConn;

use super::{
    addr::StreamAddr,
    connect::{stream::StreamConnectorTable, udp::UdpConnector},
    metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
};

#[derive(Debug, Clone)]
pub struct Context {
    pub stream: StreamContext,
    pub udp: UdpContext,
}

#[derive(Debug, Clone)]
pub struct StreamContext {
    pub session_table: Option<StreamSessionTable>,
    pub pool: Swap<ConnPool<StreamAddr, Box<dyn AsConn>>>,
    pub connector_table: Arc<StreamConnectorTable>,
    pub replay_validator: Arc<ReplayValidator>,
}

#[derive(Debug, Clone)]
pub struct UdpContext {
    pub session_table: Option<UdpSessionTable>,
    pub time_validator: Arc<TimeValidator>,
    pub connector: Arc<UdpConnector>,
}
