use std::{collections::HashMap, sync::Arc};

use common::{
    config::{merge_map, Merge},
    error::{AnyError, AnyResult},
    filter::{self, MatcherBuilder},
    loading,
    udp::proxy_table::{UdpProxyConfig, UdpProxyGroupBuilder, UdpProxyTableBuilder},
};
use protocol::{
    context::ConcreteContext,
    stream::proxy_table::{StreamProxyConfig, StreamProxyGroupBuilder, StreamProxyTableBuilder},
};
use proxy_client::{stream::StreamTracerBuilder, udp::UdpTracerBuilder};
use serde::{Deserialize, Serialize};
use socks5::server::{
    tcp::{Socks5ServerTcpAccess, Socks5ServerTcpAccessServerConfig},
    udp::{Socks5ServerUdpAccess, Socks5ServerUdpAccessServerConfig},
};
use stream::{
    proxy_table::{StreamProxyGroupBuildContext, StreamProxyTableBuildContext},
    streams::{
        http_tunnel::{HttpAccess, HttpAccessServerConfig},
        tcp::{TcpAccess, TcpAccessServerConfig},
    },
};
use tokio_util::sync::CancellationToken;
use udp::{
    proxy_table::{UdpProxyGroupBuildContext, UdpProxyTableBuildContext},
    UdpAccess, UdpAccessServerConfig,
};

pub mod socks5;
pub mod stream;
pub mod udp;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessServerStream {
    #[serde(default)]
    pub proxy_table: HashMap<Arc<str>, StreamProxyTableBuilder>,
    #[serde(default)]
    pub proxy_group: HashMap<Arc<str>, StreamProxyGroupBuilder>,
}
impl Merge for AccessServerStream {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let proxy_table = merge_map(self.proxy_table, other.proxy_table)?;
        let proxy_group = merge_map(self.proxy_group, other.proxy_group)?;
        Ok(Self {
            proxy_table,
            proxy_group,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessServerUdp {
    #[serde(default)]
    pub proxy_table: HashMap<Arc<str>, UdpProxyTableBuilder>,
    #[serde(default)]
    pub proxy_group: HashMap<Arc<str>, UdpProxyGroupBuilder>,
}
impl Merge for AccessServerUdp {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let proxy_table = merge_map(self.proxy_table, other.proxy_table)?;
        let proxy_group = merge_map(self.proxy_group, other.proxy_group)?;
        Ok(Self {
            proxy_table,
            proxy_group,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessServerConfig {
    #[serde(default)]
    pub tcp_server: Vec<TcpAccessServerConfig>,
    #[serde(default)]
    pub udp_server: Vec<UdpAccessServerConfig>,
    #[serde(default)]
    pub http_server: Vec<HttpAccessServerConfig>,
    #[serde(default)]
    pub socks5_tcp_server: Vec<Socks5ServerTcpAccessServerConfig>,
    #[serde(default)]
    pub socks5_udp_server: Vec<Socks5ServerUdpAccessServerConfig>,
    #[serde(default)]
    stream: AccessServerStream,
    #[serde(default)]
    udp: AccessServerUdp,
    #[serde(default)]
    pub matcher: HashMap<Arc<str>, MatcherBuilder>,
}
impl AccessServerConfig {
    pub fn new() -> AccessServerConfig {
        AccessServerConfig {
            tcp_server: Default::default(),
            udp_server: Default::default(),
            http_server: Default::default(),
            socks5_tcp_server: Default::default(),
            socks5_udp_server: Default::default(),
            stream: Default::default(),
            udp: Default::default(),
            matcher: Default::default(),
        }
    }

    pub async fn spawn_and_clean(
        self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        loader: &mut AccessServerLoader,
        cancellation: CancellationToken,
        context: ConcreteContext,
        stream_proxy_server: &HashMap<Arc<str>, StreamProxyConfig>,
        udp_proxy_server: &HashMap<Arc<str>, UdpProxyConfig>,
    ) -> AnyResult {
        // Shared
        let matcher: HashMap<Arc<str>, filter::Matcher> = self
            .matcher
            .into_iter()
            .map(|(k, v)| match v.build() {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        // Stream
        let stream_trace_builder = StreamTracerBuilder::new(context.stream.clone());
        let stream_proxy_group_cx = StreamProxyGroupBuildContext {
            proxy_server: stream_proxy_server,
            tracer_builder: &stream_trace_builder,
            cancellation: cancellation.clone(),
        };
        let stream_proxy_group = self
            .stream
            .proxy_group
            .into_iter()
            .map(|(k, v)| match v.build(stream_proxy_group_cx.clone()) {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        let stream_proxy_table_cx = StreamProxyTableBuildContext {
            matcher: &matcher,
            proxy_group: &stream_proxy_group,
            proxy_group_cx: stream_proxy_group_cx.clone(),
        };
        let stream_proxy_tables = self
            .stream
            .proxy_table
            .into_iter()
            .map(|(k, v)| match v.build(stream_proxy_table_cx.clone()) {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        // UDP
        let udp_trace_builder = UdpTracerBuilder::new();
        let udp_proxy_group_cx = UdpProxyGroupBuildContext {
            proxy_server: udp_proxy_server,
            tracer_builder: &udp_trace_builder,
            cancellation: cancellation.clone(),
        };
        let udp_proxy_group = self
            .udp
            .proxy_group
            .into_iter()
            .map(|(k, v)| match v.build(udp_proxy_group_cx.clone()) {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()?;
        let udp_proxy_table_cx = UdpProxyTableBuildContext {
            matcher: &matcher,
            proxy_group: &udp_proxy_group,
            proxy_group_cx: udp_proxy_group_cx.clone(),
        };
        let _udp_proxy_tables = self
            .udp
            .proxy_table
            .into_iter()
            .map(|(k, v)| match v.build(udp_proxy_table_cx.clone()) {
                Ok(v) => Ok((k, v)),
                Err(e) => Err(e),
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        // TCP servers
        let tcp_server = self
            .tcp_server
            .into_iter()
            .map(|c| {
                c.into_builder(
                    &stream_proxy_group,
                    stream_proxy_group_cx.clone(),
                    context.stream.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        loader
            .tcp_server
            .spawn_and_clean(join_set, tcp_server)
            .await?;

        // UDP servers
        let udp_server = self
            .udp_server
            .into_iter()
            .map(|c| {
                c.into_builder(
                    &udp_proxy_group,
                    udp_proxy_group_cx.clone(),
                    context.udp.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        loader
            .udp_server
            .spawn_and_clean(join_set, udp_server)
            .await?;

        // HTTP servers
        let http_server = self
            .http_server
            .into_iter()
            .map(|c| {
                c.into_builder(
                    &stream_proxy_tables,
                    stream_proxy_table_cx.clone(),
                    context.stream.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        loader
            .http_server
            .spawn_and_clean(join_set, http_server)
            .await?;

        // SOCKS5 TCP servers
        let socks5_tcp_server = self
            .socks5_tcp_server
            .into_iter()
            .map(|c| {
                c.into_builder(
                    &stream_proxy_tables,
                    stream_proxy_table_cx.clone(),
                    context.stream.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        loader
            .socks5_tcp_server
            .spawn_and_clean(join_set, socks5_tcp_server)
            .await?;

        // SOCKS5 UDP servers
        let socks5_udp_server = self
            .socks5_udp_server
            .into_iter()
            .map(|c| {
                c.into_builder(
                    &udp_proxy_group,
                    udp_proxy_group_cx.clone(),
                    context.udp.clone(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        loader
            .socks5_udp_server
            .spawn_and_clean(join_set, socks5_udp_server)
            .await?;

        Ok(())
    }
}
impl Merge for AccessServerConfig {
    type Error = AnyError;

    fn merge(mut self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        self.tcp_server.extend(other.tcp_server);
        self.udp_server.extend(other.udp_server);
        self.http_server.extend(other.http_server);
        self.socks5_tcp_server.extend(other.socks5_tcp_server);
        self.socks5_udp_server.extend(other.socks5_udp_server);
        let stream = self.stream.merge(other.stream)?;
        let udp = self.udp.merge(other.udp)?;
        let matcher = merge_map(self.matcher, other.matcher)?;
        Ok(Self {
            tcp_server: self.tcp_server,
            udp_server: self.udp_server,
            http_server: self.http_server,
            socks5_tcp_server: self.socks5_tcp_server,
            socks5_udp_server: self.socks5_udp_server,
            stream,
            udp,
            matcher,
        })
    }
}

#[derive(Default)]
pub struct AccessServerLoader {
    tcp_server: loading::Loader<TcpAccess>,
    udp_server: loading::Loader<UdpAccess>,
    http_server: loading::Loader<HttpAccess>,
    socks5_tcp_server: loading::Loader<Socks5ServerTcpAccess>,
    socks5_udp_server: loading::Loader<Socks5ServerUdpAccess>,
}
impl AccessServerLoader {
    pub fn new() -> Self {
        Self {
            tcp_server: loading::Loader::new(),
            udp_server: loading::Loader::new(),
            http_server: loading::Loader::new(),
            socks5_tcp_server: loading::Loader::new(),
            socks5_udp_server: loading::Loader::new(),
        }
    }
}
