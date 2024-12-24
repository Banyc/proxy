use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::codec::Header;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RouteRequest<A> {
    pub upstream: Option<A>,
}
impl<A> Header for RouteRequest<A> {}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RouteResponse {
    pub result: Result<(), RouteError>,
}
impl Header for RouteResponse {}

#[derive(Debug, Error, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[error("{kind}")]
pub struct RouteError {
    pub kind: RouteErrorKind,
}
#[derive(Debug, Error, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
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
        anti_replay::{ReplayValidator, VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME},
        header::codec::{read_header_async, write_header_async, MAX_HEADER_LEN},
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
        let decoded_header = read_header_async(&mut stream, &mut crypto_cursor, &replay_validator)
            .await
            .unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
