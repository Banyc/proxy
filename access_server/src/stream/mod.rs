use std::{net::SocketAddr, time::Instant};

use common::stream::{addr::StreamAddr, tokio_io, StreamMetrics};

pub mod proxy_table;
pub mod streams;

pub fn get_metrics_from_copy_result(
    start: Instant,
    upstream_addr: StreamAddr,
    upstream_sock_addr: SocketAddr,
    downstream_addr: Option<SocketAddr>,
    result: Result<(u64, u64), tokio_io::CopyBiError>,
) -> (StreamMetrics, Result<(), tokio_io::CopyBiErrorKind>) {
    let end = std::time::Instant::now();
    let (bytes_uplink, bytes_downlink) = match &result {
        Ok(res) => *res,
        Err(e) => e.amounts,
    };

    let metrics = StreamMetrics {
        start,
        end,
        bytes_uplink,
        bytes_downlink,
        upstream_addr,
        upstream_sock_addr,
        downstream_addr,
    };

    (metrics, result.map_err(|e| e.kind).map(|_| ()))
}
