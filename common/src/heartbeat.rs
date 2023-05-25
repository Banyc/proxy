use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::ProxyProtocolError,
    header::{read_header_async, write_header_async},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HeartbeatRequest {
    Noop,
    Upgrade,
}

pub async fn send_noop<S>(stream: &mut S, timeout: Duration) -> Result<(), ProxyProtocolError>
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
            return Err(ProxyProtocolError::Timeout(timeout));
        }
    }
    Ok(())
}

pub async fn send_upgrade<S>(stream: &mut S) -> Result<(), ProxyProtocolError>
where
    S: AsyncWrite + Unpin,
{
    let crypto = XorCrypto::new(vec![]);
    let mut crypto_cursor = XorCryptoCursor::new(&crypto);
    let req = HeartbeatRequest::Upgrade;
    write_header_async(stream, &req, &mut crypto_cursor).await?;
    Ok(())
}

pub async fn wait_upgrade<S>(stream: &mut S) -> Result<(), ProxyProtocolError>
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
