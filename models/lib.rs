use std::{io, net::SocketAddr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

#[derive(Debug, Error)]
pub enum ProxyProtocolError {
    #[error("io error")]
    Io(#[from] io::Error),
    #[error("bincode error")]
    Bincode(#[from] bincode::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct Header {
    pub upstream: SocketAddr,
}

#[instrument(skip_all)]
pub async fn read_header<S>(stream: &mut S) -> Result<Header, ProxyProtocolError>
where
    S: AsyncRead + Unpin,
{
    // Decode header length
    let len = {
        let mut buf = [0; 4];
        stream.read_exact(&mut buf).await?;
        u32::from_be_bytes(buf) as usize
    };
    trace!(len, "Read header length");

    // Decode header
    let header = {
        let mut buf = [0; MAX_HEADER_LEN];
        stream.read_exact(&mut buf[..len]).await?;
        bincode::deserialize(&buf[..len])?
    };
    trace!(?header, "Read header");

    Ok(header)
}

#[instrument(skip_all)]
pub async fn write_header<S>(stream: &mut S, header: &Header) -> Result<(), ProxyProtocolError>
where
    S: AsyncWrite + Unpin,
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
    stream.write_all(&len.to_be_bytes()).await?;

    // Write header
    stream.write_all(buf).await?;

    Ok(())
}

pub const MAX_HEADER_LEN: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);

        // Encode header
        let original_header = Header {
            upstream: "1.1.1.1:8080".parse().unwrap(),
        };
        write_header(&mut stream, &original_header).await.unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(&buf[..]);
        let decoded_header = read_header(&mut stream).await.unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
