use std::sync::Arc;

use common::{
    loading,
    proxy_runtime::{
        conn_handler::{ListenerBindError, udp::UdpProxyConnHandler},
        context::UdpRuntime,
    },
    udp_runtime::server::UdpServer,
};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UdpProxyServerConfig {
    pub listen_addr: Arc<str>,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
    #[serde(default)]
    pub allow_loopback: bool,
    /// Per-listener egress speed limit in bytes/s. `None` (the default) means
    /// unlimited. The limit is shared across all connections of a single
    /// listener (the `Limiter` is `Arc`-backed and cloned per connection).
    #[serde(default)]
    pub speed_limit: Option<f64>,
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
            self.config.speed_limit.unwrap_or(f64::INFINITY),
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
        connect::{ConnectorConfig, connector_config_cell},
        proxy_runtime::connect::udp::UdpConnector,
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
                speed_limit: None,
            },
            udp_context: UdpRuntime {
                session_table: None,
                time_validator: Arc::new(TimeValidator::new(Duration::from_secs(1))),
                connector: Arc::new(UdpConnector::new(
                    connector_config_cell(ConnectorConfig::default()).0,
                )),
                session_spawner: {
                    let (spawner, _rx) = common::session::SessionSpawner::channel();
                    spawner
                },
                retention: {
                    let (_actor, sender) = common::lifecycle::retention::RetentionActor::new();
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
