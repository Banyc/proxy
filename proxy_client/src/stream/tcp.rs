use std::net::SocketAddr;

use common::{
    error::ProxyProtocolError,
    header::{InternetAddr, ProxyConfig},
    stream::{pool::Pool, tcp::TcpConnector, CreatedStream, StreamConnector},
};

use super::establish;

pub async fn tcp_establish(
    proxy_configs: &[ProxyConfig],
    destination: &InternetAddr,
    tcp_pool: &Pool,
) -> Result<(CreatedStream, SocketAddr), ProxyProtocolError> {
    let connector = StreamConnector::Tcp(TcpConnector);
    establish(&connector, proxy_configs, destination, tcp_pool).await
}
