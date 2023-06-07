use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_io_timeout::TimeoutStream;

use super::{copy_bidirectional, CopyBiError};

const UPLINK_TIMEOUT: Duration = Duration::from_secs(60 * 60 * 2);
const DOWNLINK_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn timed_copy_bidirectional<A, B>(a: A, b: B) -> Result<(u64, u64), CopyBiError>
where
    A: AsyncRead + AsyncWrite,
    B: AsyncRead + AsyncWrite,
{
    let mut a = TimeoutStream::new(a);
    let mut b = TimeoutStream::new(b);

    a.set_read_timeout(Some(UPLINK_TIMEOUT));
    a.set_write_timeout(Some(DOWNLINK_TIMEOUT));
    b.set_read_timeout(Some(DOWNLINK_TIMEOUT));
    b.set_write_timeout(Some(UPLINK_TIMEOUT));

    let mut a = Box::pin(a);
    let mut b = Box::pin(b);

    copy_bidirectional(&mut a, &mut b).await
}
