use std::{collections::HashMap, sync::Arc};

use crate::{
    socks5::server::{
        tcp::{Socks5ServerTcpAccessConnHandler, Socks5ServerTcpAccessServerConfig},
        udp::{Socks5ServerUdpAccessConnHandler, Socks5ServerUdpAccessServerConfig},
    },
    stream::streams::{
        http_tunnel::{HttpAccessConnHandler, HttpAccessServerConfig},
        tcp::access_server::{TcpAccessConnHandler, TcpAccessServerConfig},
    },
    udp::access_server::{UdpAccessConnHandler, UdpAccessServerConfig},
};
use common::{
    config::{Merge, merge_map},
    error::{AnyError, AnyResult},
    filter::{self, MatcherBuilder},
    loading,
    proto::{
        client::{stream::StreamTracerBuilder, udp::UdpTracerBuilder},
        context::Context,
        route::{
            StreamConnConfig, StreamConnSelectorBuildContext, StreamConnSelectorBuilder,
            StreamRouteGroup, StreamRouteTable, StreamRouteTableBuildContext,
            StreamRouteTableBuilder, UdpConnConfig, UdpConnSelector, UdpConnSelectorBuildContext,
            UdpConnSelectorBuilder, UdpRouteTable, UdpRouteTableBuildContext, UdpRouteTableBuilder,
        },
    },
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessServerStream {
    #[serde(default)]
    #[serde(alias = "proxy_table")]
    pub route_table: HashMap<Arc<str>, StreamRouteTableBuilder>,
    #[serde(default)]
    #[serde(alias = "proxy_group")]
    pub conn_selector: HashMap<Arc<str>, StreamConnSelectorBuilder>,
}
impl Merge for AccessServerStream {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let route_table = merge_map(self.route_table, other.route_table)?;
        let conn_selector = merge_map(self.conn_selector, other.conn_selector)?;
        Ok(Self {
            route_table,
            conn_selector,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessServerUdp {
    #[serde(default)]
    #[serde(alias = "proxy_table")]
    pub route_table: HashMap<Arc<str>, UdpRouteTableBuilder>,
    #[serde(default)]
    #[serde(alias = "proxy_group")]
    pub conn_selector: HashMap<Arc<str>, UdpConnSelectorBuilder>,
}
impl Merge for AccessServerUdp {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let route_table = merge_map(self.route_table, other.route_table)?;
        let conn_selector = merge_map(self.conn_selector, other.conn_selector)?;
        Ok(Self {
            route_table,
            conn_selector,
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
    stream_conn: &HashMap<Arc<str>, StreamConnConfig>,
    udp_conn: &HashMap<Arc<str>, UdpConnConfig>,
) -> AnyResult {
    let matcher = Arc::new(matcher(config.matcher)?);

    // Stream
    let stream_trace_builder = StreamTracerBuilder::new(context.stream.clone());
    let stream_conn_selector_cx = StreamConnSelectorBuildContext {
        conn: stream_conn,
        tracer_builder: &stream_trace_builder,
        cancellation: cancellation.clone(),
    };
    let stream_conn_selector =
        stream_conn_selector(&stream_conn_selector_cx, config.stream.conn_selector)?;
    let stream_route_table_cx = StreamRouteTableBuildContext {
        matcher: &matcher,
        conn_selector: &stream_conn_selector,
        conn_selector_cx: stream_conn_selector_cx.clone(),
    };
    let stream_route_tables =
        stream_route_tables(&stream_route_table_cx, config.stream.route_table)?;

    // UDP
    let udp_trace_builder = UdpTracerBuilder::new(context.udp.clone());
    let udp_conn_selector_cx = UdpConnSelectorBuildContext {
        conn: udp_conn,
        tracer_builder: &udp_trace_builder,
        cancellation: cancellation.clone(),
    };
    let udp_conn_selector = udp_conn_selector(&udp_conn_selector_cx, config.udp.conn_selector)?;
    let udp_route_table_cx = UdpRouteTableBuildContext {
        matcher: &matcher,
        conn_selector: &udp_conn_selector,
        conn_selector_cx: udp_conn_selector_cx.clone(),
    };
    let _udp_route_tables = udp_route_tables(&udp_route_table_cx, config.udp.route_table)?;

    #[rustfmt::skip] tcp_spawn_and_clean(config.tcp_server, &stream_conn_selector, &stream_conn_selector_cx, &context, loader, join_set).await?;
    #[rustfmt::skip] udp_spawn_and_clean(config.udp_server, &udp_conn_selector, &udp_conn_selector_cx, &context, loader, join_set).await?;
    #[rustfmt::skip] http_spawn_and_clean(config.http_server, &stream_route_tables, &stream_route_table_cx, &context, loader, join_set).await?;
    #[rustfmt::skip] socks5_tcp_spawn_and_clean(config.socks5_tcp_server, &stream_route_tables, &stream_route_table_cx, &context, loader, join_set) .await?;
    #[rustfmt::skip] socks5_udp_spawn_and_clean(config.socks5_udp_server, &udp_conn_selector, &udp_conn_selector_cx, &context, loader, join_set) .await?;
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
fn stream_conn_selector(
    stream_conn_selector_cx: &StreamConnSelectorBuildContext<'_>,
    config: HashMap<Arc<str>, StreamConnSelectorBuilder>,
) -> Result<HashMap<Arc<str>, StreamRouteGroup>, AnyError> {
    let stream_conn_selector = config
        .into_iter()
        .map(|(k, v)| match v.build(stream_conn_selector_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_conn_selector)
}
fn udp_conn_selector(
    udp_conn_selector_cx: &UdpConnSelectorBuildContext<'_>,
    config: HashMap<Arc<str>, UdpConnSelectorBuilder>,
) -> Result<HashMap<Arc<str>, UdpConnSelector>, AnyError> {
    let udp_conn_selector = config
        .into_iter()
        .map(|(k, v)| match v.build(udp_conn_selector_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(udp_conn_selector)
}
fn stream_route_tables(
    stream_route_table_cx: &StreamRouteTableBuildContext<'_>,
    config: HashMap<Arc<str>, StreamRouteTableBuilder>,
) -> Result<HashMap<Arc<str>, StreamRouteTable>, AnyError> {
    let stream_route_tables = config
        .into_iter()
        .map(|(k, v)| match v.build(stream_route_table_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_route_tables)
}
fn udp_route_tables(
    udp_route_table_cx: &UdpRouteTableBuildContext<'_>,
    config: HashMap<Arc<str>, UdpRouteTableBuilder>,
) -> Result<HashMap<Arc<str>, UdpRouteTable>, AnyError> {
    let udp_route_tables = config
        .into_iter()
        .map(|(k, v)| match v.build(udp_route_table_cx.clone()) {
            Ok(v) => Ok((k, v)),
            Err(e) => Err(e),
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(udp_route_tables)
}
async fn tcp_spawn_and_clean(
    config: Vec<TcpAccessServerConfig>,
    stream_conn_selector: &HashMap<Arc<str>, StreamRouteGroup>,
    stream_conn_selector_cx: &StreamConnSelectorBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let tcp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                stream_conn_selector,
                stream_conn_selector_cx.clone(),
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
    udp_conn_selector: &HashMap<Arc<str>, UdpConnSelector>,
    udp_conn_selector_cx: &UdpConnSelectorBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let udp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                udp_conn_selector,
                udp_conn_selector_cx.clone(),
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
    stream_route_tables: &HashMap<Arc<str>, StreamRouteTable>,
    stream_route_table_cx: &StreamRouteTableBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let http_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                stream_route_tables,
                stream_route_table_cx.clone(),
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
    stream_route_tables: &HashMap<Arc<str>, StreamRouteTable>,
    stream_route_table_cx: &StreamRouteTableBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let socks5_tcp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                stream_route_tables,
                stream_route_table_cx.clone(),
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
    udp_conn_selector: &HashMap<Arc<str>, UdpConnSelector>,
    udp_conn_selector_cx: &UdpConnSelectorBuildContext<'_>,
    context: &Context,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let socks5_udp_server = config
        .into_iter()
        .map(|c| {
            c.into_builder(
                udp_conn_selector,
                udp_conn_selector_cx.clone(),
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
