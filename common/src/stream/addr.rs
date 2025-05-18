use std::{fmt::Display, str::FromStr, sync::Arc};

use hdv_derive::HdvSerde;
use serde::{Deserialize, Serialize};

use crate::addr::{InternetAddr, InternetAddrHdv, ParseInternetAddrError};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct StreamAddrBuilder {
    pub address: Arc<str>,
}
impl StreamAddrBuilder {
    pub fn build(self) -> Result<StreamAddr, ParseInternetAddrError> {
        self.address.as_ref().parse()
    }
}

/// A stream address
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, bincode::Encode, bincode::Decode,
)]
pub struct StreamAddr {
    pub address: InternetAddr,
    pub stream_type: Arc<str>,
}
impl Display for StreamAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.stream_type, self.address)
    }
}
impl FromStr for StreamAddr {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split("://");
        let stream_type = parts.next().ok_or(ParseInternetAddrError)?.into();
        let address = parts.next().ok_or(ParseInternetAddrError)?.parse()?;
        if parts.next().is_some() {
            return Err(ParseInternetAddrError);
        }
        Ok(StreamAddr {
            address,
            stream_type,
        })
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct StreamAddrHdv {
    pub addr: InternetAddrHdv,
    pub ty: Arc<str>,
}
impl From<&StreamAddr> for StreamAddrHdv {
    fn from(value: &StreamAddr) -> Self {
        let addr = (&value.address).into();
        let ty = value.stream_type.to_string().into();
        Self { addr, ty }
    }
}
