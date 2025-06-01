use std::time::Duration;

use ae::anti_replay::ValidatorRef;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use super::codec::{AsHeader, CodecError, read_header_async, write_header_async};

pub async fn send_noop<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), HeartbeatError>
where
    Stream: AsyncWrite + Unpin,
{
    let req = HeartbeatRequest::Noop;
    let res = tokio::time::timeout(timeout, write_header_async(stream, &req, *crypto.key())).await;
    res.map_err(|_| HeartbeatError::Timeout(timeout))??;
    Ok(())
}

pub async fn send_upgrade<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), HeartbeatError>
where
    Stream: AsyncWrite + Unpin,
{
    let req = HeartbeatRequest::Upgrade;
    let res = tokio::time::timeout(timeout, write_header_async(stream, &req, *crypto.key())).await;
    res.map_err(|_| HeartbeatError::Timeout(timeout))??;
    Ok(())
}

pub async fn wait_upgrade<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
    validator: &ValidatorRef<'_>,
) -> Result<(), HeartbeatError>
where
    Stream: AsyncRead + Unpin,
{
    loop {
        let res =
            tokio::time::timeout(timeout, read_header_async(stream, *crypto.key(), validator))
                .await;
        let header: HeartbeatRequest = res.map_err(|_| HeartbeatError::Timeout(timeout))??;
        if header == HeartbeatRequest::Upgrade {
            break;
        }
    }
    Ok(())
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, bincode::Encode, bincode::Decode,
)]
pub enum HeartbeatRequest {
    Noop,
    Upgrade,
}
impl AsHeader for HeartbeatRequest {}

#[derive(Debug, Error)]
pub enum HeartbeatError {
    #[error("Failed to read/write header: {0}")]
    Header(#[from] CodecError),
    #[error("Timeout: {0:?}")]
    Timeout(Duration),
}
