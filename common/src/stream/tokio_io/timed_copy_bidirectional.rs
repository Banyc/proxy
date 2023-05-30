use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

const POST_FIN_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn timed_copy_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
) -> Result<(u64, u64), TimedCopyBiError>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let (mut a_r, mut a_w) = tokio::io::split(a);
    let (mut b_r, mut b_w) = tokio::io::split(b);

    let done = tokio::select! {
        res = tokio::io::copy(&mut a_r, &mut b_w) => {
            Done::FromAToB(res.map_err(TimedCopyBiError::FromAToB)?)
        }
        res = tokio::io::copy(&mut b_r, &mut a_w) => {
            Done::FromBToA(res.map_err(TimedCopyBiError::FromBToA)?)
        }
    };

    let a = a_r.unsplit(a_w);
    let b = b_r.unsplit(b_w);

    let res = tokio::select! {
        res = transfer_after_fin(done, a, b) => res,
        () = tokio::time::sleep(POST_FIN_TIMEOUT) => {
            Err(TimedCopyBiError::FinTimeout(done))
        }
    };

    res
}

async fn transfer_after_fin<A, B>(
    done: Done,
    a: &mut A,
    b: &mut B,
) -> Result<(u64, u64), TimedCopyBiError>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let (mut a_r, mut a_w) = tokio::io::split(a);
    let (mut b_r, mut b_w) = tokio::io::split(b);

    let (a_to_b, b_to_a) = match done {
        Done::FromAToB(a_to_b) => {
            let b_to_a = tokio::io::copy(&mut b_r, &mut a_w)
                .await
                .map_err(TimedCopyBiError::FromBToA)?;
            (a_to_b, b_to_a)
        }
        Done::FromBToA(b_to_a) => {
            let a_to_b = tokio::io::copy(&mut a_r, &mut b_w)
                .await
                .map_err(TimedCopyBiError::FromAToB)?;
            (a_to_b, b_to_a)
        }
    };

    Ok((a_to_b, b_to_a))
}

#[derive(Debug, Clone, Copy)]
pub enum Done {
    FromAToB(u64),
    FromBToA(u64),
}

#[derive(Debug, Error)]
pub enum TimedCopyBiError {
    #[error("error copying from A to B")]
    FromAToB(std::io::Error),
    #[error("error copying from B to A")]
    FromBToA(std::io::Error),
    #[error("timeout")]
    FinTimeout(Done),
}
