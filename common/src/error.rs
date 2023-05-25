use std::{io, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::header::{InternetAddr, ResponseHeader};

#[derive(Debug, Error)]
pub enum ProxyProtocolError {
    #[error("io error")]
    Io(#[from] io::Error),
    #[error("bincode error")]
    Bincode(#[from] bincode::Error),
    #[error("loopback error")]
    Loopback,
    #[error("response error")]
    Response(ResponseError),
    #[error("timeout error")]
    Timeout(Duration),
}

impl ProxyProtocolError {
    pub fn into_response_header(self, source: InternetAddr) -> ResponseHeader {
        match self {
            ProxyProtocolError::Io(_) => ResponseHeader {
                result: Err(ResponseError {
                    source,
                    kind: ResponseErrorKind::Io,
                }),
            },
            ProxyProtocolError::Bincode(_) => ResponseHeader {
                result: Err(ResponseError {
                    source,
                    kind: ResponseErrorKind::Codec,
                }),
            },
            ProxyProtocolError::Loopback => ResponseHeader {
                result: Err(ResponseError {
                    source,
                    kind: ResponseErrorKind::Loopback,
                }),
            },
            ProxyProtocolError::Response(err) => ResponseHeader { result: Err(err) },
            ProxyProtocolError::Timeout(_) => ResponseHeader {
                result: Err(ResponseError {
                    source,
                    kind: ResponseErrorKind::Timeout,
                }),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ResponseError {
    pub source: InternetAddr,
    pub kind: ResponseErrorKind,
}

#[derive(Debug, Error, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum ResponseErrorKind {
    #[error("io error")]
    Io,
    #[error("bincode error")]
    Codec,
    #[error("loopback error")]
    Loopback,
    #[error("timeout error")]
    Timeout,
}
