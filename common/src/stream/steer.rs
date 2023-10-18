use std::{net::SocketAddr, time::Duration};

use metrics::counter;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    header::{
        codec::{timed_read_header_async, timed_write_header_async, CodecError},
        heartbeat::{self, HeartbeatError},
        route::RouteResponse,
    },
};

use super::{addr::StreamAddr, header::StreamRequestHeader, IoAddr, IoStream};

const IO_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn steer<S>(
    downstream: &mut S,
    crypto: &XorCrypto,
) -> Result<Option<StreamAddr>, SteerError>
where
    S: IoStream + IoAddr + std::fmt::Debug,
{
    // Wait for heartbeat upgrade
    heartbeat::wait_upgrade(downstream, IO_TIMEOUT)
        .await
        .map_err(|e| {
            let downstream_addr = downstream.peer_addr().ok();
            SteerError::ReadHeartbeatUpgrade {
                source: e,
                downstream_addr,
            }
        })?;

    // Decode header
    let mut read_crypto_cursor = XorCryptoCursor::new(crypto);
    let header: StreamRequestHeader =
        timed_read_header_async(downstream, &mut read_crypto_cursor, IO_TIMEOUT)
            .await
            .map_err(|e| {
                let downstream_addr = downstream.peer_addr().ok();
                SteerError::ReadStreamRequestHeader {
                    source: e,
                    downstream_addr,
                }
            })?;

    // Echo
    let addr = match header.upstream {
        Some(upstream) => upstream,
        None => {
            let resp = RouteResponse { result: Ok(()) };
            let mut write_crypto_cursor = XorCryptoCursor::new(crypto);
            timed_write_header_async(downstream, &resp, &mut write_crypto_cursor, IO_TIMEOUT)
                .await
                .map_err(|e| {
                    let downstream_addr = downstream.peer_addr().ok();
                    SteerError::WriteEchoResponse {
                        source: e,
                        downstream_addr,
                    }
                })?;
            let _ = tokio::time::timeout(IO_TIMEOUT, downstream.flush()).await;

            counter!("stream.echoes", 1);
            return Ok(None);
        }
    };
    Ok(Some(addr))
}

#[derive(Debug, Error)]
pub enum SteerError {
    #[error("Failed to read heartbeat header from downstream: {source}, {downstream_addr:?}")]
    ReadHeartbeatUpgrade {
        #[source]
        source: HeartbeatError,
        downstream_addr: Option<SocketAddr>,
    },
    #[error("Failed to read stream request header from downstream: {source}, {downstream_addr:?}")]
    ReadStreamRequestHeader {
        #[source]
        source: CodecError,
        downstream_addr: Option<SocketAddr>,
    },
    #[error("Failed to write echo response to downstream: {source}, {downstream_addr:?}")]
    WriteEchoResponse {
        #[source]
        source: CodecError,
        downstream_addr: Option<SocketAddr>,
    },
}