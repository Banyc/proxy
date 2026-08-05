use std::sync::Arc;

use ae::anti_replay::{ReplayValidator, TimeValidator};
use swap::Swap;
use tokio_conn_pool::ConnPool;

use crate::{retention::RetentionActorSender, session::SessionSpawner, stream::ConnParts};

use super::{
    addr::RouteAddr,
    connect::{stream::StreamConnectorTable, udp::UdpConnector},
    metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
};

#[derive(Debug, Clone)]
pub struct Runtime {
    pub stream: StreamRuntime,
    pub udp: UdpRuntime,
    pub session_spawner: SessionSpawner,
}

#[derive(Debug, Clone)]
pub struct StreamRuntime {
    pub session_table: Option<StreamSessionTable>,
    pub pool: Swap<ConnPool<RouteAddr, Box<dyn ConnParts>>>,
    pub connector_table: Arc<StreamConnectorTable>,
    pub replay_validator: Arc<ReplayValidator>,
    pub session_spawner: SessionSpawner,
    pub retention: RetentionActorSender,
}

#[derive(Debug, Clone)]
pub struct UdpRuntime {
    pub session_table: Option<UdpSessionTable>,
    pub time_validator: Arc<TimeValidator>,
    pub connector: Arc<UdpConnector>,
    pub session_spawner: SessionSpawner,
    pub retention: RetentionActorSender,
}
