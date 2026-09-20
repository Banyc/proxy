//! Fork of https://github.com/tokio-rs/tokio/blob/master/tokio/src/io/util/copy_bidirectional.rs
//! to allow us to get the read/write bytes count even
//! when an error occurred, see <https://github.com/tokio-rs/tokio/issues/4674>
//! for more info (and we can delete this fork once the original code in Rust/Tokio is fixed for MacOS localhost shutdown)

use super::copy::CopyBuffer;
use futures_core::ready;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::debug;

use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::task::{Context, Poll};

enum TransferState {
    Running(CopyBuffer),
    ShuttingDown(u64),
    Done(u64),
}

struct CopyBidirectional<'a, A: ?Sized, B: ?Sized> {
    a: &'a mut A,
    b: &'a mut B,
    a_to_b: TransferState,
    b_to_a: TransferState,
}

fn transfer_one_direction<A, B>(
    cx: &mut Context<'_>,
    state: &mut TransferState,
    r: &mut A,
    w: &mut B,
) -> Poll<io::Result<u64>>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut r = Pin::new(r);
    let mut w = Pin::new(w);

    loop {
        match state {
            TransferState::Running(buf) => {
                let count = ready!(buf.poll_copy(cx, r.as_mut(), w.as_mut()))?;
                *state = TransferState::ShuttingDown(count);
            }
            TransferState::ShuttingDown(count) => {
                (match ready!(w.as_mut().poll_shutdown(cx)) {
                    Ok(_) => Ok(()),
                    Err(err) => match err.kind() {
                        ErrorKind::NotConnected
                        | ErrorKind::ConnectionReset
                        | ErrorKind::BrokenPipe => {
                            debug!(
                                "tokio copy bidirectional: shutting down: ignore NotConnected/ConnectionReset/BrokenPipe error ignored"
                            );
                            Ok(())
                        }
                        _ => Err(err),
                    },
                })?;

                *state = TransferState::Done(*count);
            }
            TransferState::Done(count) => return Poll::Ready(Ok(*count)),
        }
    }
}

impl<A, B> Future for CopyBidirectional<'_, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    type Output = (Result<(), CopyBiError>, BytesCopied);

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Unpack self into mut refs to each field to avoid borrow check issues.
        let CopyBidirectional {
            a,
            b,
            a_to_b,
            b_to_a,
        } = &mut *self;

        let get_amounts = |a_to_b: &TransferState, b_to_a: &TransferState| -> BytesCopied {
            let a_to_b = match a_to_b {
                TransferState::Running(state) => state.amt(),
                TransferState::ShuttingDown(amt) => *amt,
                TransferState::Done(amt) => *amt,
            };
            let b_to_a = match b_to_a {
                TransferState::Running(state) => state.amt(),
                TransferState::ShuttingDown(amt) => *amt,
                TransferState::Done(amt) => *amt,
            };
            BytesCopied { a_to_b, b_to_a }
        };

        let a_to_b_poll = match transfer_one_direction(cx, a_to_b, &mut *a, &mut *b) {
            Poll::Ready(Err(e)) => {
                let amounts = get_amounts(a_to_b, b_to_a);
                return Poll::Ready((Err(CopyBiError::FromAToB(e)), amounts));
            }
            Poll::Ready(Ok(x)) => Poll::Ready(x),
            Poll::Pending => Poll::Pending,
        };
        let b_to_a_poll = match transfer_one_direction(cx, b_to_a, &mut *b, &mut *a) {
            Poll::Ready(Err(e)) => {
                let amounts = get_amounts(a_to_b, b_to_a);
                return Poll::Ready((Err(CopyBiError::FromBToA(e)), amounts));
            }
            Poll::Ready(Ok(x)) => Poll::Ready(x),
            Poll::Pending => Poll::Pending,
        };

        // It is not a problem if ready! returns early because transfer_one_direction for the
        // other direction will keep returning TransferState::Done(count) in future calls to poll
        let a_to_b = ready!(a_to_b_poll);
        let b_to_a = ready!(b_to_a_poll);
        Poll::Ready((Ok(()), BytesCopied { a_to_b, b_to_a }))
    }
}

/// Copies data in both directions between `a` and `b`.
///
/// This function returns a future that will read from both streams,
/// writing any data read to the opposing stream.
/// This happens in both directions concurrently.
///
/// If an EOF is observed on one stream, [`shutdown()`] will be invoked on
/// the other, and reading from that stream will stop. Copying of data in
/// the other direction will continue.
///
/// The future will complete successfully once both directions of communication has been shut down.
/// A direction is shut down when the reader reports EOF,
/// at which point [`shutdown()`] is called on the corresponding writer. When finished,
/// it will return a tuple of the number of bytes copied from a to b
/// and the number of bytes copied from b to a, in that order.
///
/// [`shutdown()`]: crate::io::AsyncWriteExt::shutdown
///
/// # Errors
///
/// The future will immediately return an error if any IO operation on `a`
/// or `b` returns an error. Some data read from either stream may be lost (not
/// written to the other stream) in this case.
///
/// # Return value
///
/// Returns a tuple of bytes copied `a` to `b` and bytes copied `b` to `a`.
pub async fn copy_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
) -> (Result<(), CopyBiError>, BytesCopied)
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    CopyBidirectional {
        a,
        b,
        a_to_b: TransferState::Running(CopyBuffer::new()),
        b_to_a: TransferState::Running(CopyBuffer::new()),
    }
    .await
}

#[derive(Debug, Clone, Copy)]
pub struct BytesCopied {
    pub a_to_b: u64,
    pub b_to_a: u64,
}

#[derive(Debug, Error)]
pub enum CopyBiError {
    #[error("error copying from A to B: {0}")]
    FromAToB(std::io::Error),
    #[error("error copying from B to A: {0}")]
    FromBToA(std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::ReadBuf;

    struct ReadFails;
    impl AsyncRead for ReadFails {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("read failed")))
        }
    }
    impl AsyncWrite for ReadFails {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct ReadPending;
    impl AsyncRead for ReadPending {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }
    impl AsyncWrite for ReadPending {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// An error reading the first stream is an A-to-B error; mislabelling it
    /// hides which direction of the relay failed.
    #[tokio::test]
    async fn an_error_reading_a_is_labelled_from_a() {
        let mut a = ReadFails;
        let mut b = ReadPending;
        let (result, _amounts) = copy_bidirectional(&mut a, &mut b).await;
        assert!(
            matches!(&result, Err(CopyBiError::FromAToB(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn an_error_reading_b_is_labelled_from_b() {
        let mut a = ReadPending;
        let mut b = ReadFails;
        let (result, _amounts) = copy_bidirectional(&mut a, &mut b).await;
        assert!(
            matches!(&result, Err(CopyBiError::FromBToA(_))),
            "{result:?}"
        );
    }

    /// Reads always fail with the given kind; writes and shutdown succeed.
    struct ReadFailsKind(io::ErrorKind);
    impl AsyncRead for ReadFailsKind {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(self.0, "read failed")))
        }
    }
    impl AsyncWrite for ReadFailsKind {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Reads its bytes once, then EOF; accepts writes. `poll_shutdown` fails
    /// with `shutdown_kind` so the post-copy shutdown path can be exercised.
    struct ReadOnceThenEof {
        data: Vec<u8>,
        pos: usize,
        shutdown_kind: Option<io::ErrorKind>,
    }
    impl ReadOnceThenEof {
        fn new(data: &[u8]) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
                shutdown_kind: None,
            }
        }
        fn shutdown_fails(data: &[u8], shutdown_kind: io::ErrorKind) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
                shutdown_kind: Some(shutdown_kind),
            }
        }
    }
    impl AsyncRead for ReadOnceThenEof {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for ReadOnceThenEof {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.shutdown_kind {
                Some(kind) => Poll::Ready(Err(io::Error::new(kind, "shutdown failed"))),
                None => Poll::Ready(Ok(())),
            }
        }
    }

    /// A localhost half-close surfaces as `NotConnected`, `ConnectionReset`,
    /// or `BrokenPipe` on macOS; the fork treats all three as EOF so the
    /// other direction can still drain. Dropping any one from the ignore set
    /// turns a clean relay teardown into a spurious copy error.
    #[tokio::test]
    async fn a_peer_went_away_read_error_is_treated_as_eof() {
        for kind in [
            io::ErrorKind::NotConnected,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
        ] {
            let mut a = ReadFailsKind(kind);
            let mut b = ReadOnceThenEof::new(b"payload");
            let (result, amounts) = copy_bidirectional(&mut a, &mut b).await;
            assert!(
                result.is_ok(),
                "a {kind:?} read error must be treated as EOF, got {result:?}"
            );
            assert_eq!(amounts.a_to_b, 0, "{kind:?}");
            assert_eq!(amounts.b_to_a, 7, "the other direction still drained");
        }
    }

    /// The same tolerance applies to the post-copy write-half shutdown: a
    /// peer that already went away makes `poll_shutdown` return one of these
    /// kinds, which must not surface as a copy error.
    #[tokio::test]
    async fn a_peer_went_away_shutdown_error_is_not_a_copy_error() {
        for kind in [
            io::ErrorKind::NotConnected,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
        ] {
            let mut a = ReadFailsKind(io::ErrorKind::ConnectionReset);
            let mut b = ReadOnceThenEof::shutdown_fails(b"payload", kind);
            let (result, amounts) = copy_bidirectional(&mut a, &mut b).await;
            assert!(
                result.is_ok(),
                "a {kind:?} shutdown error must be tolerated, got {result:?}"
            );
            assert_eq!(amounts.b_to_a, 7, "{kind:?}");
        }
    }

    /// An in-memory `AsyncRead + AsyncWrite` peer with a fixed send payload
    /// and a received-bytes sink, for deterministic whole-copy measurements.
    struct MemStream {
        send: Vec<u8>,
        pos: usize,
        recv: Vec<u8>,
    }
    impl MemStream {
        fn new(payload: Vec<u8>) -> Self {
            Self {
                send: payload,
                pos: 0,
                recv: Vec::with_capacity(0),
            }
        }
    }
    impl AsyncRead for MemStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = &self.send[self.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }
    impl AsyncWrite for MemStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.recv.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The relay copies through exactly two fixed buffers, one per direction,
    /// allocated once per connection: the allocation count of a whole
    /// bidirectional copy is constant regardless of how many chunks the
    /// payload splits into. A regression that allocates per chunk (or adds a
    /// third per-connection buffer) makes a large payload cost more than a
    /// small one.
    #[tokio::test]
    async fn the_relay_copy_allocates_a_constant_budget_regardless_of_payload() {
        for payload_len in [1024usize, 256 * 1024] {
            let payload = vec![0xabu8; payload_len];
            let mut a = MemStream::new(payload.clone());
            let mut b = MemStream::new(payload.clone());
            // Pre-size the receive sinks so the measurement covers only the
            // relay's own allocations, not the double's Vec growth.
            a.recv.reserve(payload_len);
            b.recv.reserve(payload_len);
            let guard = crate::test_alloc::Count::begin();
            let (result, amounts) = copy_bidirectional(&mut a, &mut b).await;
            let allocs = guard.allocs();
            let bytes = guard.bytes();
            drop(guard);
            result.unwrap();
            assert_eq!(amounts.a_to_b, payload_len as u64);
            assert_eq!(amounts.b_to_a, payload_len as u64);
            assert_eq!(
                allocs, 2,
                "a {payload_len}-byte bidirectional copy must allocate exactly the two per-direction CopyBuffers, observed {allocs} allocations"
            );
            assert_eq!(
                bytes,
                2 * super::super::DEFAULT_BUF_SIZE,
                "the two buffers must be the relay buffer size each, observed {bytes} bytes"
            );
        }
    }

    /// The relay's per-direction buffer size is a perf knob: it caps the
    /// bytes moved per read/write syscall pair. Pin it so a resizing does not
    /// slip by unnoticed.
    #[test]
    fn the_relay_buffer_is_configured_at_64kib_per_direction() {
        assert_eq!(
            super::super::DEFAULT_BUF_SIZE,
            64 * 1024,
            "the relay buffer size is a perf knob and must stay pinned"
        );
    }
}
