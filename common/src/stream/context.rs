use std::sync::Arc;

use swap::Swap;
use tokio_conn_pool::ConnPool;

use crate::{anti_replay::ReplayValidator, stream::metrics::StreamSessionTable};

use super::{addr::StreamAddr, connect::StreamTypedConnect};

#[derive(Debug)]
pub struct StreamContext<Conn, ConnectorTable, StreamType> {
    pub session_table: Option<StreamSessionTable<StreamType>>,
    pub pool: Swap<ConnPool<StreamAddr<StreamType>, Conn>>,
    pub connector_table: Arc<ConnectorTable>,
    pub replay_validator: Arc<ReplayValidator>,
}
impl<Conn, ConnectorTable, StreamType> Clone for StreamContext<Conn, ConnectorTable, StreamType>
where
    ConnectorTable: StreamTypedConnect<Conn = Conn, StreamType = StreamType>,
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
