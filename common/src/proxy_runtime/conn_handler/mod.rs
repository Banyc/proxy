use std::io;

use thiserror::Error;

pub mod stream;
pub mod udp;

#[derive(Debug, Error)]
#[error("Failed to bind to listen address: {0}")]
pub struct ListenerBindError(#[source] pub io::Error);
