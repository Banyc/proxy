use std::io::{self, Read, Write};

use duplicate::duplicate_item;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

use crate::{
    crypto::XorCryptoCursor,
    error::{ProxyProtocolError, ResponseError},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RequestHeader<A> {
    pub upstream: A,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ResponseHeader {
    pub result: Result<(), ResponseError>,
}

#[duplicate_item(
    read_header         async   stream_bounds       add_await(code) ;
    [read_header]       []      [Read]              [code]          ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn read_header<'crypto, S, H>(
    stream: &mut S,
    crypto: &mut XorCryptoCursor,
) -> Result<H, ProxyProtocolError>
where
    S: stream_bounds,
    H: for<'de> Deserialize<'de> + std::fmt::Debug,
{
    // Decode header length
    let len = {
        let mut buf = [0; 4];
        let res = stream.read_exact(&mut buf);
        add_await([res])?;
        crypto.xor(&mut buf);
        u32::from_be_bytes(buf) as usize
    };
    trace!(len, "Read header length");

    // Decode header
    let header = {
        let mut buf = [0; MAX_HEADER_LEN];
        if len > MAX_HEADER_LEN {
            return Err(ProxyProtocolError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Header too long",
            )));
        }
        let hdr = &mut buf[..len];
        let res = stream.read_exact(hdr);
        add_await([res])?;
        crypto.xor(hdr);
        bincode::deserialize(hdr)?
    };
    trace!(?header, "Read header");

    Ok(header)
}

#[duplicate_item(
    write_header         async   stream_bounds        add_await(code) ;
    [write_header]       []      [Write]              [code]          ;
    [write_header_async] [async] [AsyncWrite + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn write_header<'crypto, S, H>(
    stream: &mut S,
    header: &H,
    crypto: &mut XorCryptoCursor,
) -> Result<(), ProxyProtocolError>
where
    S: stream_bounds,
    H: Serialize + std::fmt::Debug,
{
    let mut buf = [0; MAX_HEADER_LEN];
    let mut writer = io::Cursor::new(&mut buf[..]);

    // Encode header
    let hdr = {
        bincode::serialize_into(&mut writer, header)?;
        let len = writer.position();
        let hdr = &mut buf[..len as usize];
        trace!(?header, ?len, "Encoded header");
        hdr
    };

    // Write header length
    let len = hdr.len() as u32;
    let mut len = len.to_be_bytes();
    crypto.xor(&mut len);
    add_await([stream.write_all(&len)])?;

    // Write header
    crypto.xor(hdr);
    add_await([stream.write_all(hdr)])?;

    Ok(())
}

pub const MAX_HEADER_LEN: usize = 1024;

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use rand::Rng;

    use crate::{crypto::XorCrypto, error::ResponseErrorKind};

    use super::*;

    fn create_random_crypto() -> XorCrypto {
        let mut rng = rand::thread_rng();
        let mut key = Vec::new();
        for _ in 0..MAX_HEADER_LEN {
            key.push(rng.gen());
        }
        XorCrypto::new(key)
    }

    #[tokio::test]
    async fn test_response_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto();

        // Encode header
        let original_header = ResponseHeader {
            result: Err(ResponseError {
                source: "1.1.1.1:8080".parse::<SocketAddr>().unwrap().into(),
                kind: ResponseErrorKind::Io,
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
