use std::{collections::HashMap, net::SocketAddr};

use quinn::{Connection, RecvStream, SendStream};

use crate::header::InternetAddr;

#[derive(Debug, Clone)]
pub struct QuicPersistentConnections {
    map: HashMap<InternetAddr, (Connection, SocketAddr)>,
}

impl QuicPersistentConnections {
    pub fn new(map: HashMap<InternetAddr, (Connection, SocketAddr)>) -> Self {
        Self { map }
    }

    pub async fn open_stream(
        &self,
        key: &InternetAddr,
    ) -> Option<(SendStream, RecvStream, SocketAddr)> {
        let (conn, addr) = match self.map.get(key) {
            Some(x) => x,
            None => return None,
        };
        let (send, recv) = conn.open_bi().await.unwrap();
        Some((send, recv, *addr))
    }
}
