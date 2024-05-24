use super::metrics::UdpSessionTable;

#[derive(Debug, Clone)]
pub struct UdpContext {
    pub session_table: Option<UdpSessionTable>,
}
