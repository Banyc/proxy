use std::time::Duration;

use async_speed_limit::Limiter;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_io_timeout::TimeoutStream;

use super::CopyBiError;

const UPLINK_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 2);
const DOWNLINK_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn timed_copy_bidirectional<A, B>(
    a: A,
    b: B,
    speed_limit: f64,
) -> Result<(u64, u64), CopyBiError>
where
    A: AsyncRead + AsyncWrite + Send + 'static,
    B: AsyncRead + AsyncWrite + Send + 'static,
{
    let mut a = TimeoutStream::new(a);
    let mut b = TimeoutStream::new(b);

    a.set_read_timeout(Some(UPLINK_TIMEOUT));
    a.set_write_timeout(Some(DOWNLINK_TIMEOUT));
    b.set_read_timeout(Some(DOWNLINK_TIMEOUT));
    b.set_write_timeout(Some(UPLINK_TIMEOUT));

    // STREAMS MUST BE `Unpin` BEFORE `io::split`
    let a = Box::pin(a);
    let b = Box::pin(b);

    // copy_bidirectional(&mut a, &mut b).await

    let (a_r, mut a_w) = tokio::io::split(a);
    let (b_r, mut b_w) = tokio::io::split(b);

    let limiter = <Limiter>::new(speed_limit);
    let mut a_r = limiter.clone().limit(a_r);
    let mut b_r = limiter.limit(b_r);

    let mut join_set = tokio::task::JoinSet::new();
    join_set.spawn(async move {
        tokio::io::copy(&mut a_r, &mut b_w)
            .await
            .map(CopyResult::AToB)
            .map_err(CopyBiError::FromAToB)
    });
    join_set.spawn(async move {
        tokio::io::copy(&mut b_r, &mut a_w)
            .await
            .map(CopyResult::BToA)
            .map_err(CopyBiError::FromBToA)
    });

    let mut a_to_b = None;
    let mut b_to_a = None;
    while let Some(res) = join_set.join_next().await {
        let res = res.unwrap()?;
        match res {
            CopyResult::AToB(n) => {
                a_to_b = Some(n);
            }
            CopyResult::BToA(n) => {
                b_to_a = Some(n);
            }
        }
    }
    Ok((a_to_b.unwrap(), b_to_a.unwrap()))
}

enum CopyResult {
    AToB(u64),
    BToA(u64),
}
