use std::{io, net::SocketAddr, pin::Pin};

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    stream::{IoAddr, IoStream},
};

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

impl<S> IoStream for XorStream<S> where S: IoStream {}
impl<S> IoAddr for XorStream<S>
where
    S: IoAddr,
{
    fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.async_stream.peer_addr()
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.async_stream.local_addr()
    }
}

impl<S> AsyncWrite for XorStream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        // Take the inner buffer out with encoded data
        let mut inner_buf = {
            let mut inner_buf = self.buf.take().unwrap();
            if inner_buf.is_empty() {
                self.write_crypto.xor_to(buf, &mut inner_buf)?;
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

        // Return the result
        ready
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

impl<S> AsyncRead for XorStream<S>
where
    S: AsyncRead + Unpin,
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::crypto::tests::create_random_crypto;

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
}
