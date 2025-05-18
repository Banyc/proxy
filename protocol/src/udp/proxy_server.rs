use std::sync::Arc;

use common::{
    loading,
    proto::{
        conn_handler::{ListenerBindError, udp::UdpProxyConnHandler},
        context::UdpContext,
    },
    udp::server::UdpServer,
};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyServerConfig {
    pub listen_addr: Arc<str>,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}

#[derive(Debug, Clone)]
pub struct UdpProxyServerBuilder {
    pub config: UdpProxyServerConfig,
    pub udp_context: UdpContext,
}
impl loading::Build for UdpProxyServerBuilder {
    type ConnHandler = UdpProxyConnHandler;
    type Server = UdpServer<Self::ConnHandler>;
    type Err = UdpProxyServerBuildError;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = Arc::clone(&self.config.listen_addr);
        let udp_proxy = self.build_conn_handler()?;
        let server = udp_proxy.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let header_crypto = self
            .config
            .header_key
            .build()
            .map_err(UdpProxyBuildError::HeaderCrypto)?;
        let payload_crypto = match self.config.payload_key {
            Some(payload_crypto) => Some(
                payload_crypto
                    .build()
                    .map_err(UdpProxyBuildError::HeaderCrypto)?,
            ),
            None => None,
        };
        Ok(UdpProxyConnHandler::new(
            header_crypto,
            payload_crypto,
            self.udp_context,
        ))
    }

    fn key(&self) -> &Arc<str> {
        &self.config.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum UdpProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] UdpProxyBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
#[derive(Debug, Error)]
pub enum UdpProxyBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(#[source] tokio_chacha20::config::ConfigBuildError),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(#[source] tokio_chacha20::config::ConfigBuildError),
}
