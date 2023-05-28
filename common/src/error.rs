use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::addr::InternetAddr;

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

pub type AnyResult = Result<(), AnyError>;
pub type AnyError = Box<dyn std::error::Error + Send + Sync>;
