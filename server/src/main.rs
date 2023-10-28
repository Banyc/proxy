use std::{net::SocketAddr, sync::Arc};

use axum::{extract::State, routing::get, Router};
use clap::Parser;
use common::{error::AnyResult, session_table::BothSessionTables};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use server::{
    config::multi_file_config::{spawn_watch_tasks, MultiFileConfigReader},
    serve,
};
use tabled::Table;
use tracing::info;

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_file_paths: Vec<Arc<str>>,

    /// Listen address for monitoring
    #[arg(short, long)]
    monitor: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> AnyResult {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Monitoring
    let session_table: BothSessionTables;
    if let Some(monitor_addr) = args.monitor {
        let metrics_handle = PrometheusBuilder::new().install_recorder().unwrap();
        session_table = BothSessionTables::new();
        let session_table = session_table.clone();
        tokio::spawn(async move {
            async fn metrics(metrics_handle: State<PrometheusHandle>) -> String {
                metrics_handle.render()
            }
            async fn sessions(State(session_table): State<BothSessionTables>) -> String {
                let mut text = String::new();

                text.push_str("Stream:\n");
                let mut sessions = session_table
                    .stream()
                    .sessions()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                sessions.sort_by_key(|session| session.start);
                let sessions = Table::new(sessions)
                    .with(tabled::settings::Style::blank())
                    .to_string();
                text.push_str(&sessions);
                text.push('\n');

                text.push_str("UDP:\n");
                let mut sessions = session_table
                    .udp()
                    .sessions()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                sessions.sort_by_key(|session| session.start);
                let sessions = Table::new(sessions)
                    .with(tabled::settings::Style::blank())
                    .to_string();
                text.push_str(&sessions);
                text.push('\n');

                text
            }
            let router = Router::new()
                .route("/", get(metrics))
                .with_state(metrics_handle)
                .route("/sessions", get(sessions))
                .with_state(session_table)
                .route("/health", get(|| async { Ok::<_, ()>(()) }));
            let server = axum::Server::bind(&monitor_addr).serve(router.into_make_service());
            info!(
                "Monitoring HTTP server listening addr: {}",
                server.local_addr()
            );
            server.await.unwrap();
        });
    } else {
        session_table = BothSessionTables::new_disabled();
    }

    let notify_rx = spawn_watch_tasks(&args.config_file_paths);
    let config_reader = MultiFileConfigReader::new(args.config_file_paths.into());
    serve(notify_rx, config_reader, session_table)
        .await
        .map_err(|e| e.into())
}
