use std::{
    io::{self, Write},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
};

use base64::prelude::*;
use futures_core::ready;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::stream::{HasIoAddr, OwnIoStream};

type Key = Arc<[u8]>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct XorCryptoBuilder(pub String);
impl XorCryptoBuilder {
    pub fn build(&self) -> Result<XorCrypto, XorCryptoBuildError> {
        let key = BASE64_STANDARD_NO_PAD
            .decode(&self.0)
            .map_err(|e| XorCryptoBuildError {
                source: e,
                key: self.0.clone(),
            })?;
        Ok(XorCrypto::new(key.into()))
    }
}
#[derive(Debug, Error)]
#[error("{source}, key = `{key}`")]
pub struct XorCryptoBuildError {
    #[source]
    pub source: base64::DecodeError,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct XorCrypto {
    key: Key,
}
impl XorCrypto {
    pub fn new(key: Key) -> Self {
        Self { key }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct XorCryptoCursor {
    key: Key,
    pos: usize,
}
impl XorCryptoCursor {
    pub fn new(config: &XorCrypto) -> Self {
        Self {
            key: Arc::clone(&config.key),
            pos: 0,
        }
    }
}
impl XorCryptoCursor {
    pub fn xor(&mut self, buf: &mut [u8]) {
        if self.key.is_empty() {
            return;
        }
        buf.iter_mut().enumerate().for_each(|(i, b)| {
            let i = i + self.pos;
            let xor_b = *b ^ self.key[i % self.key.len()];
            *b = xor_b;
        });
        self.pos = (self.pos + buf.len()) % self.key.len();
    }

    pub fn xor_to<W>(&mut self, buf: &[u8], to: &mut W) -> io::Result<()>
    where
        W: Write,
    {
        if self.key.is_empty() {
            return Ok(());
        }
        for (i, b) in buf.iter().enumerate() {
            let i = i + self.pos;
            let xor_b = *b ^ self.key[i % self.key.len()];
            to.write_all(&[xor_b])?;
        }
        self.pos = (self.pos + buf.len()) % self.key.len();
        Ok(())
    }
}

#[derive(Debug)]
pub struct XorStream<S> {
    write_crypto: XorCryptoCursor,
    read_crypto: XorCryptoCursor,
    async_stream: S,
    buf: Option<Vec<u8>>,
}
impl<S> XorStream<S> {
    pub fn new(
        async_stream: S,
        write_crypto: XorCryptoCursor,
        read_crypto: XorCryptoCursor,
    ) -> Self {
        Self {
            async_stream,
            write_crypto,
            read_crypto,
            buf: Some(Vec::new()),
        }
    }

    pub fn upgrade(stream: S, crypto: &XorCrypto) -> Self {
        // Establish encrypted stream
        let read_crypto_cursor = XorCryptoCursor::new(crypto);
        let write_crypto_cursor = XorCryptoCursor::new(crypto);
        XorStream::new(stream, write_crypto_cursor, read_crypto_cursor)
    }
}
impl<Stream> OwnIoStream for XorStream<Stream> where Stream: OwnIoStream {}
impl<Stream> HasIoAddr for XorStream<Stream>
where
    Stream: HasIoAddr,
{
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.async_stream.peer_addr()
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.async_stream.local_addr()
    }
}
impl<Stream> AsyncWrite for XorStream<Stream>
where
    Stream: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        loop {
            // Take the inner buffer out with encoded data
            let mut inner_buf = {
                let mut inner_buf = self.buf.take().unwrap();
                if inner_buf.is_empty() {
                    inner_buf.extend(buf);
                    self.write_crypto.xor(&mut inner_buf);
                }
                inner_buf
            };

            // Write the encoded data to the stream
            let ready = Pin::new(&mut self.async_stream).poll_write(cx, &inner_buf);

            // Clean the inner buffer if the write is successful
            if let std::task::Poll::Ready(Ok(n)) = &ready {
                inner_buf.drain(..*n);
            }

            // Put the inner buffer back
            self.buf = Some(inner_buf);

            let _ = ready!(ready)?;

            // Do not allow caller to switch buffers until the inner buffer is fully consumed
            if self.buf.as_ref().unwrap().is_empty() {
                return Ok(buf.len()).into();
            }
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.async_stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.async_stream).poll_shutdown(cx)
    }
}
impl<Stream> AsyncRead for XorStream<Stream>
where
    Stream: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let ready = Pin::new(&mut self.async_stream).poll_read(cx, buf);
        self.read_crypto.xor(buf.filled_mut());
        ready
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[tokio::test]
    async fn xor_stream() {
        let crypto = create_random_crypto(3);

        let (client, server) = tokio::io::duplex(1024);
        let mut client = XorStream::upgrade(client, &crypto);
        let mut server = XorStream::upgrade(server, &crypto);

        let data = b"Hello, world!";
        let mut buf = [0u8; 1024];
        println!("Writing data");
        client.write_all(data).await.unwrap();
        println!("Reading data");
        server.read_exact(&mut buf[..data.len()]).await.unwrap();
        assert_eq!(&buf[..data.len()], data);
    }

    #[tokio::test]
    async fn xor_stream_incompatible() {
        let crypto = create_random_crypto(3);

        let (client, mut server) = tokio::io::duplex(1024);
        let mut client = XorStream::upgrade(client, &crypto);

        let data = b"Hello, world!";
        let mut buf = [0u8; 1024];
        println!("Writing data");
        client.write_all(data).await.unwrap();
        println!("Reading data");
        server.read_exact(&mut buf[..data.len()]).await.unwrap();
        assert_ne!(&buf[..data.len()], data);
    }

    fn create_random_crypto(len: usize) -> XorCrypto {
        let mut rng = rand::rng();
        let mut key = Vec::new();
        for _ in 0..len {
            key.push(rng.random());
        }
        XorCrypto::new(key.into())
    }
}
