use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::anti_replay::ValidatorRef;

use super::codec::{AsHeader, CodecError, read_header_async, write_header_async};

pub async fn send_noop<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), HeartbeatError>
where
    Stream: AsyncWrite + Unpin,
{
    let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
    let req = HeartbeatRequest::Noop;
    let res = tokio::time::timeout(
        timeout,
        write_header_async(stream, &req, &mut crypto_cursor),
    )
    .await;
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
    let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
    let req = HeartbeatRequest::Upgrade;
    let res = tokio::time::timeout(
        timeout,
        write_header_async(stream, &req, &mut crypto_cursor),
    )
    .await;
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
        let mut crypto_cursor = tokio_chacha20::cursor::DecryptCursor::new_x(*crypto.key());
        let res = tokio::time::timeout(
            timeout,
            read_header_async(stream, &mut crypto_cursor, validator),
        )
        .await;
        let header: HeartbeatRequest = res.map_err(|_| HeartbeatError::Timeout(timeout))??;
        if header == HeartbeatRequest::Upgrade {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
