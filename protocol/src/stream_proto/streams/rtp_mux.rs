use common::{
    loading,
    proxy_runtime::{
        conn_handler::{
            ListenerBindError,
            stream::{
                StreamProxyConnHandlerBuilder, StreamProxyConnHandlerConfig,
                StreamProxyServerBuildError,
            },
        },
        context::{Runtime, UdpRuntime},
    },
    session::SessionSpawner,
};
use serde::Deserialize;
use std::sync::Arc;
use thiserror::Error;
use tokio::net::ToSocketAddrs;
mod connector;
mod server;
pub use super::mux::{ConnectorDriverError, MuxConnectorDriver};
use crate::stream_proto::streams::mux::{
    MuxProxyHandler, MuxProxyUdpBuildError, build_udp_proxy_handler,
};
pub use connector::RtpMuxConnector;
pub use server::{RtpMuxServer, ServeError};
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtpMuxProxyServerConfig {
    pub listen_addr: Arc<str>,
    /// Optional datagram obfuscation key: when set, every RTP datagram is
    /// prefixed with a 24-byte random nonce and chacha20-encrypted with this
    /// key. The peer connector must use the same key; `None` (the default)
    /// sends datagrams in the clear.
    #[serde(default)]
    pub obfuscation_key: Option<[u8; 32]>,
    #[serde(flatten)]
    pub inner: StreamProxyConnHandlerConfig,
}
impl RtpMuxProxyServerConfig {
    pub fn into_builder(self, runtime: Runtime) -> RtpMuxProxyServerBuilder {
        let listen_addr = Arc::clone(&self.listen_addr);
        let inner = self.inner.into_builder(runtime.stream, listen_addr);
        RtpMuxProxyServerBuilder {
            listen_addr: self.listen_addr,
            obfuscation_key: self.obfuscation_key,
            inner,
            udp_context: runtime.udp,
        }
    }
}
#[derive(Debug, Clone)]
pub struct RtpMuxProxyServerBuilder {
    pub listen_addr: Arc<str>,
    pub obfuscation_key: Option<[u8; 32]>,
    pub inner: StreamProxyConnHandlerBuilder,
    pub udp_context: UdpRuntime,
}
impl loading::Build for RtpMuxProxyServerBuilder {
    type ConnHandler = MuxProxyHandler;
    type Server = RtpMuxServer<Self::ConnHandler>;
    type Err = RtpMuxProxyServerBuildError;
    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let session_spawner = self.inner.stream_context.session_spawner.clone();
        let obfuscation_key = self.obfuscation_key;
        let handler = self.build_conn_handler()?;
        build_rtp_mux_proxy_server(
            listen_addr.as_ref(),
            handler,
            obfuscation_key,
            session_spawner,
        )
        .await
        .map_err(Into::into)
    }
    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        let stream = self
            .inner
            .clone()
            .build()
            .map_err(RtpMuxProxyServerBuildError::Hook)?;
        let udp = build_udp_proxy_handler(
            self.inner.header_key,
            self.inner.payload_key,
            self.udp_context,
            self.inner.allow_loopback,
        )
        .map_err(RtpMuxProxyServerBuildError::Udp)?;
        Ok(MuxProxyHandler { stream, udp })
    }
    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }
}
#[derive(Debug, Error)]
pub enum RtpMuxProxyServerBuildError {
    #[error("{0}")]
    Hook(#[from] StreamProxyServerBuildError),
    #[error("{0}")]
    Udp(#[from] MuxProxyUdpBuildError),
    #[error("{0}")]
    Server(#[from] ListenerBindError),
}
pub async fn build_rtp_mux_proxy_server(
    listen_addr: impl ToSocketAddrs + Clone + std::fmt::Debug,
    handler: MuxProxyHandler,
    obfuscation_key: Option<[u8; 32]>,
    session_spawner: SessionSpawner,
) -> Result<RtpMuxServer<MuxProxyHandler>, ListenerBindError> {
    let server = ::rtp_mux::RtpMuxServer::bind(listen_addr)
        .await
        .map_err(ListenerBindError)?
        .with_obfuscation_key(obfuscation_key.map(::rtp_mux::ObfuscationKey::from_bytes));
    Ok(RtpMuxServer::from_core(server, handler, session_spawner))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_rejects_removed_fec_option() {
        let error = serde_json::from_str::<RtpMuxProxyServerConfig>(r#"{ "listen_addr": "127.0.0.1:7000", "header_key": "aGVsbG8", "payload_key": null, "fec": true }"#)
            .unwrap_err();
        assert!(error.to_string().contains("fec"), "{error}");
    }

    #[test]
    fn server_config_accepts_an_optional_obfuscation_key() {
        let key = [7; 32];
        let config = serde_json::from_str::<RtpMuxProxyServerConfig>(&format!(
            r#"{{ "listen_addr": "127.0.0.1:7000", "header_key": "aGVsbG8", "obfuscation_key": {key:?} }}"#
        ))
        .unwrap();
        assert_eq!(config.obfuscation_key, Some(key));
        let plain = serde_json::from_str::<RtpMuxProxyServerConfig>(
            r#"{ "listen_addr": "127.0.0.1:7000", "header_key": "aGVsbG8" }"#,
        )
        .unwrap();
        assert_eq!(plain.obfuscation_key, None);
    }
}
