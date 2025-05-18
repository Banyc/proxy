use std::{collections::HashMap, sync::Arc};

use common::{
    config::{Merge, merge_map},
    error::{AnyError, AnyResult},
    filter::{self, MatcherBuilder},
    loading,
    proto::{
        client::{stream::StreamTracerBuilder, udp::UdpTracerBuilder},
        context::Context,
        proxy_table::{
            StreamProxyConfig, StreamProxyGroup, StreamProxyGroupBuildContext,
            StreamProxyGroupBuilder, StreamProxyTable, StreamProxyTableBuildContext,
            StreamProxyTableBuilder, UdpProxyConfig, UdpProxyGroup, UdpProxyGroupBuildContext,
            UdpProxyGroupBuilder, UdpProxyTable, UdpProxyTableBuildContext, UdpProxyTableBuilder,
        },
    },
};
use serde::{Deserialize, Serialize};
use socks5::server::{
    tcp::{Socks5ServerTcpAccessConnHandler, Socks5ServerTcpAccessServerConfig},
    udp::{Socks5ServerUdpAccessConnHandler, Socks5ServerUdpAccessServerConfig},
};
use stream::streams::{
    http_tunnel::{HttpAccessConnHandler, HttpAccessServerConfig},
    tcp::{TcpAccessConnHandler, TcpAccessServerConfig},
};
use tokio_util::sync::CancellationToken;
use udp::{UdpAccessConnHandler, UdpAccessServerConfig};

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
    tcp_server: loading::Loader<TcpAccessConnHandler>,
    udp_server: loading::Loader<UdpAccessConnHandler>,
    http_server: loading::Loader<HttpAccessConnHandler>,
    socks5_tcp_server: loading::Loader<Socks5ServerTcpAccessConnHandler>,
    socks5_udp_server: loading::Loader<Socks5ServerUdpAccessConnHandler>,
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

pub async fn spawn_and_clean(
    config: AccessServerConfig,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
    loader: &mut AccessServerLoader,
    cancellation: CancellationToken,
    context: Context,
    stream_proxy_server: &HashMap<Arc<str>, StreamProxyConfig>,
    udp_proxy_server: &HashMap<Arc<str>, UdpProxyConfig>,
) -> AnyResult {
    let matcher = matcher(config.matcher)?;

    // Stream
    let stream_trace_builder = StreamTracerBuilder::new(context.stream.clone());
    let stream_proxy_group_cx = StreamProxyGroupBuildContext {
        proxy_server: stream_proxy_server,
        tracer_builder: &stream_trace_builder,
        cancellation: cancellation.clone(),
    };
    let stream_proxy_group = stream_proxy_group(&stream_proxy_group_cx, config.stream.proxy_group)?;
    let stream_proxy_table_cx = StreamProxyTableBuildContext {
        matcher: &matcher,
        proxy_group: &stream_proxy_group,
        proxy_group_cx: stream_proxy_group_cx.clone(),
    };
    let stream_proxy_tables =
        stream_proxy_tables(&stream_proxy_table_cx, config.stream.proxy_table)?;

    // UDP
    let udp_trace_builder = UdpTracerBuilder::new(context.udp.clone());
    let udp_proxy_group_cx = UdpProxyGroupBuildContext {
        proxy_server: udp_proxy_server,
        tracer_builder: &udp_trace_builder,
        cancellation: cancellation.clone(),
    };
    let udp_proxy_group = udp_proxy_group(&udp_proxy_group_cx, config.udp.proxy_group)?;
    let udp_proxy_table_cx = UdpProxyTableBuildContext {
        matcher: &matcher,
        proxy_group: &udp_proxy_group,
        proxy_group_cx: udp_proxy_group_cx.clone(),
    };
    let _udp_proxy_tables = udp_proxy_tables(&udp_proxy_table_cx, config.udp.proxy_table)?;

    #[rustfmt::skip] tcp_spawn_and_clean(config.tcp_server, &stream_proxy_group, &stream_proxy_group_cx, &context, loader, join_set).await?;
    #[rustfmt::skip] udp_spawn_and_clean(config.udp_server, &udp_proxy_group, &udp_proxy_group_cx, &context, loader, join_set).await?;
    #[rustfmt::skip] http_spawn_and_clean(config.http_server, &stream_proxy_tables, &stream_proxy_table_cx, &context, loader, join_set).await?;
    #[rustfmt::skip] socks5_tcp_spawn_and_clean(config.socks5_tcp_server, &stream_proxy_tables, &stream_proxy_table_cx, &context, loader, join_set) .await?;
    #[rustfmt::skip] socks5_udp_spawn_and_clean(config.socks5_udp_server, &udp_proxy_group, &udp_proxy_group_cx, &context, loader, join_set) .await?;
    Ok(())
}
fn matcher(
    config: HashMap<Arc<str>, MatcherBuilder>,
) -> Result<HashMap<Arc<str>, filter::Matcher>, AnyError> {
    let matcher = config
        .into_iter()
        .map(|(k, v)| match v.build() {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(matcher)
}
fn stream_proxy_group(
    stream_proxy_group_cx: &StreamProxyGroupBuildContext<'_>,
    config: HashMap<Arc<str>, StreamProxyGroupBuilder>,
) -> Result<HashMap<Arc<str>, StreamProxyGroup>, AnyError> {
    let stream_proxy_group = config
        .into_iter()
        .map(|(k, v)| match v.build(stream_proxy_group_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_proxy_group)
}
fn udp_proxy_group(
    udp_proxy_group_cx: &UdpProxyGroupBuildContext<'_>,
    config: HashMap<Arc<str>, UdpProxyGroupBuilder>,
) -> Result<HashMap<Arc<str>, UdpProxyGroup>, AnyError> {
    let udp_proxy_group = config
        .into_iter()
        .map(|(k, v)| match v.build(udp_proxy_group_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(udp_proxy_group)
}
fn stream_proxy_tables(
    stream_proxy_table_cx: &StreamProxyTableBuildContext<'_>,
    config: HashMap<Arc<str>, StreamProxyTableBuilder>,
) -> Result<HashMap<Arc<str>, StreamProxyTable>, AnyError> {
    let stream_proxy_tables = config
        .into_iter()
        .map(|(k, v)| match v.build(stream_proxy_table_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_proxy_tables)
}
fn udp_proxy_tables(
    udp_proxy_table_cx: &UdpProxyTableBuildContext<'_>,
    config: HashMap<Arc<str>, UdpProxyTableBuilder>,
) -> Result<HashMap<Arc<str>, UdpProxyTable>, AnyError> {
    let udp_proxy_tables = config
        .into_iter()
        .map(|(k, v)| match v.build(udp_proxy_table_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(udp_proxy_tables)
}
async fn tcp_spawn_and_clean(
    config: Vec<TcpAccessServerConfig>,
    stream_proxy_group: &HashMap<Arc<str>, StreamProxyGroup>,
    stream_proxy_group_cx: &StreamProxyGroupBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let tcp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                stream_proxy_group,
                stream_proxy_group_cx.clone(),
                context.stream.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .tcp_server
        .spawn_and_clean(join_set, tcp_server)
        .await?;
    Ok(())
}
async fn udp_spawn_and_clean(
    config: Vec<UdpAccessServerConfig>,
    udp_proxy_group: &HashMap<Arc<str>, UdpProxyGroup>,
    udp_proxy_group_cx: &UdpProxyGroupBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let udp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                udp_proxy_group,
                udp_proxy_group_cx.clone(),
                context.udp.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .udp_server
        .spawn_and_clean(join_set, udp_server)
        .await?;
    Ok(())
}
async fn http_spawn_and_clean(
    config: Vec<HttpAccessServerConfig>,
    stream_proxy_tables: &HashMap<Arc<str>, StreamProxyTable>,
    stream_proxy_table_cx: &StreamProxyTableBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let http_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                stream_proxy_tables,
                stream_proxy_table_cx.clone(),
                context.stream.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .http_server
        .spawn_and_clean(join_set, http_server)
        .await?;
    Ok(())
}
async fn socks5_tcp_spawn_and_clean(
    config: Vec<Socks5ServerTcpAccessServerConfig>,
    stream_proxy_tables: &HashMap<Arc<str>, StreamProxyTable>,
    stream_proxy_table_cx: &StreamProxyTableBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let socks5_tcp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                stream_proxy_tables,
                stream_proxy_table_cx.clone(),
                context.stream.clone(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .socks5_tcp_server
        .spawn_and_clean(join_set, socks5_tcp_server)
        .await?;
    Ok(())
}
async fn socks5_udp_spawn_and_clean(
    config: Vec<Socks5ServerUdpAccessServerConfig>,
    udp_proxy_group: &HashMap<Arc<str>, UdpProxyGroup>,
    udp_proxy_group_cx: &UdpProxyGroupBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let socks5_udp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                udp_proxy_group,
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
