use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfigBuilder<AS> {
    pub address: AS,
    pub header_key: tokio_chacha20::config::ConfigBuilder,
    pub payload_key: Option<tokio_chacha20::config::ConfigBuilder>,
}
impl<AS> ProxyConfigBuilder<AS> {
    pub fn build<A>(self) -> Result<ProxyConfig<A>, ProxyConfigBuildError>
    where
        AS: AddressString<Address = A>,
    {
        let header_crypto = self.header_key.build()?;
        let payload_crypto = self.payload_key.map(|p| p.build()).transpose()?;
        let address = self.address.into_address();
        Ok(ProxyConfig {
            address,
            header_crypto,
            payload_crypto,
        })
    }
}
#[derive(Debug, Error)]
pub enum ProxyConfigBuildError {
    #[error("{0}")]
    Crypto(#[from] tokio_chacha20::config::ConfigBuildError),
}

pub trait AddressString: Serialize + DeserializeOwned {
    type Address;
    fn into_address(self) -> Self::Address;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig<A> {
    pub address: A,
    pub header_crypto: tokio_chacha20::config::Config,
    pub payload_crypto: Option<tokio_chacha20::config::Config>,
}
