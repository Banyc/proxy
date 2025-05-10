use std::{net::SocketAddr, time::Duration};

use metrics::counter;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::{
    anti_replay::{ReplayValidator, ValidatorRef},
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        heartbeat::{self, HeartbeatError},
        route::RouteResponse,
    },
};

use super::{HasIoAddr, OwnIoStream, addr::StreamAddr, header::StreamRequestHeader};

const IO_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn steer<Downstream, StreamType: std::fmt::Debug + DeserializeOwned>(
    downstream: &mut Downstream,
    crypto: &tokio_chacha20::config::Config,
    replay_validator: &ReplayValidator,
) -> Result<Option<StreamAddr<StreamType>>, SteerError>
where
    Downstream: OwnIoStream + HasIoAddr + std::fmt::Debug,
{
    let validator = ValidatorRef::Replay(replay_validator);
    // Wait for heartbeat upgrade
    heartbeat::wait_upgrade(downstream, IO_TIMEOUT, crypto, &validator)
        .await
        .map_err(|e| {
            let downstream_addr = downstream.peer_addr().ok();
            SteerError::ReadHeartbeatUpgrade {
                source: e,
                downstream_addr,
            }
        })?;

    // Decode header
    let mut read_crypto_cursor = tokio_chacha20::cursor::DecryptCursor::new_x(*crypto.key());
    let header: StreamRequestHeader<StreamType> =
        timed_read_header_async(downstream, &mut read_crypto_cursor, &validator, IO_TIMEOUT)
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
            let mut write_crypto_cursor =
                tokio_chacha20::cursor::EncryptCursor::new_x(*crypto.key());
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

            counter!("stream.echoes").increment(1);
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
