use std::{collections::HashMap, io, sync::Arc};

use async_speed_limit::Limiter;
use common::{
    config::SharableConfig,
    loading,
    proxy_table::ProxyGroupBuildError,
    udp::{
        context::UdpContext,
        io_copy::{CopyBidirectional, DownstreamParts, UpstreamParts},
        proxy_table::{UdpProxyGroup, UdpProxyGroupBuilder},
        Flow, Packet, UdpServer, UdpServerHandleConn, UpstreamAddr,
    },
};
use proxy_client::udp::{EstablishError, UdpProxyClient};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::{ToSocketAddrs, UdpSocket};
use tracing::{error, warn};
use udp_listener::Conn;

use crate::{socks5::messages::UdpRequestHeader, udp::proxy_table::UdpProxyGroupBuildContext};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Socks5ServerUdpAccessServerConfig {
    pub listen_addr: Arc<str>,
    pub proxy_group: SharableConfig<UdpProxyGroupBuilder>,
    pub speed_limit: Option<f64>,
}
impl Socks5ServerUdpAccessServerConfig {
    pub fn into_builder(
        self,
        proxy_group: &HashMap<Arc<str>, UdpProxyGroup>,
        cx: UdpProxyGroupBuildContext<'_>,
        udp_context: UdpContext,
    ) -> Result<Socks5ServerUdpAccessServerBuilder, BuildError> {
        let proxy_group = match self.proxy_group {
            SharableConfig::SharingKey(key) => proxy_group
                .get(&key)
                .ok_or_else(|| BuildError::ProxyGroupKeyNotFound(key.clone()))?
                .clone(),
            SharableConfig::Private(x) => x.build(cx)?,
        };

        Ok(Socks5ServerUdpAccessServerBuilder {
            listen_addr: self.listen_addr,
            proxy_group,
            speed_limit: self.speed_limit.unwrap_or(f64::INFINITY),
            udp_context,
        })
    }
}
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("Proxy group key not found: {0}")]
    ProxyGroupKeyNotFound(Arc<str>),
    #[error("{0}")]
    ProxyGroup(#[from] ProxyGroupBuildError),
}

#[derive(Debug, Clone)]
pub struct Socks5ServerUdpAccessServerBuilder {
    listen_addr: Arc<str>,
    proxy_group: UdpProxyGroup,
    speed_limit: f64,
    udp_context: UdpContext,
}
impl loading::Build for Socks5ServerUdpAccessServerBuilder {
    type ConnHandler = Socks5ServerUdpAccessConnHandler;
    type Server = UdpServer<Self::ConnHandler>;
    type Err = io::Error;

    async fn build_server(self) -> Result<Self::Server, Self::Err> {
        let listen_addr = self.listen_addr.clone();
        let access = self.build_conn_handler()?;
        let server = access.build(listen_addr.as_ref()).await?;
        Ok(server)
    }

    fn key(&self) -> &Arc<str> {
        &self.listen_addr
    }

    fn build_conn_handler(self) -> Result<Self::ConnHandler, Self::Err> {
        Ok(Socks5ServerUdpAccessConnHandler::new(
            self.proxy_group,
            self.speed_limit,
            self.udp_context,
        ))
    }
}

#[derive(Debug)]
pub struct Socks5ServerUdpAccessConnHandler {
    proxy_group: UdpProxyGroup,
    speed_limiter: Limiter,
    udp_context: UdpContext,
}
impl loading::HandleConn for Socks5ServerUdpAccessConnHandler {}
impl Socks5ServerUdpAccessConnHandler {
    pub fn new(proxy_group: UdpProxyGroup, speed_limit: f64, udp_context: UdpContext) -> Self {
        Self {
            proxy_group,
            speed_limiter: Limiter::new(speed_limit),
            udp_context,
        }
    }

    pub async fn build(self, listen_addr: impl ToSocketAddrs) -> io::Result<UdpServer<Self>> {
        let listener = tokio::net::UdpSocket::bind(listen_addr).await?;
        Ok(UdpServer::new(listener, self))
    }

    async fn proxy(&self, conn: Conn<UdpSocket, Flow, Packet>) -> Result<(), AccessProxyError> {
        // Connect to upstream
        let proxy_chain = self.proxy_group.choose_chain();
        let flow = conn.conn_key().clone();
        let upstream = UdpProxyClient::establish(
            proxy_chain.chain.clone(),
            flow.upstream.as_ref().unwrap().0.clone(),
            &self.udp_context,
        )
        .await?;
        let upstream_remote = upstream.remote_addr().clone();

        let (upstream_read, upstream_write) = upstream.into_split();

        let response_header = {
            let destination = flow.upstream.as_ref().unwrap().0.clone();
            move || {
                let mut wtr = Vec::new();
                let udp_request_header = UdpRequestHeader {
                    fragment: 0,
                    destination: destination.clone(),
                };
                futures::executor::block_on(async {
                    udp_request_header.encode(&mut wtr).await.unwrap();
                    wtr.into()
                })
            }
        };

        let speed_limiter = self.speed_limiter.clone();
        let payload_crypto = proxy_chain.payload_crypto.clone();
        let session_table = self.udp_context.session_table.clone();
        let upstream_local = upstream_read.inner().local_addr().ok();
        let (dn_read, dn_write) = conn.split();
        tokio::spawn(async move {
            let io_copy = CopyBidirectional {
                flow,
                upstream: UpstreamParts {
                    read: upstream_read,
                    write: upstream_write,
                },
                downstream: DownstreamParts {
                    read: dn_read,
                    write: dn_write,
                },
                speed_limiter,
                payload_crypto,
                response_header: Some(Box::new(response_header)),
            };
            let _ = io_copy
                .serve_as_access_server(session_table, upstream_local, upstream_remote, "SOCKS UDP")
                .await;
        });
        Ok(())
    }
}
#[derive(Debug, Error)]
pub enum AccessProxyError {
    #[error("Failed to establish proxy chain: {0}")]
    Establish(#[from] EstablishError),
}
impl UdpServerHandleConn for Socks5ServerUdpAccessConnHandler {
    fn parse_upstream_addr(&self, buf: &mut io::Cursor<&[u8]>) -> Option<Option<UpstreamAddr>> {
        let res = futures::executor::block_on(async move { UdpRequestHeader::decode(buf).await });
        let request_header = match res {
            Ok(header) => header,
            Err(e) => {
                warn!(?e, "Failed to decode UDP request header");
                return None;
            }
        };
        if request_header.fragment != 0 {
            return None;
        }

        Some(Some(UpstreamAddr(request_header.destination)))
    }

    async fn handle_flow(&self, accepted: Conn<UdpSocket, Flow, Packet>) {
        let res = self.proxy(accepted).await;
        match res {
            Ok(()) => (),
            Err(e) => {
                warn!(?e, "Failed to proxy");
            }
        }
    }
}
