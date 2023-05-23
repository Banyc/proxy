use std::net::SocketAddr;

use crate::{header::InternetAddr, stream::CreatedStream};

#[derive(Debug, Clone)]
pub struct PersistentConnections {}

impl PersistentConnections {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn open_stream(&self, _addr: &InternetAddr) -> Option<(CreatedStream, SocketAddr)> {
        None
    }
}

impl Default for PersistentConnections {
    fn default() -> Self {
        Self::new()
    }
}
