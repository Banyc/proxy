//! Wire protocol for reverse-tunnel registration, plus the session-level
//! error type and mux reaping helper shared by the initiator and responder
//! sides.

use std::{io, sync::Arc};

use common::header::codec::{AsHeader, CodecError};
use mux::MuxError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub(crate) const REGISTER_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisterRequest {
    pub(crate) version: u16,
    pub(crate) name: Arc<str>,
}
impl AsHeader for RegisterRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegisterResponse {
    pub(crate) result: Result<(), RegisterError>,
}
impl AsHeader for RegisterResponse {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
pub(crate) enum RegisterError {
    #[error("unsupported reverse tunnel protocol version")]
    UnsupportedVersion,
    #[error("invalid reverse tunnel name")]
    InvalidName,
}

#[derive(Debug, Error)]
pub(crate) enum ReverseTunnelSessionError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("registration codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("registration rejected: {0}")]
    Registration(RegisterError),
    #[error("mux error: {0}")]
    Mux(String),
    #[error("reverse tunnel session closed")]
    Closed,
}

pub(crate) fn mux_result(result: Option<Result<MuxError, tokio::task::JoinError>>) -> MuxError {
    match result {
        Some(result) => result.unwrap(),
        None => MuxError::TaskStopped { task: "revtun" },
    }
}

#[cfg(test)]
mod tests {
    use super::REGISTER_VERSION;

    /// The registration version travels in every reverse-tunnel registration
    /// header, and the responder compares it against this same constant, so
    /// an initiator and responder built from this tree always agree and every
    /// test stays green however the constant moves. Pin the literal: a
    /// version bump is a wire change that needs a peer on the other side,
    /// not an internal refactor.
    #[test]
    fn the_registration_wire_version_is_two() {
        assert_eq!(REGISTER_VERSION, 2);
    }
}
