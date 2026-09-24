#![warn(clippy::disallowed_methods, clippy::disallowed_types)]

use std::{collections::HashMap, sync::Arc};

use access_server::{AccessServerConfig, AccessServerLoader, AccessServerLoaderSnapshot};
use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_WINDOW},
    config::{Merge, merge_map},
    connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
    error::AnyError,
    lifecycle::retention::RetentionActorSender,
    lifecycle::suspend::SystemResumeSignal,
    proxy_runtime::{
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
        metrics::{stream::StreamSessionTable, udp::UdpSessionTable},
    },
    route::HopConfig,
    session::SessionSpawner,
    stream_runtime::pool::{StreamConnPool, StreamPoolBuilder},
};
use config::ReadConfig;
use protocol::{
    access_server::{self},
    proxy_server::{ProxyServerConfig, ProxyServerLoader, ProxyServerLoaderSnapshot},
    reverse_tunnel::{ReverseTunnelConfig, ReverseTunnelLoader, ReverseTunnelLoaderSnapshot},
    stream_proto::connect::build_concrete_stream_connector_table,
};
use serde::Deserialize;
use swap::Swap;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::config::ConfigChangeSignal;

pub mod config;
pub mod monitor;
pub mod profiling;
pub mod reload;

pub struct ServeContext {
    pub stream_session_table: Option<StreamSessionTable>,
    pub udp_session_table: Option<UdpSessionTable>,
    pub config_changed: ConfigChangeSignal,
    pub system_resume: SystemResumeSignal,
    pub retention: RetentionActorSender,
}
impl ServeContext {
    /// The connector-reset authority.
    ///
    /// Firing it tears down every pooled mux session, because a system resume
    /// invalidates the connections underneath them. A configuration change
    /// must not: `commit_reload` replaces the connector configuration in place
    /// and live sessions keep serving. The two authorities are separate
    /// broadcasts, and this is the single place the reset signal is derived,
    /// so the connector table can only ever be handed the resume signal.
    fn connector_reset(&self) -> ConnectorResetSignal {
        ConnectorResetSignal(self.system_resume.0.clone())
    }
}

/// The validator `serve` judges UDP route headers with. The window is the one
/// `common::anti_replay` derives for every UDP-path validator, so the client's
/// own validator cannot end up judging the same header against a different
/// window.
fn udp_time_validator() -> TimeValidator {
    TimeValidator::new(VALIDATOR_UDP_WINDOW)
}

pub async fn serve<CR>(
    config_reader: CR,
    serve_context: ServeContext,
) -> Result<(), ServerServeError>
where
    CR: ReadConfig<Config = ServerConfig> + Send + Sync + 'static,
{
    // Reload machinery now lives in `crate::reload`; pull the items used by
    // this serve loop into scope.
    use crate::reload::{
        ReloadStep, ServerReloadMachine, commit_reload, drive_reload, prepare_reload,
    };

    let config_reader = Arc::new(config_reader);
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    let mut sessions = tokio::task::JoinSet::new();
    let mut server_loader = ServerLoader {
        access_server: AccessServerLoader::new(),
        proxy_server: ProxyServerLoader::new(),
        reverse_tunnel: ReverseTunnelLoader::new(),
    };
    let mut server_tasks = tokio::task::JoinSet::new();

    let stream_pool = Swap::new(StreamConnPool::empty());
    let stream_validator = Arc::new(ReplayValidator::new(
        VALIDATOR_TIME_FRAME,
        VALIDATOR_CAPACITY,
    ));
    let udp_validator = Arc::new(udp_time_validator());
    let connector_reset = serve_context.connector_reset();
    // One connector-configuration cell shared by the stream connector table,
    // the UDP connector, and every mux UDP dialer: a reload replaces it in a
    // single write, so stream and UDP connectors can never observe different
    // configurations. The reload path holds the sole updater; every
    // connector holds a reader clone.
    let (connector_config_reader, connector_config_updater) =
        connector_config_cell(ConnectorConfig::default());
    let udp_connector = Arc::new(UdpConnector::new(connector_config_reader.clone()));
    let stream_connector_table = Arc::new(build_concrete_stream_connector_table(
        connector_config_reader.clone(),
        connector_reset,
        &mut server_tasks,
        &udp_connector,
    ));
    let runtime = Runtime {
        session_spawner: session_spawner.clone(),
        stream: StreamRuntime {
            session_table: serve_context.stream_session_table,
            pool: stream_pool,
            connector_table: stream_connector_table,
            replay_validator: Arc::clone(&stream_validator),
            session_spawner: session_spawner.clone(),
            retention: serve_context.retention.clone(),
        },
        udp: UdpRuntime {
            session_table: serve_context.udp_session_table,
            time_validator: Arc::clone(&udp_validator),
            connector: udp_connector,
            session_spawner: session_spawner.clone(),
            retention: serve_context.retention.clone(),
        },
    };

    let cancellation = CancellationToken::new();
    // Initial configuration preparation: race the first preparation against
    // the connector drivers already spawned into `server_tasks`, so a
    // connector-driver panic or failure during startup surfaces immediately
    // instead of parking until the serve loop begins.
    let prepared = {
        let prepare = prepare_reload(
            Arc::clone(&config_reader),
            server_loader.snapshot(),
            cancellation.clone(),
            runtime.clone(),
        );
        tokio::pin!(prepare);
        loop {
            tokio::select! {
                res = &mut prepare => break res?,
                Some(res) = server_tasks.join_next() => {
                    // Surface connector-driver failures and panics during
                    // startup instead of parking them.
                    res.unwrap().map_err(ServerServeError::ServerTask)?;
                }
            }
        }
    };
    let (guard, commit_error) = commit_reload(
        &mut server_tasks,
        &mut server_loader,
        prepared,
        &runtime,
        &connector_config_updater,
    );
    if let Some(e) = commit_error {
        return Err(ServerServeError::Commit(e));
    }
    let mut _cancellation_guard = guard;
    let mut config_changed = serve_context.config_changed.subscription();
    let mut reload = ServerReloadMachine::new();

    let outcome = loop {
        tokio::select! {
            Some(fut) = session_rx.recv() => {
                sessions.spawn(fut);
            }
            Some(res) = sessions.join_next() => {
                if let Err(error) = res.unwrap() {
                    error!(?error, "Session task returned an error");
                }
            }
            step = drive_reload(&mut reload, &mut server_tasks, &mut config_changed) => {
                match step {
                    Ok(ReloadStep::ConfigChanged) => {
                        info!("Config file changed");
                    }
                    Ok(ReloadStep::DebounceElapsed) => {
                        // The debounce window expired; start building the
                        // next generation while the current one keeps
                        // serving. The prepare future owns its inputs (an
                        // `Arc` config reader and a snapshot of the
                        // loaders), so it is `'static` and never borrows
                        // live state.
                        reload.begin_preparing(Box::pin(prepare_reload(
                            Arc::clone(&config_reader),
                            server_loader.snapshot(),
                            CancellationToken::new(),
                            runtime.clone(),
                        )));
                    }
                    Ok(ReloadStep::Prepared(result)) => match result {
                        Ok(prepared) => {
                            // Commit the prepared reload exactly once, then
                            // return to idle; a failed commit is reported
                            // and not retried.
                            let (guard, commit_error) = commit_reload(
                                &mut server_tasks,
                                &mut server_loader,
                                prepared,
                                &runtime,
                                &connector_config_updater,
                            );
                            _cancellation_guard = guard;
                            if let Some(e) = commit_error {
                                error!(
                                    ?e,
                                    "Reload commit partially failed: a listener died; the \
                                     error names the loaders whose handler updates were lost; \
                                     the rest of the new generation is installed"
                                );
                            }
                        }
                        Err(e) => {
                            error!(?e, "Failed to prepare reload; live config unchanged");
                        }
                    },
                    Err(e) => break Err(e),
                }
            }
        }
    };
    // Fatal-outcome epilog: stop admitting sessions, adopt every future that
    // is still queued, then abort and reap the session and server task sets
    // with logging so a completed panic is not hidden by a JoinSet drop.
    session_rx.close();
    while let Some(fut) = session_rx.recv().await {
        sessions.spawn(fut);
    }
    common::lifecycle::task_scope::abort_and_reap_with(&mut sessions, |res| {
        if let Err(error) = res {
            error!(?error, "Session task returned an error during shutdown");
        }
    })
    .await;
    common::lifecycle::task_scope::abort_and_reap_with(&mut server_tasks, |res| {
        if let Err(error) = res {
            error!(?error, "Server task returned an error during shutdown");
        }
    })
    .await;
    outcome
}

/// The window that collapses bursts of watcher events into one reload.
pub struct ServerLoader {
    pub access_server: AccessServerLoader,
    pub proxy_server: ProxyServerLoader,
    pub reverse_tunnel: ReverseTunnelLoader,
}
impl ServerLoader {
    /// A read-only snapshot of the live loaders, for preparation. The
    /// snapshot resolves against the same live listeners but cannot commit.
    pub fn snapshot(&self) -> ServerLoaderSnapshot {
        ServerLoaderSnapshot {
            access_server: self.access_server.snapshot(),
            proxy_server: self.proxy_server.snapshot(),
            reverse_tunnel: self.reverse_tunnel.snapshot(),
        }
    }
}

/// An immutable snapshot of the live [`ServerLoader`]s, taken by
/// [`ServerLoader::snapshot`] for preparation. Preparation can resolve and
/// bind builders against the live listener set, but it cannot commit —
/// replacement authority stays with the single owning [`ServerLoader`].
pub struct ServerLoaderSnapshot {
    pub access_server: AccessServerLoaderSnapshot,
    pub proxy_server: ProxyServerLoaderSnapshot,
    pub reverse_tunnel: ReverseTunnelLoaderSnapshot,
}

#[derive(Debug, Error)]
pub enum ServerServeError {
    #[error("Failed to read config file: {0}")]
    Config(#[source] AnyError),
    #[error("Failed to load config: {0}")]
    Load(#[source] AnyError),
    #[error("Failed to commit reload: {0}")]
    Commit(#[source] AnyError),
    #[error("Server task failed: {0}")]
    ServerTask(#[source] AnyError),
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct StreamConfig {
    #[serde(default)]
    pool: StreamPoolBuilder,
    #[serde(default)]
    #[serde(alias = "conn", alias = "proxy_server")]
    upstream: HashMap<Arc<str>, HopConfig>,
}
impl Merge for StreamConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let pool = self.pool.merge(other.pool)?;
        let upstream = merge_map(self.upstream, other.upstream)?;
        Ok(Self { pool, upstream })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UdpConfig {
    #[serde(default)]
    #[serde(alias = "conn", alias = "proxy_server")]
    upstream: HashMap<Arc<str>, HopConfig>,
}
impl Merge for UdpConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let upstream = merge_map(self.upstream, other.upstream)?;
        Ok(Self { upstream })
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default)]
    pub connector: ConnectorConfig,
    #[serde(default)]
    pub access_server: AccessServerConfig,
    #[serde(default)]
    pub proxy_server: ProxyServerConfig,
    #[serde(default)]
    pub reverse_tunnel: ReverseTunnelConfig,
    #[serde(default)]
    pub stream: StreamConfig,
    #[serde(default)]
    pub udp: UdpConfig,
}
impl Merge for ServerConfig {
    type Error = AnyError;

    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        let connector = self.connector.merge(other.connector)?;
        let access_server = self.access_server.merge(other.access_server)?;
        let proxy_server = self.proxy_server.merge(other.proxy_server)?;
        let reverse_tunnel = self.reverse_tunnel.merge(other.reverse_tunnel)?;
        let stream = self.stream.merge(other.stream)?;
        let udp = self.udp.merge(other.udp)?;
        Ok(Self {
            access_server,
            proxy_server,
            reverse_tunnel,
            stream,
            udp,
            connector,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::anti_replay::VALIDATOR_UDP_HDR_TTL;
    use common::{
        lifecycle::{retention::RetentionActor, suspend::SystemResumeSignal},
        notify::{Notify, Subscription},
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Whether `subscription` has an unconsumed broadcast pending. The probe is
    /// synchronous: `notify_waiters` bumps a generation counter, so the answer
    /// depends on which channel broadcast, never on scheduling.
    async fn woken(subscription: &mut Subscription) -> bool {
        tokio::time::timeout(Duration::ZERO, subscription.notified())
            .await
            .is_ok()
    }

    /// `serve` hands the connector table the system-resume signal as its reset
    /// authority, not the config-change signal. The two are separate
    /// broadcasts with different jobs: a resume invalidates every pooled mux
    /// session, while a config change replaces the connector configuration in
    /// place (`commit_reload`) and leaves live sessions serving.
    ///
    /// The probe is the primitive the connector itself consumes —
    /// `run_mux_connector` subscribes to the reset signal and awaits it — so a
    /// reset signal woken by a config change, or one a resume leaves asleep, is
    /// red here.
    #[tokio::test]
    async fn the_connector_reset_signal_is_the_resume_signal_not_the_config_change_signal() {
        let config_changed = ConfigChangeSignal::new();
        let system_resume = SystemResumeSignal(Notify::new());
        let (_retention_actor, retention) = RetentionActor::new();
        let serve_context = ServeContext {
            stream_session_table: None,
            udp_session_table: None,
            config_changed: config_changed.clone(),
            system_resume: system_resume.clone(),
            retention,
        };
        let mut reset = serve_context.connector_reset().0.subscription();

        config_changed.notify_waiters();
        assert!(
            !woken(&mut reset).await,
            "a config change must not reset the connectors: it leaves live mux sessions serving"
        );

        system_resume.0.notify_waiters();
        assert!(
            woken(&mut reset).await,
            "a system resume must reset the connectors: every pooled mux session it holds is stale"
        );
    }

    fn server_config(src: &str) -> ServerConfig {
        toml::from_str(src).unwrap()
    }

    /// `serve` judges UDP route headers with `udp_time_validator()`; the peer
    /// client judges the same headers with its own constructor in `common`.
    /// The two must derive the same window, or one end refuses a header the
    /// other serves, so both are probed at the same instants: a stamp one
    /// header TTL of cached lifetime old is inside the window, and a stamp at
    /// the window's own horizon is outside it.
    ///
    /// The probes sit `VALIDATOR_TIME_FRAME` inside the horizon and exactly on
    /// it, so a window that stops covering the header TTL, or that reaches
    /// past the horizon, shows up here as a disagreement; a window landing
    /// strictly between the two probe ages does not. That inside probe also
    /// absorbs the clock advancing between the two `validates` calls, so the
    /// test does not depend on how the runner schedules it.
    #[test]
    fn the_server_udp_validator_agrees_with_the_shared_acceptance_window() {
        let server = udp_time_validator();
        let shared = TimeValidator::new(VALIDATOR_UDP_WINDOW);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch");
        for age in [Duration::ZERO, VALIDATOR_UDP_HDR_TTL, VALIDATOR_UDP_WINDOW] {
            let stamp = now - age;
            assert_eq!(
                server.validates(stamp),
                shared.validates(stamp),
                "the server's UDP validator and the shared window disagree about a stamp {age:?} old"
            );
        }
    }

    #[test]
    fn merging_config_files_rejects_a_duplicate_upstream_key() {
        // Two config files both defining `stream.upstream.a` must be rejected:
        // a later file may add keys, never silently override an earlier one.
        let first = server_config(
            "[stream.upstream.a]\naddress = \"tcp://127.0.0.1:1\"\nheader_key = \"aGVsbG8\"\n",
        );
        let second = server_config(
            "[stream.upstream.a]\naddress = \"tcp://127.0.0.1:2\"\nheader_key = \"aGVsbG8\"\n",
        );
        let err = first.merge(second).unwrap_err();
        assert!(format!("{err}").contains("Repeated key"), "{err}");
    }

    #[test]
    fn merging_config_files_keeps_every_distinct_upstream_key() {
        let first = server_config(
            "[stream.upstream.a]\naddress = \"tcp://127.0.0.1:1\"\nheader_key = \"aGVsbG8\"\n",
        );
        let second = server_config(
            "[stream.upstream.b]\naddress = \"tcp://127.0.0.1:2\"\nheader_key = \"aGVsbG8\"\n",
        );
        let merged = first.merge(second).unwrap();
        assert!(merged.stream.upstream.contains_key("a"));
        assert!(merged.stream.upstream.contains_key("b"));
    }

    /// Every top-level section of [`ServerConfig`] must be forwarded to that
    /// section's own `Merge` and the merged value returned: a section whose
    /// merge result is replaced (or taken from the wrong file) silently
    /// discards the later file's contribution. Each file below contributes
    /// only keys the other file does not, so only a per-section union of both
    /// files satisfies the assertions.
    #[test]
    fn merging_config_files_unions_every_top_level_section() {
        let first = server_config(
            r#"
[connector.bind]
v4 = "127.0.0.1"

[stream.upstream.stream_a]
address = "tcp://127.0.0.1:1"
header_key = "aGVsbG8"

[udp.upstream.udp_a]
address = "127.0.0.1:1"
header_key = "aGVsbG8"

[stream]
pool = ["pool_a"]

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:1"
destination = "tcp://127.0.0.1:9"
conn_selector = "default"

[[proxy_server.tcp_server]]
listen_addr = "127.0.0.1:1"
header_key = "aGVsbG8"
allow_loopback = true

[[reverse_tunnel.responder]]
listen_addr = "tcp://127.0.0.1:1"
header_key = "aGVsbG8"
"#,
        );
        let second = server_config(
            r#"
[connector.bind]
v6 = "::1"

[stream.upstream.stream_b]
address = "tcp://127.0.0.1:2"
header_key = "aGVsbG8"

[udp.upstream.udp_b]
address = "127.0.0.1:2"
header_key = "aGVsbG8"

[stream]
pool = ["pool_b"]

[[access_server.tcp_server]]
listen_addr = "127.0.0.1:2"
destination = "tcp://127.0.0.1:9"
conn_selector = "default"

[[proxy_server.tcp_server]]
listen_addr = "127.0.0.1:2"
header_key = "aGVsbG8"
allow_loopback = true

[[reverse_tunnel.responder]]
listen_addr = "tcp://127.0.0.1:2"
header_key = "aGVsbG8"
"#,
        );
        let merged = first.merge(second).unwrap();

        assert!(
            merged.connector.bind.v4.is_some() && merged.connector.bind.v6.is_some(),
            "connector: {:?}",
            merged.connector.bind
        );
        assert_eq!(
            merged.stream.upstream.len(),
            2,
            "stream.upstream: {:?}",
            merged.stream.upstream
        );
        assert_eq!(merged.udp.upstream.len(), 2, "udp.upstream");
        assert_eq!(merged.access_server.tcp_server.len(), 2, "access_server");
        assert_eq!(merged.proxy_server.tcp_server.len(), 2, "proxy_server");
        assert_eq!(merged.reverse_tunnel.responder.len(), 2, "reverse_tunnel");

        // The pool is an ordered concatenation of the files' entries; a
        // merge taken in the wrong direction would reverse them.
        let pool = &merged.stream.pool.0;
        assert_eq!(pool.len(), 2, "stream.pool: {pool:?}");
        let mut pool_names = pool.iter().map(|entry| match entry {
            common::config::SharableConfig::SharingKey(key) => key.to_string(),
            common::config::SharableConfig::Private(hop) => format!("{hop}"),
        });
        assert_eq!(pool_names.next().as_deref(), Some("pool_a"));
        assert_eq!(pool_names.next().as_deref(), Some("pool_b"));
    }

    /// The multi-file reader applies each file to the config accumulated from
    /// the earlier ones, in the order the paths were given — the direction
    /// every `Merge` impl is written for, which the test above pins for
    /// [`ServerConfig::merge`] directly. The reader is the production input
    /// path (`MultiFileConfigReader` is what `main` hands to `serve`), and it
    /// decides the direction; the reader's own integration tests drive it
    /// with a `Fragment` whose only field is a `BTreeMap`, which has no order
    /// to lose. The stream pool is an ordered concatenation, so a merge taken
    /// in the wrong direction comes out reversed here.
    #[tokio::test]
    async fn the_multi_file_reader_applies_the_files_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "proxy-reader-order-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the system clock is after the Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let first: Arc<str> = Arc::from(dir.join("a.toml").to_str().unwrap());
        let second: Arc<str> = Arc::from(dir.join("b.toml").to_str().unwrap());
        std::fs::write(first.as_ref(), "[stream]\npool = [\"pool_a\"]\n").unwrap();
        std::fs::write(second.as_ref(), "[stream]\npool = [\"pool_b\"]\n").unwrap();

        let reader = config::multi_file_config::MultiFileConfigReader::<ServerConfig>::new(
            vec![first, second].into(),
        );
        let merged = reader.read_config().await.unwrap();
        let pool_names = merged
            .stream
            .pool
            .0
            .iter()
            .map(|entry| match entry {
                common::config::SharableConfig::SharingKey(key) => key.to_string(),
                common::config::SharableConfig::Private(hop) => format!("{hop}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pool_names,
            vec!["pool_a".to_string(), "pool_b".to_string()],
            "the reader must apply each file to the accumulated config in the order the paths \
             were given: a merge taken in the wrong direction reverses an ordered field"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
