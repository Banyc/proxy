use swap::Swap;
use tokio_conn_pool::ConnPool;

use crate::stream::metrics::StreamSessionTable;

use super::{addr::StreamAddr, connect::StreamConnectorTable};

#[derive(Debug)]
pub struct StreamContext<C, CT, ST> {
    pub session_table: Option<StreamSessionTable<ST>>,
    pub pool: Swap<ConnPool<StreamAddr<ST>, C>>,
    pub connector_table: CT,
}
impl<C, CT, ST> Clone for StreamContext<C, CT, ST>
where
    CT: StreamConnectorTable<Connection = C, StreamType = ST>,
{
    fn clone(&self) -> Self {
        Self {
            session_table: self.session_table.clone(),
            pool: self.pool.clone(),
            connector_table: self.connector_table.clone(),
        }
    }
}
