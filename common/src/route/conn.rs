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
        let header_crypto = self.header_key.build()?;
        let payload_crypto = self.payload_key.map(|p| p.build()).transpose()?;
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
    #[error("{0}")]
    Crypto(#[from] tokio_chacha20::config::ConfigBuildError),
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
