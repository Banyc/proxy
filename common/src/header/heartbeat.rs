use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::crypto::{XorCrypto, XorCryptoCursor};

use super::codec::{read_header_async, write_header_async, CodecError, Header};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HeartbeatRequest {
    Noop,
    Upgrade,
}

impl Header for HeartbeatRequest {}

pub async fn send_noop<S>(stream: &mut S, timeout: Duration) -> Result<(), HeartbeatError>
where
    S: AsyncWrite + Unpin,
{
    let crypto = XorCrypto::new(vec![].into());
    let mut crypto_cursor = XorCryptoCursor::new(&crypto);
    let req = HeartbeatRequest::Noop;
    let res = tokio::time::timeout(
        timeout,
        write_header_async(stream, &req, &mut crypto_cursor),
    )
    .await;
    res.map_err(|_| HeartbeatError::Timeout(timeout))??;
    Ok(())
}

#[derive(Debug, Error)]
pub enum HeartbeatError {
    #[error("Failed to read/write header: {0}")]
    Header(#[from] CodecError),
    #[error("Timeout: {0:?}")]
    Timeout(Duration),
}

pub async fn send_upgrade<S>(stream: &mut S, timeout: Duration) -> Result<(), HeartbeatError>
where
    S: AsyncWrite + Unpin,
{
    let crypto = XorCrypto::new(vec![].into());
    let mut crypto_cursor = XorCryptoCursor::new(&crypto);
    let req = HeartbeatRequest::Upgrade;
    let res = tokio::time::timeout(
        timeout,
        write_header_async(stream, &req, &mut crypto_cursor),
    )
    .await;
    res.map_err(|_| HeartbeatError::Timeout(timeout))??;
    Ok(())
}

pub async fn wait_upgrade<S>(stream: &mut S, timeout: Duration) -> Result<(), HeartbeatError>
where
    S: AsyncRead + Unpin,
{
    let crypto = XorCrypto::new(vec![].into());
    loop {
        let mut crypto_cursor = XorCryptoCursor::new(&crypto);
        let res =
            tokio::time::timeout(timeout, read_header_async(stream, &mut crypto_cursor)).await;
        let header: HeartbeatRequest = res.map_err(|_| HeartbeatError::Timeout(timeout))??;
        if header == HeartbeatRequest::Upgrade {
            break;
        }
    }
    Ok(())
}
