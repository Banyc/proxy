use std::io;

use thiserror::Error;

pub mod speed_limit;
pub mod stream;
pub mod udp;

pub use speed_limit::{SpeedLimit, SpeedLimitError};

#[derive(Debug, Error)]
#[error("Failed to bind to listen address: {0}")]
pub struct ListenerBindError(#[source] pub io::Error);
