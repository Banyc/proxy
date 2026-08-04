use std::time::Duration;

use ae::anti_replay::ValidatorRef;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use super::codec::{AsHeader, CodecError, read_header_async, write_header_async};

pub async fn send_keep_alive<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), PreambleError>
where
    Stream: AsyncWrite + Unpin,
{
    let req = Preamble::KeepAlive;
    let res = tokio::time::timeout(timeout, write_header_async(stream, &req, *crypto.key())).await;
    res.map_err(|_| PreambleError::Timeout(timeout))??;
    Ok(())
}

pub async fn send_upgrade<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), PreambleError>
where
    Stream: AsyncWrite + Unpin,
{
    let req = Preamble::Upgrade;
    let res = tokio::time::timeout(timeout, write_header_async(stream, &req, *crypto.key())).await;
    res.map_err(|_| PreambleError::Timeout(timeout))??;
    Ok(())
}

pub async fn wait_upgrade<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
    validator: &ValidatorRef<'_>,
) -> Result<(), PreambleError>
where
    Stream: AsyncRead + Unpin,
{
    loop {
        let res =
            tokio::time::timeout(timeout, read_header_async(stream, *crypto.key(), validator))
                .await;
        let header: Preamble = res.map_err(|_| PreambleError::Timeout(timeout))??;
        if header == Preamble::Upgrade {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preamble {
    KeepAlive,
    Upgrade,
}
impl AsHeader for Preamble {}

#[derive(Debug, Error)]
pub enum PreambleError {
    #[error("Failed to read/write header: {0}")]
    Header(#[from] CodecError),
    #[error("Timeout: {0:?}")]
    Timeout(Duration),
}
