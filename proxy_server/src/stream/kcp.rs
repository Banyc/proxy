use std::io;

use common::stream::{
    kcp::{KcpConnector, KcpServer},
    pool::Pool,
    StreamConnector,
};
use serde::Deserialize;
use tokio::net::ToSocketAddrs;
use tokio_kcp::{KcpConfig, KcpListener};
use tracing::error;

use super::{StreamProxyServer, StreamProxyServerBuilder};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct KcpProxyServerBuilder {
    pub listen_addr: String,
    #[serde(flatten)]
    pub inner: StreamProxyServerBuilder,
}

impl KcpProxyServerBuilder {
    pub async fn build(self) -> io::Result<KcpServer<StreamProxyServer>> {
        let connector = StreamConnector::Kcp(KcpConnector);
        let pool = Pool::new();
        pool.set_connector(connector);
        let connector = StreamConnector::Kcp(KcpConnector);
        let stream_proxy = self.inner.build(pool, connector).await?;
        build_kcp_proxy_server(self.listen_addr, stream_proxy).await
    }
}

pub async fn build_kcp_proxy_server(
    listen_addr: impl ToSocketAddrs,
    stream_proxy: StreamProxyServer,
) -> io::Result<KcpServer<StreamProxyServer>> {
    let config = KcpConfig::default();
    let listener = KcpListener::bind(config, listen_addr)
        .await
        .inspect_err(|e| error!(?e, "Failed to bind to listen address"))?;
    let server = KcpServer::new(listener, stream_proxy);
    Ok(server)
}
