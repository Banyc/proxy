use std::{
    io::{self, BufRead, Write},
    net::SocketAddr,
};

use duplicate::duplicate_item;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

pub mod addr;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RequestHeader {
    pub upstream: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ResponseHeader {
    pub result: Result<(), ResponseError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ResponseError {
    pub source: SocketAddr,
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub struct XorCrypto {
    key: Vec<u8>,
}

impl XorCrypto {
    pub fn new(key: Vec<u8>) -> Self {
        Self { key }
    }

    pub fn xor(&self, buf: &mut [u8]) {
        if self.key.is_empty() {
            return;
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b ^= self.key[i % self.key.len()];
        }
    }
}

#[duplicate_item(
    name                async   stream_bounds       add_await(code) ;
    [read_header]       []      [BufRead]           [code]          ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn name<S, H>(stream: &mut S, crypto: &XorCrypto) -> Result<H, ProxyProtocolError>
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
    name                 async   stream_bounds        add_await(code) ;
    [write_header]       []      [Write]              [code]          ;
    [write_header_async] [async] [AsyncWrite + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn name<S, H>(
    stream: &mut S,
    header: &H,
    crypto: &XorCrypto,
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
        crypto.xor(hdr);
        trace!(?header, ?len, "Encoded header");
        hdr
    };

    // Write header length
    let len = hdr.len() as u32;
    let mut len = len.to_be_bytes();
    crypto.xor(&mut len);
    add_await([stream.write_all(&len)])?;

    // Write header
    add_await([stream.write_all(hdr)])?;

    Ok(())
}

pub const MAX_HEADER_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub address: SocketAddr,
    pub crypto: XorCrypto,
}

pub fn convert_proxy_configs_to_header_crypto_pairs<'config>(
    nodes: &'config [ProxyConfig],
    destination: &SocketAddr,
) -> Vec<(RequestHeader, &'config XorCrypto)> {
    let mut pairs = Vec::new();
    for i in 0..nodes.len() - 1 {
        let node = &nodes[i];
        let next_node = &nodes[i + 1];
        pairs.push((
            RequestHeader {
                upstream: next_node.address,
            },
            &node.crypto,
        ));
    }
    pairs.push((
        RequestHeader {
            upstream: *destination,
        },
        &nodes.last().unwrap().crypto,
    ));
    pairs
}

#[cfg(test)]
mod tests {
    use rand::Rng;

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
    async fn test_request_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto();

        // Encode header
        let original_header = RequestHeader {
            upstream: "1.1.1.1:8080".parse().unwrap(),
        };
        write_header_async(&mut stream, &original_header, &crypto)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(&buf[..]);
        let decoded_header = read_header_async(&mut stream, &crypto).await.unwrap();
        assert_eq!(original_header, decoded_header);
    }

    #[tokio::test]
    async fn test_response_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto();

        // Encode header
        let original_header = ResponseHeader {
            result: Err(ResponseError {
                source: "1.1.1.1:8080".parse().unwrap(),
                kind: ResponseErrorKind::Io,
            }),
        };
        write_header_async(&mut stream, &original_header, &crypto)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(&buf[..]);
        let decoded_header = read_header_async(&mut stream, &crypto).await.unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
