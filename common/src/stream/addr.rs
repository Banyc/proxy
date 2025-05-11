use std::{fmt::Display, str::FromStr, sync::Arc};

use hdv_derive::HdvSerde;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::addr::{InternetAddr, InternetAddrHdv, ParseInternetAddrError};

pub trait AsStreamType:
    Clone
    + Display
    + FromStr<Err = ParseInternetAddrError>
    + Serialize
    + DeserializeOwned
    + std::hash::Hash
    + Eq
    + std::fmt::Debug
    + Sync
    + Send
    + 'static
{
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct StreamAddrBuilder {
    pub address: Arc<str>,
}
impl StreamAddrBuilder {
    pub fn build<StreamType: AsStreamType>(
        self,
    ) -> Result<StreamAddr<StreamType>, ParseInternetAddrError> {
        self.address.as_ref().parse()
    }
}

/// A stream address
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, bincode::Encode, bincode::Decode,
)]
pub struct StreamAddr<StreamType> {
    pub address: InternetAddr,
    pub stream_type: StreamType,
}
impl<StreamType: Display> Display for StreamAddr<StreamType> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.stream_type, self.address)
    }
}
impl<StreamType: FromStr<Err = ParseInternetAddrError>> FromStr for StreamAddr<StreamType> {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split("://");
        let stream_type: StreamType = parts.next().ok_or(ParseInternetAddrError)?.parse()?;
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
impl<StreamType: Display> From<&StreamAddr<StreamType>> for StreamAddrHdv {
    fn from(value: &StreamAddr<StreamType>) -> Self {
        let addr = (&value.address).into();
        let ty = value.stream_type.to_string().into();
        Self { addr, ty }
    }
}
