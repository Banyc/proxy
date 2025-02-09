use std::sync::Arc;

use crate::anti_replay::TimeValidator;

use super::{connect::UdpConnector, metrics::UdpSessionTable};

#[derive(Debug, Clone)]
pub struct UdpContext {
    pub session_table: Option<UdpSessionTable>,
    pub time_validator: Arc<TimeValidator>,
    pub connector: Arc<UdpConnector>,
}
