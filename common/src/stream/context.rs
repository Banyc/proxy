use std::sync::Arc;

use swap::Swap;
use tokio_conn_pool::ConnPool;

use crate::{anti_replay::ReplayValidator, stream::metrics::StreamSessionTable};

use super::{addr::StreamAddr, connect::StreamTimedConnect};

#[derive(Debug)]
pub struct StreamContext<Conn, ConnectorTable> {
    pub session_table: Option<StreamSessionTable>,
    pub pool: Swap<ConnPool<StreamAddr, Conn>>,
    pub connector_table: Arc<ConnectorTable>,
    pub replay_validator: Arc<ReplayValidator>,
}
impl<Conn, ConnectorTable> Clone for StreamContext<Conn, ConnectorTable>
where
    ConnectorTable: StreamTimedConnect<Conn = Conn>,
{
    fn clone(&self) -> Self {
        Self {
            session_table: self.session_table.clone(),
            pool: self.pool.clone(),
            connector_table: self.connector_table.clone(),
            replay_validator: self.replay_validator.clone(),
        }
    }
}
