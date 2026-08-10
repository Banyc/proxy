use std::sync::Arc;

use common::{
    loading,
    proto::{
        conn_handler::{ListenerBindError, udp::UdpProxyConnHandler},
        context::UdpRuntime,
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
    #[serde(default)]
    pub allow_loopback: bool,
}

#[derive(Debug, Clone)]
pub struct UdpProxyServerBuilder {
    pub config: UdpProxyServerConfig,
    pub udp_context: UdpRuntime,
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
            .map_err(|e| UdpProxyBuildError::HeaderCrypto(e.source.to_string()))?;
        let payload_crypto = match self.config.payload_key {
            Some(payload_crypto) => Some(
                payload_crypto
                    .build()
                    .map_err(|e| UdpProxyBuildError::PayloadCrypto(e.source.to_string()))?,
            ),
            None => None,
        };
        Ok(UdpProxyConnHandler::new(
            header_crypto,
            payload_crypto,
            self.udp_context,
            self.config.allow_loopback,
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
    HeaderCrypto(String),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(String),
}
#[cfg(test)]
mod tests {
    use super::*;
    use ae::anti_replay::TimeValidator;
    use common::{
        connect::{ConnectorConfig, ConnectorConfigHandle},
        proto::connect::udp::UdpConnector,
    };
    use std::time::Duration;

    #[test]
    fn a_bad_payload_key_is_not_reported_as_a_bad_header_key() {
        let builder = UdpProxyServerBuilder {
            config: UdpProxyServerConfig {
                listen_addr: "127.0.0.1:0".into(),
                header_key: tokio_chacha20::config::ConfigBuilder("aGVsbG8".to_owned()),
                payload_key: Some(tokio_chacha20::config::ConfigBuilder(
                    "c2VjcmV0LXByb3h5LWtleQ!!".to_owned(),
                )),
                allow_loopback: false,
            },
            udp_context: UdpRuntime {
                session_table: None,
                time_validator: Arc::new(TimeValidator::new(Duration::from_secs(1))),
                connector: Arc::new(UdpConnector::new(ConnectorConfigHandle::new(
                    ConnectorConfig::default(),
                ))),
                session_spawner: {
                    let (spawner, _rx) = common::session::SessionSpawner::channel();
                    spawner
                },
                retention: {
                    let (_actor, sender) = common::retention::RetentionActor::new();
                    sender
                },
            },
        };
        let e = loading::Build::build_conn_handler(builder).unwrap_err();
        assert!(
            matches!(
                e,
                UdpProxyServerBuildError::Hook(UdpProxyBuildError::PayloadCrypto(_))
            ),
            "a bad payload key must be reported as a payload-key error"
        );
        assert!(!format!("{e}").contains("c2VjcmV0"), "{e}");
        assert!(!format!("{e:?}").contains("c2VjcmV0"), "{e:?}");
    }
}
