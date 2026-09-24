//! Pins that the proxy-server and reverse-tunnel reload commits attempt every
//! *kind*, not only the kinds before the first listener that died.
//!
//! `ProxyServerLoader::commit` commits seven independent kinds (tcp, tcp-mux,
//! udp, kcp, mptcp, rtp, rtp-mux) and `ReverseTunnelLoader::commit` commits
//! three (initiator, tcp responder, rtp responder). Each kind is its own
//! `common::loading::Loader`, so a listener that dies in an earlier kind must
//! not suppress a kind whose listeners are healthy and whose commit cannot
//! fail. The kinds' later entries speak their own wire protocols, so what this
//! test reads is the *commit effect*: after a commit whose first kind lost its
//! listener, the later kinds' listener tasks are still spawned into the
//! server's task set. (The access-server kinds, which relay plain TCP and UDP,
//! are pinned end-to-end by destination in `access_server_reload_kinds.rs`.)
//!
//! Each loader is driven through its real `prepare` / `Loader::commit` entry
//! points: generation 1 installs the first kind's listener alone, generation 2
//! prepares the first kind (alive, so its op is a *replacement*) together with
//! the later kinds (new, so their ops are *spawns*), then aborts the first
//! kind's task before committing. One task set holds the first kind's task and
//! another holds the later kinds', so exactly one listener dies.

use std::sync::Arc;

use ae::anti_replay::{ReplayValidator, TimeValidator};
use common::{
    anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME, VALIDATOR_UDP_HDR_TTL},
    connect::{ConnectorConfig, ConnectorResetSignal, connector_config_cell},
    error::AnyError,
    lifecycle::retention::{RetentionActor, RetentionActorSender},
    notify::Notify,
    proxy_runtime::{
        connect::udp::UdpConnector,
        context::{Runtime, StreamRuntime, UdpRuntime},
    },
    session::SessionSpawner,
    stream_runtime::pool::StreamConnPool,
};
use protocol::{
    proxy_server::{self, ProxyServerConfig, ProxyServerLoader},
    reverse_tunnel::{self, ReverseTunnelConfig, ReverseTunnelLoader},
    stream_proto::connect::build_concrete_stream_connector_table,
};
use swap::Swap;
use tokio::task::JoinSet;

/// The example header key, used by every proxy-server and responder listener.
const HEADER_KEY: &str = "cHJveHktZXhhbXBsZS1rZXk";

/// The test's process actors live here, so returning from the test aborts them.
type Tasks = JoinSet<()>;

/// The server-task set `commit` spawns listener tasks into.
type ServerTasks = JoinSet<Result<(), AnyError>>;

fn spawn_process_actors(tasks: &mut Tasks) -> (SessionSpawner, RetentionActorSender) {
    let (session_spawner, mut session_rx) = SessionSpawner::channel();
    tasks.spawn(async move {
        let mut sessions = JoinSet::new();
        loop {
            tokio::select! {
                Some(fut) = session_rx.recv() => { sessions.spawn(fut); }
                Some(res) = sessions.join_next() => {
                    let _ = res.expect("session task panicked");
                }
                else => break,
            }
        }
    });
    let (retention_actor, retention) = RetentionActor::new();
    tasks.spawn(async move {
        let _exit = retention_actor.run().await;
    });
    (session_spawner, retention)
}

/// The runtime every listener commits into, reading the connector
/// configuration the test's own cell holds. The connector drivers get a task
/// set of their own, so no later abort of a listener set can reach them.
fn runtime(tasks: &mut Tasks) -> Runtime {
    let (session_spawner, retention) = spawn_process_actors(tasks);
    let mut connector_drivers: ServerTasks = JoinSet::new();
    let connector_config = connector_config_cell(ConnectorConfig::default()).0;
    let udp_connector = Arc::new(UdpConnector::new(connector_config.clone()));
    let connector_table = Arc::new(build_concrete_stream_connector_table(
        connector_config,
        ConnectorResetSignal(Notify::new()),
        &mut connector_drivers,
        &udp_connector,
    ));
    tasks.spawn(async move {
        while let Some(result) = connector_drivers.join_next().await {
            result
                .expect("connector driver panicked")
                .expect("connector driver failed");
        }
    });
    let stream = StreamRuntime {
        session_table: None,
        pool: Swap::new(StreamConnPool::empty()),
        connector_table,
        replay_validator: Arc::new(ReplayValidator::new(
            VALIDATOR_TIME_FRAME,
            VALIDATOR_CAPACITY,
        )),
        session_spawner: session_spawner.clone(),
        retention: retention.clone(),
    };
    let udp = UdpRuntime {
        session_table: None,
        time_validator: Arc::new(TimeValidator::new(
            VALIDATOR_TIME_FRAME + VALIDATOR_UDP_HDR_TTL,
        )),
        connector: udp_connector,
        session_spawner: session_spawner.clone(),
        retention,
    };
    Runtime {
        stream,
        udp,
        session_spawner,
    }
}

/// Kill exactly the tasks in `tasks` and wait for them to be reaped, so their
/// handler receivers are certainly dropped — the state a listener that died on
/// an I/O error leaves behind.
async fn kill(tasks: &mut ServerTasks) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn proxy_config(tcp: bool, tcp_mux: bool, udp: bool) -> ProxyServerConfig {
    let mut value = serde_json::json!({});
    if tcp {
        value["tcp_server"] = serde_json::json!([{
            "listen_addr": "127.0.0.1:0",
            "header_key": HEADER_KEY,
            "allow_loopback": true,
        }]);
    }
    if tcp_mux {
        value["tcp_mux_server"] = serde_json::json!([{
            "listen_addr": "127.0.0.1:0",
            "header_key": HEADER_KEY,
            "allow_loopback": true,
        }]);
    }
    if udp {
        value["udp_server"] = serde_json::json!([{
            "listen_addr": "127.0.0.1:0",
            "header_key": HEADER_KEY,
            "allow_loopback": true,
        }]);
    }
    serde_json::from_value(value).expect("the scripted proxy-server config must deserialize")
}

fn reverse_tunnel_config(tcp_responder: bool, rtp_responder: bool) -> ReverseTunnelConfig {
    let mut responder = Vec::new();
    if tcp_responder {
        responder.push(serde_json::json!({
            "listen_addr": "tcp://127.0.0.1:0",
            "header_key": HEADER_KEY,
        }));
    }
    if rtp_responder {
        responder.push(serde_json::json!({
            "listen_addr": "rtpmux://127.0.0.1:0",
            "header_key": HEADER_KEY,
        }));
    }
    serde_json::from_value(serde_json::json!({ "responder": responder }))
        .expect("the scripted reverse-tunnel config must deserialize")
}

/// A proxy-server commit whose first kind (`tcp_server`) loses its listener
/// still commits the kinds after it: the `tcp_mux_server` and `udp_server`
/// listeners are spawned. Reverting the aggregate back to a `?` chain leaves
/// both unspawned.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_proxy_server_kind_does_not_suppress_the_kinds_after_it() {
    let mut tasks: Tasks = JoinSet::new();
    let runtime = runtime(&mut tasks);
    let mut loader = ProxyServerLoader::new();
    let mut dying_tasks: ServerTasks = JoinSet::new();

    // Generation 1: the first kind's listener alone, so its task can be killed
    // without touching the later kinds.
    let snapshot = loader.snapshot();
    let prepared =
        proxy_server::prepare(proxy_config(true, false, false), &snapshot, runtime.clone())
            .await
            .expect("generation 1 preparation must succeed");
    loader
        .commit(&mut dying_tasks, prepared)
        .expect("generation 1 commits into an empty loader, so nothing can fail");
    assert_eq!(
        dying_tasks.len(),
        1,
        "generation 1 spawns exactly one listener task, so the abort kills exactly the first kind"
    );

    // Generation 2: the first kind (a replacement) plus the two later kinds
    // (spawns). Prepare while the first kind is alive, then kill it.
    let snapshot = loader.snapshot();
    let prepared =
        proxy_server::prepare(proxy_config(true, true, true), &snapshot, runtime.clone())
            .await
            .expect("generation 2 preparation must succeed");
    kill(&mut dying_tasks).await;

    let mut live_tasks: ServerTasks = JoinSet::new();
    let error = loader.commit(&mut live_tasks, prepared).expect_err(
        "the first kind's listener died between preparation and commit, so the commit must \
         report the lost handler update",
    );
    assert!(
        error.to_string().contains("tcp_server") && error.to_string().contains("listener died"),
        "the reported failure must name the kind and what happened; got: {error}"
    );
    assert_eq!(
        live_tasks.len(),
        2,
        "the two kinds after the failed one must still have been committed and spawned; \
         the commit reported: {error}"
    );

    tasks.shutdown().await;
}

/// A reverse-tunnel commit whose second kind (`tcp_responder`) loses its
/// listener still commits the kind after it: the `rtp_responder` listener is
/// spawned. Reverting the aggregate back to a `?` chain leaves it unspawned.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_reverse_tunnel_kind_does_not_suppress_the_kind_after_it() {
    let mut tasks: Tasks = JoinSet::new();
    let runtime = runtime(&mut tasks);
    let mut loader = ReverseTunnelLoader::new();
    let mut dying_tasks: ServerTasks = JoinSet::new();

    // Generation 1: the tcp responder alone.
    let snapshot = loader.snapshot();
    let prepared = reverse_tunnel::prepare(
        reverse_tunnel_config(true, false),
        &snapshot,
        runtime.clone(),
    )
    .await
    .expect("generation 1 preparation must succeed");
    loader
        .commit(&mut dying_tasks, prepared)
        .expect("generation 1 commits into an empty loader, so nothing can fail");
    assert_eq!(
        dying_tasks.len(),
        1,
        "generation 1 spawns exactly one responder task, so the abort kills exactly the tcp \
         responder"
    );

    // Generation 2: the tcp responder (a replacement) plus the rtp responder
    // (a spawn). Prepare while the tcp responder is alive, then kill it.
    let snapshot = loader.snapshot();
    let prepared = reverse_tunnel::prepare(
        reverse_tunnel_config(true, true),
        &snapshot,
        runtime.clone(),
    )
    .await
    .expect("generation 2 preparation must succeed");
    kill(&mut dying_tasks).await;

    let mut live_tasks: ServerTasks = JoinSet::new();
    let error = loader.commit(&mut live_tasks, prepared).expect_err(
        "the tcp responder died between preparation and commit, so the commit must report the \
         lost handler update",
    );
    assert!(
        error.to_string().contains("tcp_responder") && error.to_string().contains("listener died"),
        "the reported failure must name the kind and what happened; got: {error}"
    );
    assert_eq!(
        live_tasks.len(),
        1,
        "the kind after the failed one must still have been committed and spawned; the commit \
         reported: {error}"
    );

    tasks.shutdown().await;
}
