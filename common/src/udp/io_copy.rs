use std::{
    io::{self, Write},
    sync::{Arc, RwLock},
};

use async_speed_limit::Limiter;
use async_trait::async_trait;
use thiserror::Error;
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::trace;

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    error::AnyError,
    header::{codec::write_header, route::RouteResponse},
    udp::TIMEOUT,
};

use super::{Flow, FlowMetrics, Packet, UdpDownstreamWriter, BUFFER_LENGTH, LIVE_CHECK_INTERVAL};

#[async_trait]
pub trait UdpRecv {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError>;
}

#[async_trait]
pub trait UdpSend {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError>;
}

pub struct UpstreamParts<R, W> {
    pub read: R,
    pub write: W,
}

pub struct DownstreamParts {
    pub rx: mpsc::Receiver<Packet>,
    pub write: UdpDownstreamWriter,
}

pub async fn copy_bidirectional<R, W>(
    flow: Flow,
    mut upstream: UpstreamParts<R, W>,
    mut downstream: DownstreamParts,
    speed_limiter: Limiter,
    payload_crypto: Option<XorCrypto>,
    header_crypto: Option<XorCrypto>,
) -> Result<FlowMetrics, CopyBiError>
where
    R: UdpRecv + Send + 'static,
    W: UdpSend + Send + 'static,
{
    let start = std::time::Instant::now();

    // Periodic check if the flow is still alive
    let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
    let last_uplink_packet = Arc::new(RwLock::new(std::time::Instant::now()));
    let last_downlink_packet = Arc::new(RwLock::new(std::time::Instant::now()));

    let bytes_uplink = Arc::new(RwLock::new(0));
    let bytes_downlink = Arc::new(RwLock::new(0));
    let packets_uplink = Arc::new(RwLock::new(0));
    let packets_downlink = Arc::new(RwLock::new(0));

    let mut join_set = tokio::task::JoinSet::<Result<(), CopyBiError>>::new();
    join_set.spawn({
        let last_uplink_packet = Arc::clone(&last_uplink_packet);
        let bytes_uplink = Arc::clone(&bytes_uplink);
        let packets_uplink = Arc::clone(&packets_uplink);
        let speed_limiter = speed_limiter.clone();
        let payload_crypto = payload_crypto.clone();
        async move {
            loop {
                let res = downstream.rx.recv().await;
                trace!("Received packet from downstream");
                let mut packet = match res {
                    Some(packet) => packet,
                    None => {
                        // Channel closed
                        break;
                    }
                };

                // Limit speed
                speed_limiter.consume(packet.0.len()).await;

                // Xor payload
                if let Some(payload_crypto) = &payload_crypto {
                    let mut crypto_cursor = XorCryptoCursor::new(payload_crypto);
                    crypto_cursor.xor(&mut packet.0);
                }

                // Send packet to upstream
                upstream
                    .write
                    .trait_send(&packet.0)
                    .await
                    .map_err(CopyBiError::SendUpstream)?;

                *bytes_uplink.write().unwrap() += packet.0.len() as u64;
                *packets_uplink.write().unwrap() += 1;
                *last_uplink_packet.write().unwrap() = std::time::Instant::now();
            }
            Ok(())
        }
    });
    join_set.spawn({
        let last_downlink_packet = Arc::clone(&last_downlink_packet);
        let bytes_downlink = Arc::clone(&bytes_downlink);
        let packets_downlink = Arc::clone(&packets_downlink);
        let payload_crypto = payload_crypto.clone();
        let mut downlink_buf = [0; BUFFER_LENGTH];
        let mut downlink_protocol_buf = Vec::new();
        async move {
            loop {
                let res = upstream.read.trait_recv(&mut downlink_buf).await;
                trace!("Received packet from upstream");
                let n = res.map_err(CopyBiError::RecvUpstream)?;
                let pkt = &mut downlink_buf[..n];

                // Limit speed
                speed_limiter.consume(pkt.len()).await;

                // Xor payload
                if let Some(payload_crypto) = &payload_crypto {
                    let mut crypto_cursor = XorCryptoCursor::new(payload_crypto);
                    crypto_cursor.xor(pkt);
                }

                let pkt = if let Some(header_crypto) = &header_crypto {
                    // Set up protocol buffer writer
                    downlink_protocol_buf.clear();

                    // Write header
                    let header = RouteResponse { result: Ok(()) };
                    let mut crypto_cursor = XorCryptoCursor::new(header_crypto);
                    write_header(&mut downlink_protocol_buf, &header, &mut crypto_cursor).unwrap();

                    // Write payload
                    downlink_protocol_buf.write_all(pkt).unwrap();

                    downlink_protocol_buf.as_slice()
                } else {
                    pkt
                };

                // Send packet to downstream
                downstream
                    .write
                    .send(pkt)
                    .await
                    .map_err(|e| CopyBiError::SendDownstream {
                        source: e,
                        downstream: downstream.write.clone(),
                    })?;

                *bytes_downlink.write().unwrap() += pkt.len() as u64;
                *packets_downlink.write().unwrap() += 1;
                *last_downlink_packet.write().unwrap() = std::time::Instant::now();
            }
        }
    });

    // Forward packets
    loop {
        trace!("Waiting for packet");
        tokio::select! {
            res = join_set.join_next() => {
                let res = match res {
                    Some(res) => res.unwrap(),
                    None => break,
                };
                res?;
            }
            _ = tick.tick() => {
                trace!("Checking if flow is still alive");
                let now = std::time::Instant::now();
                let last_uplink_packet = *last_uplink_packet.read().unwrap();
                let last_downlink_packet = *last_downlink_packet.read().unwrap();

                if now.duration_since(last_uplink_packet) > TIMEOUT && now.duration_since(last_downlink_packet) > TIMEOUT {
                    trace!(?flow, "Flow timed out");
                    break;
                }
            }
        }
    }
    join_set.abort_all();

    let last_packet = std::time::Instant::max(
        *last_downlink_packet.read().unwrap(),
        *last_uplink_packet.read().unwrap(),
    );
    let bytes_uplink = *bytes_uplink.read().unwrap();
    let bytes_downlink = *bytes_downlink.read().unwrap();
    let packets_uplink = *packets_uplink.read().unwrap();
    let packets_downlink = *packets_downlink.read().unwrap();
    Ok(FlowMetrics {
        flow,
        start,
        end: last_packet,
        bytes_uplink,
        bytes_downlink,
        packets_uplink,
        packets_downlink,
    })
}

#[derive(Debug, Error)]
pub enum CopyBiError {
    #[error("Failed to send to upstream")]
    SendUpstream(#[source] AnyError),
    #[error("Failed to recv from upstream")]
    RecvUpstream(#[source] AnyError),
    #[error("Failed to send to downstream")]
    SendDownstream {
        #[source]
        source: io::Error,
        downstream: UdpDownstreamWriter,
    },
}

#[async_trait]
impl UdpSend for Arc<UdpSocket> {
    async fn trait_send(&mut self, buf: &[u8]) -> Result<usize, AnyError> {
        UdpSocket::send(self, buf).await.map_err(|e| e.into())
    }
}

#[async_trait]
impl UdpRecv for Arc<UdpSocket> {
    async fn trait_recv(&mut self, buf: &mut [u8]) -> Result<usize, AnyError> {
        UdpSocket::recv(self, buf).await.map_err(|e| e.into())
    }
}
