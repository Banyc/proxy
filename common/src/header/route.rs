use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RouteRequest<A> {
    pub upstream: Option<A>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RouteResponse {
    pub result: Result<(), RouteError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
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

    use rand::Rng;
    use tracing::trace;

    use crate::{
        crypto::{XorCrypto, XorCryptoCursor},
        header::codec::{read_header_async, write_header_async, MAX_HEADER_LEN},
    };

    use super::*;

    fn create_random_crypto() -> XorCrypto {
        let mut rng = rand::thread_rng();
        let mut key = Vec::new();
        for _ in 0..MAX_HEADER_LEN {
            key.push(rng.gen());
        }
        XorCrypto::new(key.into())
    }

    #[tokio::test]
    async fn test_response_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto();

        // Encode header
        let original_header = RouteResponse {
            result: Err(RouteError {
                kind: RouteErrorKind::Io,
            }),
        };
        let mut crypto_cursor = XorCryptoCursor::new(&crypto);
        write_header_async(&mut stream, &original_header, &mut crypto_cursor)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(buf);
        let mut crypto_cursor = XorCryptoCursor::new(&crypto);
        let decoded_header = read_header_async(&mut stream, &mut crypto_cursor)
            .await
            .unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
