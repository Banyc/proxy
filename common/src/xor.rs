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
    buf: Vec<u8>,
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
            buf: Vec::new(),
        }
    }

    pub fn upgrade(stream: S, crypto: &XorCrypto) -> Self {
        // Establish encrypted stream
        let read_crypto_cursor = XorCryptoCursor::new(crypto);
        let write_crypto_cursor = XorCryptoCursor::new(crypto);
        XorStream::new(stream, write_crypto_cursor, read_crypto_cursor)
    }
}
impl<Stream> XorStream<Stream>
where
    Stream: AsyncWrite + Unpin,
{
    fn poll_drain(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        let Self {
            async_stream, buf, ..
        } = self;
        while !buf.is_empty() {
            let n = ready!(Pin::new(&mut *async_stream).poll_write(cx, buf))?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into()).into();
            }
            buf.drain(..n);
        }
        Ok(()).into()
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
        ready!(self.as_mut().get_mut().poll_drain(cx))?;
        if buf.is_empty() {
            return Ok(0).into();
        }
        let this = &mut *self;
        this.buf.extend_from_slice(buf);
        this.write_crypto.xor(&mut this.buf);
        if let std::task::Poll::Ready(Err(e)) = this.poll_drain(cx) {
            return Err(e).into();
        }
        Ok(buf.len()).into()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        ready!(self.as_mut().get_mut().poll_drain(cx))?;
        Pin::new(&mut self.async_stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        ready!(self.as_mut().get_mut().poll_drain(cx))?;
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
        let filled_before = buf.filled().len();
        let res = ready!(Pin::new(&mut self.async_stream).poll_read(cx, buf));
        self.read_crypto.xor(&mut buf.filled_mut()[filled_before..]);
        std::task::Poll::Ready(res)
    }
}

#[cfg(test)]
mod tests {
    use rand::RngExt;
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

    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }
    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            let n = self
                .chunk
                .min(self.data.len() - self.pos)
                .min(buf.remaining());
            let pos = self.pos;
            buf.put_slice(&self.data[pos..pos + n]);
            self.pos += n;
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn a_stream_arriving_in_chunks_decrypts_to_what_was_encrypted() {
        let crypto = create_random_crypto(3);
        let plaintext = b"Hello, world! This one spans several reads.";
        let mut ciphertext = plaintext.to_vec();
        XorCryptoCursor::new(&crypto).xor(&mut ciphertext);
        for chunk in [1, 2, 5, 7] {
            let inner = ChunkedReader {
                data: ciphertext.clone(),
                pos: 0,
                chunk,
            };
            let mut stream = XorStream::upgrade(inner, &crypto);
            let mut got = vec![0u8; plaintext.len()];
            stream.read_exact(&mut got).await.unwrap();
            assert_eq!(
                got, plaintext,
                "delivered {chunk} bytes at a time, the plaintext came back wrong"
            );
        }
    }
    struct StingyWriter {
        sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        stall: bool,
    }
    impl AsyncWrite for StingyWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.stall = !self.stall;
            if self.stall {
                return std::task::Poll::Pending;
            }
            let n = buf.len().min(4);
            self.sink.lock().unwrap().extend_from_slice(&buf[..n]);
            std::task::Poll::Ready(Ok(n))
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn a_write_reports_only_the_slice_it_was_given() {
        let crypto = create_random_crypto(3);
        let first = b"first message";
        let second = b"second message";
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = StingyWriter {
            sink: std::sync::Arc::clone(&sink),
            stall: true,
        };
        let mut stream = XorStream::upgrade(writer, &crypto);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let _ = Pin::new(&mut stream).poll_write(&mut cx, first);
        let drive = |f: &mut dyn FnMut(&mut std::task::Context<'_>) -> std::task::Poll<()>| {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            for _ in 0..1000 {
                if f(&mut cx).is_ready() {
                    return;
                }
            }
            panic!("the write never finished");
        };
        drive(&mut |cx| {
            Pin::new(&mut stream)
                .poll_write(cx, second)
                .map(|r| assert_eq!(r.unwrap(), second.len()))
        });
        drive(&mut |cx| Pin::new(&mut stream).poll_flush(cx).map(|r| r.unwrap()));
        let mut got = sink.lock().unwrap().clone();
        XorCryptoCursor::new(&crypto).xor(&mut got);
        let mut want = first.to_vec();
        want.extend_from_slice(second);
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want),
            "the peer did not receive both messages exactly once",
        );
    }
}
