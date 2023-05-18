use std::{
    fmt::Display,
    io::{self, Read, Write},
    net::SocketAddr,
};

use duplicate::duplicate_item;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::lookup_host,
};
use tracing::{instrument, trace};

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::{ProxyProtocolError, ResponseError},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct RequestHeader {
    pub upstream: InternetAddr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum InternetAddr {
    SocketAddr(SocketAddr),
    String(String),
}

impl Display for InternetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SocketAddr(addr) => write!(f, "{}", addr),
            Self::String(string) => write!(f, "{}", string),
        }
    }
}

impl From<SocketAddr> for InternetAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::SocketAddr(addr)
    }
}

impl From<String> for InternetAddr {
    fn from(string: String) -> Self {
        match string.parse::<SocketAddr>() {
            Ok(addr) => Self::SocketAddr(addr),
            Err(_) => Self::String(string),
        }
    }
}

impl InternetAddr {
    pub async fn to_socket_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::SocketAddr(addr) => Ok(*addr),
            Self::String(host) => lookup_host(host)
                .await?
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "No address")),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfigBuilder {
    pub address: String,
    pub xor_key: Vec<u8>,
}

impl ProxyConfigBuilder {
    pub fn build(self) -> ProxyConfig {
        ProxyConfig {
            address: self.address.into(),
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub address: InternetAddr,
    pub crypto: XorCrypto,
}

pub fn convert_proxy_configs_to_header_crypto_pairs<'config>(
    nodes: &'config [ProxyConfig],
    destination: &InternetAddr,
) -> Vec<(RequestHeader, &'config XorCrypto)> {
    let mut pairs = Vec::new();
    for i in 0..nodes.len() - 1 {
        let node = &nodes[i];
        let next_node = &nodes[i + 1];
        pairs.push((
            RequestHeader {
                upstream: next_node.address.clone(),
            },
            &node.crypto,
        ));
    }
    pairs.push((
        RequestHeader {
            upstream: destination.clone(),
        },
        &nodes.last().unwrap().crypto,
    ));
    pairs
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use crate::error::ResponseErrorKind;

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
            upstream: "1.1.1.1:8080".parse::<SocketAddr>().unwrap().into(),
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
