use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnConfigBuilder<AddrStr> {
    pub address: AddrStr,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}
impl<AddrStr> ConnConfigBuilder<AddrStr> {
    pub fn build<Addr>(self) -> Result<ConnConfig<Addr>, ConnConfigBuildError>
    where
        AddrStr: IntoAddr<Addr = Addr>,
    {
        let header_crypto = self
            .header_key
            .build()
            .map_err(|e| ConnConfigBuildError::HeaderCrypto(e.source.to_string()))?;
        let payload_crypto = self
            .payload_key
            .map(|p| p.build())
            .transpose()
            .map_err(|e| ConnConfigBuildError::PayloadCrypto(e.source.to_string()))?;
        let address = self.address.into_address();
        Ok(ConnConfig {
            address,
            header_crypto,
            payload_crypto,
        })
    }
}
#[derive(Debug, Error)]
pub enum ConnConfigBuildError {
    #[error("HeaderCrypto: {0}")]
    HeaderCrypto(String),
    #[error("PayloadCrypto: {0}")]
    PayloadCrypto(String),
}

pub trait IntoAddr: Serialize + DeserializeOwned {
    type Addr;
    fn into_address(self) -> Self::Addr;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ConnConfig<Addr> {
    pub address: Addr,
    pub header_crypto: tokio_chacha20::config::Config,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::InternetAddrStr;
    const BAD_KEY: &str = "c2VjcmV0LXByb3h5LWtleQ!!";
    fn build(header: &str, payload: &str) -> ConnConfigBuildError {
        ConnConfigBuilder {
            address: InternetAddrStr("127.0.0.1:1".parse().unwrap()),
            header_key: tokio_chacha20::config::ConfigBuilder(header.to_owned()),
            payload_key: Some(tokio_chacha20::config::ConfigBuilder(payload.to_owned())),
        }
        .build::<crate::addr::InternetAddr>()
        .unwrap_err()
    }
    #[test]
    fn a_key_that_fails_to_decode_is_not_repeated_back_into_the_log() {
        let e = build(BAD_KEY, "aGVsbG8");
        assert!(!format!("{e}").contains("c2VjcmV0"), "{e}");
        assert!(!format!("{e:?}").contains("c2VjcmV0"), "{e:?}");
    }
    #[test]
    fn a_bad_payload_key_is_not_reported_as_a_bad_header_key() {
        let e = build("aGVsbG8", BAD_KEY);
        assert!(matches!(e, ConnConfigBuildError::PayloadCrypto(_)), "{e:?}");
    }
}
