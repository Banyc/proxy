//! Tokio wrappers which apply timeouts to IO operations.
//!
//! These timeouts are analogous to the read and write timeouts on traditional blocking sockets. A timeout countdown is
//! initiated when a read/write operation returns [`Poll::Pending`]. If a read/write does not return successfully before
//! the countdown expires, an [`io::Error`] with a kind of [`TimedOut`](io::ErrorKind::TimedOut) is returned.
#![warn(missing_docs)]

use pin_project_lite::pin_project;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{sleep_until, Instant, Sleep};

pin_project! {
    #[derive(Debug)]
    struct TimeoutState {
        timeout: Option<Duration>,
        #[pin]
        cur: Sleep,
        active: bool,
    }
}

impl TimeoutState {
    #[inline]
    fn new() -> TimeoutState {
        TimeoutState {
            timeout: None,
            cur: sleep_until(Instant::now()),
            active: false,
        }
    }

    #[inline]
    fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    #[inline]
    fn set_timeout(&mut self, timeout: Option<Duration>) {
        // since this takes &mut self, we can't yet be active
        self.timeout = timeout;
    }

    #[inline]
    fn set_timeout_pinned(mut self: Pin<&mut Self>, timeout: Option<Duration>) {
        *self.as_mut().project().timeout = timeout;
        self.reset();
    }

    #[inline]
    fn reset(self: Pin<&mut Self>) {
        let this = self.project();

        if *this.active {
            *this.active = false;
            this.cur.reset(Instant::now());
        }
    }

    #[inline]
    fn poll_check(self: Pin<&mut Self>, cx: &mut Context<'_>) -> io::Result<()> {
        let mut this = self.project();

        let timeout = match this.timeout {
            Some(timeout) => *timeout,
            None => return Ok(()),
        };

        if !*this.active {
            this.cur.as_mut().reset(Instant::now() + timeout);
            *this.active = true;
        }

        match this.cur.poll(cx) {
            Poll::Ready(()) => Err(io::Error::from(io::ErrorKind::TimedOut)),
            Poll::Pending => Ok(()),
        }
    }
}

pin_project! {
    /// An `AsyncRead` and `AsyncWrite`er which applies a timeout to read and write operations.
    #[derive(Debug)]
    pub struct TimeoutStreamShared<S> {
        #[pin]
        stream: S,
        #[pin]
        state: TimeoutState,
    }
}

impl<S> TimeoutStreamShared<S>
where
    S: AsyncRead + AsyncWrite,
{
    /// Returns a new `TimeoutStreamShared` wrapping the specified stream.
    ///
    /// There is initially no timeout.
    pub fn new(stream: S) -> TimeoutStreamShared<S> {
        TimeoutStreamShared {
            stream,
            state: TimeoutState::new(),
        }
    }

    /// Returns the current read and write timeout.
    pub fn timeout(&self) -> Option<Duration> {
        self.state.timeout()
    }

    /// Sets the read and write timeout.
    ///
    /// This can only be used before the stream is pinned; use [`set_timeout_pinned`](Self::set_timeout_pinned)
    /// otherwise.
    pub fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.state.set_timeout(timeout);
    }

    /// Sets the read and write timeout.
    ///
    /// This will reset any pending timeout. Use [`set_timeout`](Self::set_timeout) instead if the stream is not yet
    /// pinned.
    pub fn set_timeout_pinned(self: Pin<&mut Self>, timeout: Option<Duration>) {
        self.project().state.set_timeout_pinned(timeout);
    }

    /// Returns a shared reference to the inner stream.
    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    /// Returns a mutable reference to the inner stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Returns a pinned mutable reference to the inner stream.
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut S> {
        self.project().stream
    }

    /// Consumes the `TimeoutStreamShared`, returning the inner stream.
    pub fn into_inner(self) -> S {
        self.stream
    }
}

impl<R> AsyncRead for TimeoutStreamShared<R>
where
    R: AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        let r = this.stream.poll_read(cx, buf);
        match r {
            Poll::Pending => this.state.poll_check(cx)?,
            _ => this.state.reset(),
        }
        r
    }
}

impl<W> AsyncWrite for TimeoutStreamShared<W>
where
    W: AsyncWrite,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.project();
        let r = this.stream.poll_write(cx, buf);
        match r {
            Poll::Pending => this.state.poll_check(cx)?,
            _ => this.state.reset(),
        }
        r
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        let r = this.stream.poll_flush(cx);
        match r {
            Poll::Pending => this.state.poll_check(cx)?,
            _ => this.state.reset(),
        }
        r
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        let this = self.project();
        let r = this.stream.poll_shutdown(cx);
        match r {
            Poll::Pending => this.state.poll_check(cx)?,
            _ => this.state.reset(),
        }
        r
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let r = this.stream.poll_write_vectored(cx, bufs);
        match r {
            Poll::Pending => this.state.poll_check(cx)?,
            _ => this.state.reset(),
        }
        r
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::pin;

    pin_project! {
        struct DelayStream {
            #[pin]
            sleep: Sleep,
        }
    }

    impl DelayStream {
        fn new(until: Instant) -> Self {
            DelayStream {
                sleep: sleep_until(until),
            }
        }
    }

    impl AsyncRead for DelayStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context,
            _buf: &mut ReadBuf,
        ) -> Poll<Result<(), io::Error>> {
            match self.project().sleep.poll(cx) {
                Poll::Ready(()) => Poll::Ready(Ok(())),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl AsyncWrite for DelayStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            match self.project().sleep.poll(cx) {
                Poll::Ready(()) => Poll::Ready(Ok(buf.len())),
                Poll::Pending => Poll::Pending,
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), io::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn read_timeout() {
        let reader = DelayStream::new(Instant::now() + Duration::from_millis(500));
        let mut reader = TimeoutStreamShared::new(reader);
        reader.set_timeout(Some(Duration::from_millis(100)));
        pin!(reader);

        let r = reader.read(&mut [0]).await;
        assert_eq!(r.err().unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[allow(clippy::unused_io_amount)]
    #[tokio::test]
    async fn read_ok() {
        let reader = DelayStream::new(Instant::now() + Duration::from_millis(100));
        let mut reader = TimeoutStreamShared::new(reader);
        reader.set_timeout(Some(Duration::from_millis(500)));
        pin!(reader);

        reader.read(&mut [0]).await.unwrap();
    }

    #[tokio::test]
    async fn write_timeout() {
        let writer = DelayStream::new(Instant::now() + Duration::from_millis(500));
        let mut writer = TimeoutStreamShared::new(writer);
        writer.set_timeout(Some(Duration::from_millis(100)));
        pin!(writer);

        let r = writer.write(&[0]).await;
        assert_eq!(r.err().unwrap().kind(), io::ErrorKind::TimedOut);
    }

    #[allow(clippy::unused_io_amount)]
    #[tokio::test]
    async fn write_ok() {
        let writer = DelayStream::new(Instant::now() + Duration::from_millis(100));
        let mut writer = TimeoutStreamShared::new(writer);
        writer.set_timeout(Some(Duration::from_millis(500)));
        pin!(writer);

        writer.write(&[0]).await.unwrap();
    }

    #[allow(clippy::unused_io_amount)]
    #[tokio::test]
    async fn tcp_read() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        thread::spawn(move || {
            let mut socket = listener.accept().unwrap().0;
            thread::sleep(Duration::from_millis(10));
            socket.write_all(b"f").unwrap();
            thread::sleep(Duration::from_millis(500));
            let _ = socket.write_all(b"f"); // this may hit an eof
        });

        let s = TcpStream::connect(&addr).await.unwrap();
        let mut s = TimeoutStreamShared::new(s);
        s.set_timeout(Some(Duration::from_millis(100)));
        pin!(s);
        s.read(&mut [0]).await.unwrap();
        let r = s.read(&mut [0]).await;

        match r {
            Ok(_) => panic!("unexpected success"),
            Err(ref e) if e.kind() == io::ErrorKind::TimedOut => (),
            Err(e) => panic!("{:?}", e),
        }
    }
}
