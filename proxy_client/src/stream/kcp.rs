use std::net::SocketAddr;

use common::{
    error::ProxyProtocolError,
    header::{InternetAddr, ProxyConfig},
    stream::{kcp::KcpConnector, pool::Pool, CreatedStream, StreamConnector},
};

use super::establish;

pub async fn kcp_establish(
    proxy_configs: &[ProxyConfig],
    destination: &InternetAddr,
    tcp_pool: &Pool,
) -> Result<(CreatedStream, SocketAddr), ProxyProtocolError> {
    let connector = StreamConnector::Kcp(KcpConnector);
    establish(&connector, proxy_configs, destination, tcp_pool).await
}
