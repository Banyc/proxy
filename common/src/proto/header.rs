use crate::{addr::InternetAddr, header::route::RouteRequest};

use super::addr::StreamAddr;

pub type StreamRequestHeader = RouteRequest<StreamAddr>;
pub type UdpRequestHeader = RouteRequest<InternetAddr>;

#[cfg(test)]
mod tests {
    use std::{io, net::SocketAddr};

    use tracing::trace;

    use crate::{
        anti_replay::{TimeValidator, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL, ValidatorRef},
        header::codec::{MAX_HEADER_LEN, read_header_async, write_header_async},
    };

    use super::*;

    fn create_random_crypto() -> tokio_chacha20::config::Config {
        let key: [u8; 32] = rand::random();
        tokio_chacha20::config::Config::new(key.into())
    }

    #[tokio::test]
    async fn test_request_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto();
        let time_validator = TimeValidator::new(VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL);

        // Encode header
        let original_header: UdpRequestHeader = RouteRequest {
            upstream: Some("1.1.1.1:8080".parse::<SocketAddr>().unwrap().into()),
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
        let validator = ValidatorRef::Time(&time_validator);
        let decoded_header = read_header_async(&mut stream, &mut crypto_cursor, &validator)
            .await
            .unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
