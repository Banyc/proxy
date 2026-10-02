//! A stream wrapper that publishes each written chunk to a byte counter as
//! it moves.

use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::ready;
use metrics::Counter;
use pin_project_lite::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pin_project! {
    /// Wraps a relay stream and increments a byte counter by every chunk the
    /// copy actually writes, so a still-running stream reports its bytes now
    /// instead of only when it closes.
    ///
    /// The relay's `a` side is the downstream (client) connection and its `b`
    /// side the upstream: bytes written to `b` are uplink and bytes written to
    /// `a` are downlink, which is exactly the `a_to_b`/`b_to_a` accounting the
    /// completion log reports.
    pub(super) struct BytePublishingStream<S> {
        #[pin]
        inner: S,
        counter: Counter,
    }
}

impl<S> BytePublishingStream<S> {
    pub(super) fn uplink(inner: S) -> Self {
        Self {
            inner,
            counter: metrics::counter!("stream.up.bytes"),
        }
    }

    pub(super) fn downlink(inner: S) -> Self {
        Self {
            inner,
            counter: metrics::counter!("stream.dn.bytes"),
        }
    }
}

impl<S: AsyncRead> AsyncRead for BytePublishingStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.project().inner.poll_read(cx, buf)
    }
}

impl<S: AsyncWrite> AsyncWrite for BytePublishingStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        let written = ready!(this.inner.poll_write(cx, buf))?;
        this.counter.increment(written as u64);
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}
