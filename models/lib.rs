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

#[duplicate_item(
    name                async   stream_bounds       add_await(code) ;
    [read_header]       []      [BufRead]           [code]          ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn name<S, H>(stream: &mut S) -> Result<H, ProxyProtocolError>
where
    S: stream_bounds,
    H: for<'de> Deserialize<'de> + std::fmt::Debug,
{
    // Decode header length
    let len = {
        let mut buf = [0; 4];
        let res = stream.read_exact(&mut buf);
        add_await([res])?;
        u32::from_be_bytes(buf) as usize
    };
    trace!(len, "Read header length");

    // Decode header
    let header = {
        let mut buf = [0; MAX_HEADER_LEN];
        let res = stream.read_exact(&mut buf[..len]);
        add_await([res])?;
        bincode::deserialize(&buf[..len])?
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
pub async fn name<S, H>(stream: &mut S, header: &H) -> Result<(), ProxyProtocolError>
where
    S: stream_bounds,
    H: Serialize + std::fmt::Debug,
{
    let mut buf = [0; MAX_HEADER_LEN];
    let mut writer = io::Cursor::new(&mut buf[..]);

    // Encode header
    let buf = {
        bincode::serialize_into(&mut writer, header)?;
        let len = writer.position();
        let buf = &buf[..len as usize];
        trace!(?header, ?len, "Encoded header");
        buf
    };

    // Write header length
    let len = buf.len() as u32;
    add_await([stream.write_all(&len.to_be_bytes())])?;

    // Write header
    add_await([stream.write_all(buf)])?;

    Ok(())
}

pub const MAX_HEADER_LEN: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_request_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);

        // Encode header
        let original_header = RequestHeader {
            upstream: "1.1.1.1:8080".parse().unwrap(),
        };
        write_header_async(&mut stream, &original_header)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(&buf[..]);
        let decoded_header = read_header_async(&mut stream).await.unwrap();
        assert_eq!(original_header, decoded_header);
    }

    #[tokio::test]
    async fn test_response_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);

        // Encode header
        let original_header = ResponseHeader {
            result: Err(ResponseError {
                source: "1.1.1.1:8080".parse().unwrap(),
                kind: ResponseErrorKind::Io,
            }),
        };
        write_header_async(&mut stream, &original_header)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(&buf[..]);
        let decoded_header = read_header_async(&mut stream).await.unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
