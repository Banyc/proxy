use crate::stream::session_table::StreamSessionTable;

use super::{addr::ConcreteStreamType, pool::SharedConcreteConnPool};

#[derive(Debug, Clone)]
pub struct StreamContext {
    pub session_table: Option<StreamSessionTable<ConcreteStreamType>>,
    pub pool: SharedConcreteConnPool,
}
