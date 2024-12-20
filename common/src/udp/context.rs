use std::sync::Arc;

use crate::anti_replay::ReplayValidator;

use super::metrics::UdpSessionTable;

#[derive(Debug, Clone)]
pub struct UdpContext {
    pub session_table: Option<UdpSessionTable>,
    pub replay_validator: Arc<ReplayValidator>,
}
