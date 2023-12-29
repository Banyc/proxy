use std::{fmt::Display, str::FromStr, sync::Arc};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::addr::{InternetAddr, ParseInternetAddrError};

pub trait StreamType:
    Clone + Display + FromStr<Err = ParseInternetAddrError> + Serialize + DeserializeOwned
{
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct StreamAddrBuilder {
    pub address: Arc<str>,
}

impl StreamAddrBuilder {
    pub fn build<ST: StreamType>(self) -> Result<StreamAddr<ST>, ParseInternetAddrError> {
        self.address.as_ref().parse()
    }
}

/// A stream address
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct StreamAddr<ST> {
    pub address: InternetAddr,
    pub stream_type: ST,
}

impl<ST: Display> Display for StreamAddr<ST> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://{}", self.stream_type, self.address)
    }
}

impl<ST: FromStr<Err = ParseInternetAddrError>> FromStr for StreamAddr<ST> {
    type Err = ParseInternetAddrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split("://");
        let stream_type: ST = parts.next().ok_or(ParseInternetAddrError)?.parse()?;
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

pub trait StreamAddrStr {
    type StreamType: StreamType;

    fn inner(&self) -> &StreamAddr<Self::StreamType>;
    fn into_inner(self) -> StreamAddr<Self::StreamType>;
}
