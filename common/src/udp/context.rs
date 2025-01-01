use std::sync::Arc;

use crate::anti_replay::TimeValidator;

use super::metrics::UdpSessionTable;

#[derive(Debug, Clone)]
pub struct UdpContext {
    pub session_table: Option<UdpSessionTable>,
    pub time_validator: Arc<TimeValidator>,
}
