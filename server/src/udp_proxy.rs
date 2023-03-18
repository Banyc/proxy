use std::{
    collections::HashMap,
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::Duration,
};

use models::{
    read_header, write_header, ProxyProtocolError, RequestHeader, ResponseError, ResponseErrorKind,
    ResponseHeader,
};
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{error, info, instrument, trace};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DownstreamAddr(SocketAddr);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct UpstreamAddr(SocketAddr);

type FlowMap = HashMap<Flow, mpsc::Sender<Packet>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Flow {
    upstream: UpstreamAddr,
    downstream: DownstreamAddr,
}

struct Packet(Vec<u8>);

pub struct UdpProxy {
    listener: UdpSocket,
}

impl UdpProxy {
    pub fn new(listener: UdpSocket) -> Self {
        Self { listener }
    }

    #[instrument(skip(self))]
    pub async fn serve(self) -> io::Result<()> {
        let flows: FlowMap = HashMap::new();
        let flows = Arc::new(RwLock::new(flows));
        let downstream_listener = Arc::new(self.listener);

        let addr = downstream_listener.local_addr()?;
        info!(?addr, "Listening");
        let mut buf = [0; 1024];
        loop {
            let (n, downstream_addr) = downstream_listener.recv_from(&mut buf).await?;
            let downstream_addr = DownstreamAddr(downstream_addr);

            let res = steer(
                Arc::clone(&downstream_listener),
                Arc::clone(&flows),
                &buf[..n],
                downstream_addr,
            )
            .await;
            handle_steer_result(&downstream_listener, downstream_addr, res).await;
        }
    }
}

#[instrument(skip(downstream_writer, flows, buf))]
async fn steer(
    downstream_writer: Arc<UdpSocket>,
    flows: Arc<RwLock<FlowMap>>,
    buf: &[u8],
    downstream_addr: DownstreamAddr,
) -> Result<(), ProxyProtocolError> {
    // Decode header
    let mut reader = io::Cursor::new(buf);
    let header: RequestHeader = read_header(&mut reader)?;
    let header_len = reader.position() as usize;
    let payload = &buf[header_len..];

    // Prevent connections to localhost
    if header.upstream.ip().is_loopback() {
        error!(?header, "Loopback");
        return Err(ProxyProtocolError::Loopback);
    }

    // Create flow if not exists
    let flow = Flow {
        upstream: UpstreamAddr(header.upstream),
        downstream: downstream_addr,
    };
    let flow_tx = {
        let flows = flows.read().unwrap();
        flows.get(&flow).cloned()
    };
    let flow_tx = match flow_tx {
        Some(flow_tx) => flow_tx,
        None => {
            let (tx, rx) = mpsc::channel(1);
            flows.write().unwrap().insert(flow, tx.clone());

            tokio::spawn(async move {
                let res = proxy(rx, flow, Arc::clone(&downstream_writer)).await;
                handle_proxy_result(&downstream_writer, downstream_addr, res).await;

                // Remove flow
                flows.write().unwrap().remove(&flow);
            });

            tx
        }
    };

    // Steer packet
    let packet = Packet(payload.to_vec());
    let _ = flow_tx.send(packet).await;

    Ok(())
}

async fn handle_steer_result(
    downstream_listener: &Arc<UdpSocket>,
    downstream_addr: DownstreamAddr,
    res: Result<(), ProxyProtocolError>,
) {
    match res {
        Ok(()) => {
            // No response
        }
        Err(err) => {
            error!(?err, "Failed to steer");
            respond_with_error(&downstream_listener, downstream_addr, err).await;
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FlowMetrics {
    flow: Flow,
    start: std::time::Instant,
    end: std::time::Instant,
    bytes_uplink: usize,
    bytes_downlink: usize,
    packets_uplink: usize,
    packets_downlink: usize,
}

#[instrument(skip(rx, downstream_writer))]
async fn proxy(
    mut rx: mpsc::Receiver<Packet>,
    flow: Flow,
    downstream_writer: Arc<UdpSocket>,
) -> Result<FlowMetrics, ProxyProtocolError> {
    let start = std::time::Instant::now();

    // Connect to upstream
    let any_ip = match flow.upstream.0.ip() {
        IpAddr::V4(_) => IpAddr::V4("0.0.0.0".parse().unwrap()),
        IpAddr::V6(_) => IpAddr::V6("::".parse().unwrap()),
    };
    let any_addr = SocketAddr::new(any_ip, 0);
    let upstream = UdpSocket::bind(any_addr).await?;
    upstream.connect(flow.upstream.0).await?;

    // Periodic check if the flow is still alive
    let mut tick = tokio::time::interval(LIVE_CHECK_INTERVAL);
    let mut last_packet = std::time::Instant::now();

    let mut bytes_uplink = 0;
    let mut bytes_downlink = 0;
    let mut packets_uplink = 0;
    let mut packets_downlink = 0;

    // Forward packets
    let mut downlink_buf = [0; 1024];
    loop {
        tokio::select! {
            res = rx.recv() => {
                let packet = match res {
                    Some(packet) => packet,
                    None => {
                        // Channel closed
                        break;
                    }
                };

                // Send packet to upstream
                upstream.send(&packet.0).await?;
                bytes_uplink += &packet.0.len();
                packets_uplink += 1;

                last_packet = std::time::Instant::now();
            }
            res = upstream.recv(&mut downlink_buf) => {
                let n = res?;
                let pkt = &downlink_buf[..n];

                // Write header
                let mut writer = io::Cursor::new(Vec::new());
                let header = ResponseHeader {
                    result: Ok(()),
                };
                write_header(&mut writer, &header)?;

                // Write payload
                writer.write_all(pkt)?;

                // Send packet to downstream
                let pkt = writer.into_inner();
                downstream_writer.send_to(&pkt, flow.downstream.0).await?;
                bytes_downlink += &pkt.len();
                packets_downlink += 1;

                last_packet = std::time::Instant::now();
            }
            _ = tick.tick() => {
                if last_packet.elapsed() > TIMEOUT {
                    info!(?flow, "Flow timed out");
                    break;
                }
            }
        }
    }

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

#[instrument(skip(downstream_writer, res))]
async fn handle_proxy_result(
    downstream_writer: &UdpSocket,
    downstream_addr: DownstreamAddr,
    res: Result<FlowMetrics, ProxyProtocolError>,
) {
    match res {
        Ok(metrics) => {
            // No response
            info!(?metrics, "Connection closed normally");
        }
        Err(e) => {
            error!(?e, "Connection closed with error");
            respond_with_error(downstream_writer, downstream_addr, e).await;
        }
    }
}

#[instrument(skip(downstream_writer))]
async fn respond_with_error(
    downstream_writer: &UdpSocket,
    downstream_addr: DownstreamAddr,
    error: ProxyProtocolError,
) {
    let local_addr = match downstream_writer.local_addr() {
        Ok(addr) => addr,
        Err(e) => {
            trace!(?e, "Failed to get local address");
            return;
        }
    };

    // Respond with error
    let resp = match error {
        ProxyProtocolError::Io(_) => ResponseHeader {
            result: Err(ResponseError {
                source: local_addr,
                kind: ResponseErrorKind::Io,
            }),
        },
        ProxyProtocolError::Bincode(_) => ResponseHeader {
            result: Err(ResponseError {
                source: local_addr,
                kind: ResponseErrorKind::Codec,
            }),
        },
        ProxyProtocolError::Loopback => ResponseHeader {
            result: Err(ResponseError {
                source: local_addr,
                kind: ResponseErrorKind::Loopback,
            }),
        },
        ProxyProtocolError::Response(err) => ResponseHeader { result: Err(err) },
    };
    let mut buf = Vec::new();
    write_header(&mut buf, &resp).unwrap();
    let _ = downstream_writer.send_to(&buf, downstream_addr.0).await;
}
