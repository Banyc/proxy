use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::codec::AsHeader;

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, bincode::Encode, bincode::Decode,
)]
pub struct RouteRequest<Addr> {
    pub upstream: Option<Addr>,
}
impl<Addr> AsHeader for RouteRequest<Addr> {}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize, bincode::Encode, bincode::Decode,
)]
pub struct RouteResponse {
    pub result: Result<(), RouteError>,
}
impl AsHeader for RouteResponse {}

#[derive(
    Debug,
    Error,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    bincode::Encode,
    bincode::Decode,
)]
#[error("{kind}")]
pub struct RouteError {
    pub kind: RouteErrorKind,
}
#[derive(
    Debug,
    Error,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Deserialize,
    Serialize,
    bincode::Encode,
    bincode::Decode,
)]
pub enum RouteErrorKind {
    #[error("io error")]
    Io,
    #[error("codec error")]
    Codec,
    #[error("loopback error")]
    Loopback,
    #[error("timeout error")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use std::io;

    use tracing::trace;

    use crate::{
        anti_replay::{ReplayValidator, VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, ValidatorRef},
        header::codec::{MAX_HEADER_LEN, read_header_async, write_header_async},
    };

    use super::*;

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    #[tokio::test]
    async fn test_response_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto();
        let replay_validator = ReplayValidator::new(VALIDATOR_TIME_FRAME, VALIDATOR_CAPACITY);

        // Encode header
        let original_header = RouteResponse {
            result: Err(RouteError {
                kind: RouteErrorKind::Io,
            }),
        };
        let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
        write_header_async(&mut stream, &original_header, &mut crypto_cursor)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(buf);
        let mut crypto_cursor = tokio_chacha20::cursor::DecryptCursor::new_x(*crypto.key());
        let validator = ValidatorRef::Replay(&replay_validator);
        let decoded_header = read_header_async(&mut stream, &mut crypto_cursor, &validator)
            .await
            .unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
