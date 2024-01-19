use super::session_table::UdpSessionTable;

#[derive(Debug, Clone)]
pub struct UdpContext {
    pub session_table: Option<UdpSessionTable>,
}
