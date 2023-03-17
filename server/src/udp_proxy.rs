use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::Duration,
};

use models::{read_header, ProxyProtocolError, RequestHeader};
use tokio::{net::UdpSocket, sync::mpsc};
use tracing::{error, info};

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

            // Respond in best effort
            match res {
                Ok(_) => {
                    // No response
                }
                Err(e) => {
                    error!(?e, "Failed to steer");
                    // No response
                }
            }
        }
    }
}

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
                let res = proxy(Arc::clone(&flows), rx, flow, downstream_writer).await;
                if let Err(e) = res {
                    error!(?e, "Failed to proxy");
                }
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

const TIMEOUT: Duration = Duration::from_secs(10);
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

async fn proxy(
    flows: Arc<RwLock<FlowMap>>,
    mut rx: mpsc::Receiver<Packet>,
    flow: Flow,
    downstream_writer: Arc<UdpSocket>,
) -> io::Result<()> {
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

    // Forward packets
    let mut downlink_buf = [0; 1024];
    loop {
        tokio::select! {
            Some(packet) = rx.recv() => {
                upstream.send(&packet.0).await?;
                last_packet = std::time::Instant::now();
            }
            res = upstream.recv(&mut downlink_buf) => {
                let n = res?;
                let pkt = &downlink_buf[..n];
                downstream_writer.send_to(pkt, flow.downstream.0).await?;
                last_packet = std::time::Instant::now();
            }
            _ = tick.tick() => {
                if last_packet.elapsed() > TIMEOUT {
                    info!("Flow timed out");

                    // Remove flow
                    flows.write().unwrap().remove(&flow);

                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use models::{write_header, RequestHeader};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_proxy() {
        // Start proxy server
        let proxy_addr = {
            let listener = UdpSocket::bind("localhost:0").await.unwrap();
            let proxy_addr = listener.local_addr().unwrap();
            let proxy = UdpProxy::new(listener);
            tokio::spawn(async move {
                proxy.serve().await.unwrap();
            });
            proxy_addr
        };

        // Message to send
        let req_msg = b"hello world";
        let resp_msg = b"goodbye world";

        // Start origin server
        let origin_addr = {
            let listener = UdpSocket::bind("[::]:0").await.unwrap();
            let origin_addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                let mut buf = [0; 1024];
                let (n, addr) = listener.recv_from(&mut buf).await.unwrap();
                let msg_buf = &buf[..n];
                assert_eq!(msg_buf, req_msg);
                listener.send_to(resp_msg, addr).await.unwrap();
            });
            origin_addr
        };

        // Connect to proxy server
        let socket = UdpSocket::bind("localhost:0").await.unwrap();
        socket.connect(proxy_addr).await.unwrap();

        let mut buf = Vec::new();
        let mut writer = io::Cursor::new(&mut buf);

        // Establish connection to origin server
        {
            // Encode header
            let header = RequestHeader {
                upstream: origin_addr,
            };
            write_header(&mut writer, &header).unwrap();
        }

        // Write message
        writer.write_all(req_msg).await.unwrap();

        // Send message
        socket.send(&buf).await.unwrap();

        // Read response
        {
            let mut buf = [0; 1024];
            let n = socket.recv(&mut buf).await.unwrap();
            let msg_buf = &buf[..n];
            assert_eq!(msg_buf, resp_msg);
        }
    }
}
