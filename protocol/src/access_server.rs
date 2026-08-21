use std::{collections::HashMap, sync::Arc};

use crate::{
    socks5::server::{
        tcp::{
            Socks5ServerTcpAccessConnHandler, Socks5ServerTcpAccessServerBuilder,
            Socks5ServerTcpAccessServerConfig, Socks5TcpBuildError,
        },
        udp::{
            Socks5ServerUdpAccessConnHandler, Socks5ServerUdpAccessServerBuilder,
            Socks5ServerUdpAccessServerConfig, Socks5UdpBuildError,
        },
    },
    stream::streams::{
        http_tunnel::{
            HttpAccessConnHandler, HttpAccessServerBuilder, HttpAccessServerConfig, HttpBuildError,
        },
        tcp::access_server::{
            TcpAccessBuildError, TcpAccessConnHandler, TcpAccessServerBuilder,
            TcpAccessServerConfig,
        },
    },
    udp::access_server::{
        UdpAccessBuildError, UdpAccessConnHandler, UdpAccessServerBuilder, UdpAccessServerConfig,
    },
};
use common::{
    config::{Merge, merge_map},
    error::{AnyError, AnyResult},
    loading,
    matcher::Matcher,
    proxy_runtime::{
        client::{stream::StreamTracer, udp::UdpTracer},
        context::Runtime,
    },
    route::{
        ProbeFutures, ProbeRtt, Registries, RouteSelector, RouteSelectorBuilder, RouteTable,
        RouteTableBuilder,
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
    pub conn_selector: HashMap<Arc<str>, RouteSelectorBuilder>,
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
    pub conn_selector: HashMap<Arc<str>, RouteSelectorBuilder>,
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

    /// A read-only snapshot of the live loaders, for preparation. The
    /// snapshot resolves against the same live listeners but cannot commit.
    pub fn snapshot(&self) -> AccessServerLoaderSnapshot {
        AccessServerLoaderSnapshot {
            tcp_server: self.tcp_server.snapshot(),
            udp_server: self.udp_server.snapshot(),
            http_server: self.http_server.snapshot(),
            socks5_tcp_server: self.socks5_tcp_server.snapshot(),
            socks5_udp_server: self.socks5_udp_server.snapshot(),
        }
    }
}

/// An immutable snapshot of the live [`AccessServerLoader`]s, taken by
/// [`AccessServerLoader::snapshot`] for preparation. It can resolve and bind
/// builders against the live listener set, but it cannot commit —
/// replacement authority stays with the single owning loader.
pub struct AccessServerLoaderSnapshot {
    tcp_server: loading::LoaderSnapshot<TcpAccessConnHandler>,
    udp_server: loading::LoaderSnapshot<UdpAccessConnHandler>,
    http_server: loading::LoaderSnapshot<HttpAccessConnHandler>,
    socks5_tcp_server: loading::LoaderSnapshot<Socks5ServerTcpAccessConnHandler>,
    socks5_udp_server: loading::LoaderSnapshot<Socks5ServerUdpAccessConnHandler>,
}

impl AccessServerLoader {
    /// Commit a previously-prepared access-server reload: hot-swap handlers
    /// on existing listeners, spawn new listener tasks, and drop handles for
    /// removed listeners. Returns an error if a listener died between
    /// prepare and commit (a handler update would be silently lost).
    pub fn commit(
        &mut self,
        join_set: &mut tokio::task::JoinSet<AnyResult>,
        prepared: PreparedAccessServer,
    ) -> AnyResult {
        // Spawn this generation's probe futures into the server-owned,
        // actively-reaped task set ONLY at commit: nothing runs during
        // prepare, so a failed or abandoned prepare cannot lose a probe
        // panic. Unwrapping surfaces panics via the join set.
        let probe_futures = prepared.probe_futures;
        for fut in probe_futures.into_futures() {
            join_set.spawn(async move {
                fut.await;
                Ok(())
            });
        }
        self.tcp_server.commit(join_set, prepared.tcp_server)?;
        self.udp_server.commit(join_set, prepared.udp_server)?;
        self.http_server.commit(join_set, prepared.http_server)?;
        self.socks5_tcp_server
            .commit(join_set, prepared.socks5_tcp_server)?;
        self.socks5_udp_server
            .commit(join_set, prepared.socks5_udp_server)?;
        Ok(())
    }
}

/// A fully-prepared access-server reload: resolved route selectors/tables,
/// bound listener sockets, and built handlers for every access-server kind,
/// ready to commit. Dropping it without [`AccessServerLoader::commit`] drops
/// the bound sockets and the unspawned probe futures — nothing has started
/// running yet — so live state is untouched.
pub struct PreparedAccessServer {
    tcp_server: loading::PreparedOps<TcpAccessConnHandler>,
    udp_server: loading::PreparedOps<UdpAccessConnHandler>,
    http_server: loading::PreparedOps<HttpAccessConnHandler>,
    socks5_tcp_server: loading::PreparedOps<Socks5ServerTcpAccessConnHandler>,
    socks5_udp_server: loading::PreparedOps<Socks5ServerUdpAccessConnHandler>,
    probe_futures: ProbeFutures,
}

/// Prepare an access-server reload: resolve conn selectors and route tables
/// (collecting probe futures tied to `cancellation`), bind every new
/// listener, and build every handler — all without touching live state. On
/// any failure the returned `Err` drops everything already prepared (bound
/// sockets and unspawned probe futures — nothing has started running),
/// leaving the live configuration untouched.
pub async fn prepare(
    config: AccessServerConfig,
    loader: &AccessServerLoaderSnapshot,
    cancellation: CancellationToken,
    context: Runtime,
    stream_conn: &HashMap<Arc<str>, common::route::HopConfig>,
    udp_conn: &HashMap<Arc<str>, common::route::HopConfig>,
) -> Result<PreparedAccessServer, AnyError> {
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
    // Probe futures are collected during prepare (still tied to
    // `cancellation` via the captured token) and spawned only at commit.
    let mut probe_futures = common::route::ProbeFutures::new();
    let stream_conn_selector = stream_conn_selector(
        &stream_registries,
        config.stream.conn_selector,
        &mut probe_futures,
    )?;
    stream_registries.conn_selector = &stream_conn_selector;
    let stream_route_tables = stream_route_tables(
        &stream_registries,
        config.stream.route_table,
        &mut probe_futures,
    )?;
    let udp_tracer: Arc<dyn ProbeRtt + Send + Sync> = Arc::new(UdpTracer::new(context.udp.clone()));
    let mut udp_registries = Registries {
        conn: udp_conn,
        matcher: &matcher,
        conn_selector: &HashMap::new(),
        tracer: &udp_tracer,
        connector_table: &context.stream.connector_table,
        cancellation: cancellation.clone(),
    };
    let udp_conn_selector = udp_conn_selector(
        &udp_registries,
        config.udp.conn_selector,
        &mut probe_futures,
    )?;
    udp_registries.conn_selector = &udp_conn_selector;
    deny_udp_route_table_key(&config.udp.route_table)?;
    let tcp_server = tcp_prepare(
        config.tcp_server,
        &stream_conn_selector,
        &stream_registries,
        &context,
        &loader.tcp_server,
        &mut probe_futures,
    )
    .await?;
    let udp_server = udp_prepare(
        config.udp_server,
        &udp_conn_selector,
        &udp_registries,
        &context,
        &loader.udp_server,
        &mut probe_futures,
    )
    .await?;
    let http_server = http_prepare(
        config.http_server,
        &stream_route_tables,
        &stream_registries,
        &context,
        &loader.http_server,
        &mut probe_futures,
    )
    .await?;
    let socks5_tcp_server = socks5_tcp_prepare(
        config.socks5_tcp_server,
        &stream_route_tables,
        &stream_registries,
        &context,
        &loader.socks5_tcp_server,
        &mut probe_futures,
    )
    .await?;
    let socks5_udp_server = socks5_udp_prepare(
        config.socks5_udp_server,
        &udp_conn_selector,
        &udp_registries,
        &context,
        &loader.socks5_udp_server,
        &mut probe_futures,
    )
    .await?;
    Ok(PreparedAccessServer {
        tcp_server,
        udp_server,
        http_server,
        socks5_tcp_server,
        socks5_udp_server,
        probe_futures,
    })
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
    config: HashMap<Arc<str>, RouteSelectorBuilder>,
    probes: &mut ProbeFutures,
) -> Result<HashMap<Arc<str>, RouteSelector>, AnyError> {
    let stream_conn_selector = config
        .into_iter()
        .map(|(k, v)| -> Result<(Arc<str>, RouteSelector), AnyError> {
            forbid_reserved_selector_name(k.clone())?;
            let selector = v.resolve(registries, probes)?;
            Ok((k, selector))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_conn_selector)
}
fn udp_conn_selector(
    registries: &Registries<'_>,
    config: HashMap<Arc<str>, RouteSelectorBuilder>,
    probes: &mut ProbeFutures,
) -> Result<HashMap<Arc<str>, RouteSelector>, AnyError> {
    let udp_conn_selector = config
        .into_iter()
        .map(|(k, v)| -> Result<(Arc<str>, RouteSelector), AnyError> {
            forbid_reserved_selector_name(k.clone())?;
            let selector = v.resolve(registries, probes)?;
            Ok((k, selector))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(udp_conn_selector)
}
fn stream_route_tables(
    registries: &Registries<'_>,
    config: HashMap<Arc<str>, RouteTableBuilder>,
    probes: &mut ProbeFutures,
) -> Result<HashMap<Arc<str>, RouteTable>, AnyError> {
    let stream_route_tables = config
        .into_iter()
        .map(|(k, v)| -> Result<(Arc<str>, RouteTable), AnyError> {
            let table = v.resolve(registries, probes)?;
            Ok((k, table))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(stream_route_tables)
}
async fn tcp_prepare(
    config: Vec<TcpAccessServerConfig>,
    stream_conn_selector: &HashMap<Arc<str>, RouteSelector>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &loading::LoaderSnapshot<TcpAccessConnHandler>,
    probes: &mut ProbeFutures,
) -> Result<loading::PreparedOps<TcpAccessConnHandler>, AnyError> {
    let builders = config
        .into_iter()
        .map(|c| -> Result<TcpAccessServerBuilder, TcpAccessBuildError> {
            c.into_builder(
                stream_conn_selector,
                registries,
                context.stream.clone(),
                probes,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = loader.prepare(builders).await?;
    Ok(prepared)
}
async fn udp_prepare(
    config: Vec<UdpAccessServerConfig>,
    udp_conn_selector: &HashMap<Arc<str>, RouteSelector>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &loading::LoaderSnapshot<UdpAccessConnHandler>,
    probes: &mut ProbeFutures,
) -> Result<loading::PreparedOps<UdpAccessConnHandler>, AnyError> {
    let builders = config
        .into_iter()
        .map(|c| -> Result<UdpAccessServerBuilder, UdpAccessBuildError> {
            c.into_builder(udp_conn_selector, registries, context.udp.clone(), probes)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = loader.prepare(builders).await?;
    Ok(prepared)
}
async fn http_prepare(
    config: Vec<HttpAccessServerConfig>,
    stream_route_tables: &HashMap<Arc<str>, RouteTable>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &loading::LoaderSnapshot<HttpAccessConnHandler>,
    probes: &mut ProbeFutures,
) -> Result<loading::PreparedOps<HttpAccessConnHandler>, AnyError> {
    let builders = config
        .into_iter()
        .map(|c| -> Result<HttpAccessServerBuilder, HttpBuildError> {
            c.into_builder(
                stream_route_tables,
                registries,
                context.stream.clone(),
                probes,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = loader.prepare(builders).await?;
    Ok(prepared)
}
async fn socks5_tcp_prepare(
    config: Vec<Socks5ServerTcpAccessServerConfig>,
    stream_route_tables: &HashMap<Arc<str>, RouteTable>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &loading::LoaderSnapshot<Socks5ServerTcpAccessConnHandler>,
    probes: &mut ProbeFutures,
) -> Result<loading::PreparedOps<Socks5ServerTcpAccessConnHandler>, AnyError> {
    let builders = config
        .into_iter()
        .map(
            |c| -> Result<Socks5ServerTcpAccessServerBuilder, Socks5TcpBuildError> {
                c.into_builder(
                    stream_route_tables,
                    registries,
                    context.stream.clone(),
                    probes,
                )
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = loader.prepare(builders).await?;
    Ok(prepared)
}
async fn socks5_udp_prepare(
    config: Vec<Socks5ServerUdpAccessServerConfig>,
    udp_conn_selector: &HashMap<Arc<str>, RouteSelector>,
    registries: &Registries<'_>,
    context: &Runtime,
    loader: &loading::LoaderSnapshot<Socks5ServerUdpAccessConnHandler>,
    probes: &mut ProbeFutures,
) -> Result<loading::PreparedOps<Socks5ServerUdpAccessConnHandler>, AnyError> {
    let builders = config
        .into_iter()
        .map(
            |c| -> Result<Socks5ServerUdpAccessServerBuilder, Socks5UdpBuildError> {
                c.into_builder(udp_conn_selector, registries, context.udp.clone(), probes)
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = loader.prepare(builders).await?;
    Ok(prepared)
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
