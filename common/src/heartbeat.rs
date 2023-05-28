use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    header::{read_header_async, write_header_async, HeaderError},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HeartbeatRequest {
    Noop,
    Upgrade,
}

pub async fn send_noop<S>(stream: &mut S, timeout: Duration) -> Result<(), SendNoopError>
where
    S: AsyncWrite + Unpin,
{
    let crypto = XorCrypto::new(vec![]);
    let mut crypto_cursor = XorCryptoCursor::new(&crypto);
    let req = HeartbeatRequest::Noop;
    tokio::select! {
        res = write_header_async(stream, &req, &mut crypto_cursor) => {
            res?;
        }
        () = tokio::time::sleep(timeout) => {
            return Err(SendNoopError::Timeout(timeout));
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum SendNoopError {
    #[error("Failed to write header")]
    Header(#[from] HeaderError),
    #[error("Timeout")]
    Timeout(Duration),
}

pub async fn send_upgrade<S>(stream: &mut S) -> Result<(), HeaderError>
where
    S: AsyncWrite + Unpin,
{
    let crypto = XorCrypto::new(vec![]);
    let mut crypto_cursor = XorCryptoCursor::new(&crypto);
    let req = HeartbeatRequest::Upgrade;
    write_header_async(stream, &req, &mut crypto_cursor).await?;
    Ok(())
}

pub async fn wait_upgrade<S>(stream: &mut S) -> Result<(), HeaderError>
where
    S: AsyncRead + Unpin,
{
    let crypto = XorCrypto::new(vec![]);
    loop {
        let mut crypto_cursor = XorCryptoCursor::new(&crypto);
        let header: HeartbeatRequest = read_header_async(stream, &mut crypto_cursor).await?;
        if header == HeartbeatRequest::Upgrade {
            break;
        }
    }
    Ok(())
}
