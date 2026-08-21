use std::net::SocketAddr;

use ae::anti_replay::{ReplayValidator, ValidatorRef};
use metrics::counter;
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::{
    header::{
        codec::{CodecError, timed_read_header_async, timed_write_header_async},
        preamble::{self, PreambleError},
        route::RouteResponse,
    },
    proxy_runtime::{addr::RouteAddr, header::StreamRequestHeader},
    stream::IoConnection,
};

pub async fn read_route_header<Downstream>(
    downstream: &mut Downstream,
    crypto: &tokio_chacha20::config::Config,
    replay_validator: &ReplayValidator,
) -> Result<Option<RouteAddr>, SteerError>
where
    Downstream: IoConnection + std::fmt::Debug,
{
    let validator = ValidatorRef::Replay(replay_validator);
    // Wait for heartbeat upgrade
    preamble::wait_upgrade(downstream, crate::STREAM_IO_TIMEOUT, crypto, &validator)
        .await
        .map_err(|e| {
            let downstream_addr = downstream.peer_addr().ok();
            SteerError::ReadHeartbeatUpgrade {
                source: e,
                downstream_addr,
            }
        })?;

    // Decode header
    let header: StreamRequestHeader = timed_read_header_async(
        downstream,
        *crypto.key(),
        &validator,
        crate::STREAM_IO_TIMEOUT,
    )
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
            timed_write_header_async(downstream, &resp, *crypto.key(), crate::STREAM_IO_TIMEOUT)
                .await
                .map_err(|e| {
                    let downstream_addr = downstream.peer_addr().ok();
                    SteerError::WriteEchoResponse {
                        source: e,
                        downstream_addr,
                    }
                })?;
            let _ = tokio::time::timeout(crate::STREAM_IO_TIMEOUT, downstream.flush()).await;

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
        source: PreambleError,
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
