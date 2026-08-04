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
    loading,
    matcher::Matcher,
    proto::{
        client::{stream::StreamTracer, udp::UdpTracer},
        context::Runtime,
    },
    route::{
        ConnSelector, ConnSelectorBuilder, ProbeRtt, Registries, RouteTable, RouteTableBuilder,
    },
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessServerStream {
    #[serde(default)]
    #[serde(alias = "proxy_table")]
    pub route_table: HashMap<Arc<str>, RouteTableBuilder>,
    #[serde(default)]
    #[serde(alias = "proxy_group")]
    pub conn_selector: HashMap<Arc<str>, ConnSelectorBuilder>,
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
    pub route_table: HashMap<Arc<str>, RouteTableBuilder>,
    #[serde(default)]
    #[serde(alias = "proxy_group")]
    pub conn_selector: HashMap<Arc<str>, ConnSelectorBuilder>,
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
    pub matcher: HashMap<Arc<str>, Matcher>,
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
    context: Runtime,
    stream_conn: &HashMap<Arc<str>, common::route::ConnConfig>,
    udp_conn: &HashMap<Arc<str>, common::route::ConnConfig>,
) -> AnyResult {
    let matcher = Arc::new(config.matcher);
    let stream_tracer: Arc<dyn ProbeRtt + Send + Sync> =
        Arc::new(StreamTracer::new(context.stream.clone()));
    let mut stream_registries = Registries {
        conn: stream_conn,
        matcher: &matcher,
        conn_selector: &HashMap::new(),
        tracer: &stream_tracer,
        connector_table: &context.stream.connector_table,
        cancellation: cancellation.clone(),
    };
    let stream_conn_selector =
        stream_conn_selector(&stream_registries, config.stream.conn_selector)?;
    stream_registries.conn_selector = &stream_conn_selector;
    let stream_route_tables = stream_route_tables(&stream_registries, config.stream.route_table)?;
    let udp_tracer: Arc<dyn ProbeRtt + Send + Sync> = Arc::new(UdpTracer::new(context.udp.clone()));
    let mut udp_registries = Registries {
        conn: udp_conn,
        matcher: &matcher,
        conn_selector: &HashMap::new(),
        tracer: &udp_tracer,
        connector_table: &context.stream.connector_table,
        cancellation: cancellation.clone(),
    };
    let udp_conn_selector = udp_conn_selector(&udp_registries, config.udp.conn_selector)?;
    udp_registries.conn_selector = &udp_conn_selector;
    deny_udp_route_table_key(&config.udp.route_table)?;
    #[rustfmt::skip] tcp_spawn_and_clean(config.tcp_server, &stream_conn_selector, &stream_registries, &context, loader, join_set).await?;
    #[rustfmt::skip] udp_spawn_and_clean(config.udp_server, &udp_conn_selector, &udp_registries, &context, loader, join_set).await?;
    #[rustfmt::skip] http_spawn_and_clean(config.http_server, &stream_route_tables, &stream_registries, &context, loader, join_set).await?;
    #[rustfmt::skip] socks5_tcp_spawn_and_clean(config.socks5_tcp_server, &stream_route_tables, &stream_registries, &context, loader, join_set).await?;
    #[rustfmt::skip] socks5_udp_spawn_and_clean(config.socks5_udp_server, &udp_conn_selector, &udp_registries, &context, loader, join_set).await?;
    Ok(())
}
fn deny_udp_route_table_key<T>(config: &HashMap<Arc<str>, T>) -> Result<(), AnyError> {
    if config.is_empty() {
        return Ok(());
    }
    let mut names = config.keys().map(|k| k.as_ref()).collect::<Vec<&str>>();
    names.sort_unstable();
    Err(format!("'access_server.udp.route_table' is not honored by any UDP access server: udp_server and socks5_udp_server select a chain through conn_selector only, so these tables would silently not apply: {}. Move the rules into a conn_selector, or remove them.", names.join(", ")).into())
}
fn forbid_reserved_selector_name(name: Arc<str>) -> Result<(), AnyError> {
    if matches!(name.as_ref(), "direct" | "block") {
        return Err(format!(
            "conn_selector key `{name}` is reserved: use the `direct`/`block` route action instead"
        )
        .into());
    }
    Ok(())
}
fn stream_conn_selector(
    registries: &Registries<'_>,
    config: HashMap<Arc<str>, ConnSelectorBuilder>,
) -> Result<HashMap<Arc<str>, ConnSelector>, AnyError> {
    let stream_conn_selector = config
        .into_iter()
        .map(|(k, v)| -> Result<(Arc<str>, ConnSelector), AnyError> {
            forbid_reserved_selector_name(k.clone())?;
            Ok((k, v.resolve(registries)?))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_conn_selector)
}
fn udp_conn_selector(
    registries: &Registries<'_>,
    config: HashMap<Arc<str>, ConnSelectorBuilder>,
) -> Result<HashMap<Arc<str>, ConnSelector>, AnyError> {
    let udp_conn_selector = config
        .into_iter()
        .map(|(k, v)| -> Result<(Arc<str>, ConnSelector), AnyError> {
            forbid_reserved_selector_name(k.clone())?;
            Ok((k, v.resolve(registries)?))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(udp_conn_selector)
}
fn stream_route_tables(
    registries: &Registries<'_>,
    config: HashMap<Arc<str>, RouteTableBuilder>,
) -> Result<HashMap<Arc<str>, RouteTable>, AnyError> {
    let stream_route_tables = config
        .into_iter()
        .map(|(k, v)| -> Result<(Arc<str>, RouteTable), AnyError> {
            Ok((k, v.resolve(registries)?))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_route_tables)
}
async fn tcp_spawn_and_clean(
    config: Vec<TcpAccessServerConfig>,
    stream_conn_selector: &HashMap<Arc<str>, ConnSelector>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let tcp_server = config
        .into_iter()
        .map(|c| c.into_builder(stream_conn_selector, registries, context.stream.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .tcp_server
        .spawn_and_clean(join_set, tcp_server)
        .await?;
    Ok(())
}
async fn udp_spawn_and_clean(
    config: Vec<UdpAccessServerConfig>,
    udp_conn_selector: &HashMap<Arc<str>, ConnSelector>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let udp_server = config
        .into_iter()
        .map(|c| c.into_builder(udp_conn_selector, registries, context.udp.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .udp_server
        .spawn_and_clean(join_set, udp_server)
        .await?;
    Ok(())
}
async fn http_spawn_and_clean(
    config: Vec<HttpAccessServerConfig>,
    stream_route_tables: &HashMap<Arc<str>, RouteTable>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let http_server = config
        .into_iter()
        .map(|c| c.into_builder(stream_route_tables, registries, context.stream.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .http_server
        .spawn_and_clean(join_set, http_server)
        .await?;
    Ok(())
}
async fn socks5_tcp_spawn_and_clean(
    config: Vec<Socks5ServerTcpAccessServerConfig>,
    stream_route_tables: &HashMap<Arc<str>, RouteTable>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let socks5_tcp_server = config
        .into_iter()
        .map(|c| c.into_builder(stream_route_tables, registries, context.stream.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .socks5_tcp_server
        .spawn_and_clean(join_set, socks5_tcp_server)
        .await?;
    Ok(())
}
async fn socks5_udp_spawn_and_clean(
    config: Vec<Socks5ServerUdpAccessServerConfig>,
    udp_conn_selector: &HashMap<Arc<str>, ConnSelector>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &mut AccessServerLoader,
    join_set: &mut tokio::task::JoinSet<AnyResult>,
) -> Result<(), AnyError> {
    let socks5_udp_server = config
        .into_iter()
        .map(|c| c.into_builder(udp_conn_selector, registries, context.udp.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    loader
        .socks5_udp_server
        .spawn_and_clean(join_set, socks5_udp_server)
        .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_udp_route_table_is_nothing_to_complain_about() {
        let config: AccessServerUdp = serde_json::from_str("{}").unwrap();
        deny_udp_route_table_key(&config.route_table).unwrap();
    }

    #[test]
    fn a_udp_route_table_that_cannot_apply_is_rejected() {
        let config: AccessServerUdp = serde_json::from_str("{\"route_table\": {\n\"lan\": [{\"matcher\": {}, \"action\": \"block\"}],\n\"default\": [{\"matcher\": {}, \"action\": \"block\"}]\n}}",).unwrap();
        let err = deny_udp_route_table_key(&config.route_table)
            .unwrap_err()
            .to_string();
        assert!(err.contains("default, lan"), "{err}");
    }
}
