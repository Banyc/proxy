use std::io;

use common::{crypto::XorCrypto, stream::tcp::TcpServer, tcp_pool::TcpPool};
use serde::Deserialize;

use super::StreamProxyServer;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct TcpProxyServerBuilder {
    pub listen_addr: String,
    pub header_xor_key: Vec<u8>,
    pub payload_xor_key: Option<Vec<u8>>,
    pub tcp_pool: Option<Vec<String>>,
}

impl TcpProxyServerBuilder {
    pub async fn build(self) -> io::Result<TcpServer<StreamProxyServer>> {
        let header_crypto = XorCrypto::new(self.header_xor_key);
        let payload_crypto = self.payload_xor_key.map(XorCrypto::new);
        let tcp_pool = TcpPool::new();
        if let Some(addrs) = self.tcp_pool {
            tcp_pool.add_many_queues(addrs.into_iter().map(|v| v.into()));
        }
        let tcp_proxy = StreamProxyServer::new(header_crypto, payload_crypto, tcp_pool);
        let server = tcp_proxy.build(self.listen_addr).await?;
        Ok(server)
    }
}
