use std::io;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::header::InternetAddr;

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
}
