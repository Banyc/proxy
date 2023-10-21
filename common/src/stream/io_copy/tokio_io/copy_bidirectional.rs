//! Fork of https://github.com/tokio-rs/tokio/blob/master/tokio/src/io/util/copy_bidirectional.rs
//! to allow us to get the read/write bytes count even
//! when an error occurred, see <https://github.com/tokio-rs/tokio/issues/4674>
//! for more info (and we can delete this fork once the original code in Rust/Tokio is fixed for MacOS localhost shutdown)

use super::copy::CopyBuffer;
use futures_core::ready;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::debug;

use std::future::Future;
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
                            debug!("tokio copy bidirectional: shutting down: ignore NotConnected/ConnectionReset/BrokenPipe error ignored");
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

impl<'a, A, B> Future for CopyBidirectional<'a, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    type Output = Result<(u64, u64), CopyBiError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Unpack self into mut refs to each field to avoid borrow check issues.
        let CopyBidirectional {
            a,
            b,
            a_to_b,
            b_to_a,
        } = &mut *self;

        let get_amounts = |a_to_b: &TransferState, b_to_a: &TransferState| -> (u64, u64) {
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
            (a_to_b, b_to_a)
        };

        let a_to_b_poll = transfer_one_direction(cx, a_to_b, &mut *a, &mut *b).map_err(|e| {
            let amounts = get_amounts(a_to_b, b_to_a);
            CopyBiError {
                kind: CopyBiErrorKind::FromAToB(e),
                amounts,
            }
        })?;
        let b_to_a_poll = transfer_one_direction(cx, b_to_a, &mut *b, &mut *a).map_err(|e| {
            let amounts = get_amounts(a_to_b, b_to_a);
            CopyBiError {
                kind: CopyBiErrorKind::FromBToA(e),
                amounts,
            }
        })?;

        // It is not a problem if ready! returns early because transfer_one_direction for the
        // other direction will keep returning TransferState::Done(count) in future calls to poll
        let a_to_b = ready!(a_to_b_poll);
        let b_to_a = ready!(b_to_a_poll);

        Poll::Ready(Ok((a_to_b, b_to_a)))
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
pub async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> Result<(u64, u64), CopyBiError>
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

#[derive(Debug, Error)]
#[error("{}", kind)]
pub struct CopyBiError {
    pub kind: CopyBiErrorKind,
    pub amounts: (u64, u64),
}

#[derive(Debug, Error)]
pub enum CopyBiErrorKind {
    #[error("error copying from A to B: {0}")]
    FromAToB(std::io::Error),
    #[error("error copying from B to A: {0}")]
    FromBToA(std::io::Error),
}
