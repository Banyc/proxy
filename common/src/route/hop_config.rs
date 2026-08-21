use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::proxy_runtime::addr::RouteAddrStr;

#[derive(Debug, Clone, Serialize)]
pub struct HopConfig {
    pub address: crate::proxy_runtime::addr::RouteAddr,
    pub header_crypto: tokio_chacha20::config::Config,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}
impl<'de> Deserialize<'de> for HopConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct HopConfigSeed {
            address: RouteAddrStr,
            header_key: String,
            payload_key: Option<String>,
        }
        let HopConfigSeed {
            address,
            header_key,
            payload_key,
        } = HopConfigSeed::deserialize(deserializer)?;
        let header_crypto = tokio_chacha20::config::ConfigBuilder(header_key)
            .build()
            .map_err(|e| {
                D::Error::custom(HopConfigBuildError::HeaderCrypto(e.source.to_string()))
            })?;
        let payload_crypto = payload_key
            .map(|p| tokio_chacha20::config::ConfigBuilder(p).build())
            .transpose()
            .map_err(|e| {
                D::Error::custom(HopConfigBuildError::PayloadCrypto(e.source.to_string()))
            })?;
        Ok(HopConfig {
            address: address.0,
            header_crypto,
            payload_crypto,
        })
    }
}
#[derive(Debug, Error)]
pub enum HopConfigBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(String),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    const BAD_KEY: &str = "c2VjcmV0LXByb3h5LWtleQ!!";
    fn build(header: &str, payload: &str) -> String {
        let src = format!(
            r#"{{"address": "tcp://127.0.0.1:1", "header_key": "{header}", "payload_key": "{payload}"}}"#
        );
        let e = serde_json::from_str::<HopConfig>(&src).unwrap_err();
        format!("{e}")
    }
    #[test]
    fn a_key_that_fails_to_decode_is_not_repeated_back_into_the_log() {
        let e = build(BAD_KEY, "aGVsbG8");
        assert!(!e.contains("c2VjcmV0"), "{e}");
    }
    #[test]
    fn a_bad_payload_key_is_not_reported_as_a_bad_header_key() {
        let e = build("aGVsbG8", BAD_KEY);
        assert!(e.contains("PayloadCrypto"), "{e}");
        assert!(!e.contains("HeaderCrypto"), "{e}");
    }
}
