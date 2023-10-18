use std::{net::SocketAddr, sync::Arc};

use axum::{extract::State, routing::get, Router};
use clap::Parser;
use common::{error::AnyResult, stream::session_table::StreamSessionTable};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use server::{
    config::multi_file_config::{spawn_watch_tasks, MultiFileConfigReader},
    serve,
};
use tracing::info;

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_file_paths: Vec<Arc<str>>,

    /// Listen address for metrics
    #[arg(short, long)]
    metrics: Option<SocketAddr>,
}

#[tokio::main]
async fn main() -> AnyResult {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Metrics
    let stream_session_table: StreamSessionTable;
    if let Some(metrics_addr) = args.metrics {
        let metrics_handle = PrometheusBuilder::new().install_recorder().unwrap();
        stream_session_table = StreamSessionTable::new();
        let stream_session_table = stream_session_table.clone();
        tokio::spawn(async move {
            async fn metrics(metrics_handle: State<PrometheusHandle>) -> String {
                metrics_handle.render()
            }
            async fn sessions(stream_session_table: State<StreamSessionTable>) -> String {
                let mut sessions = stream_session_table
                    .sessions()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>();
                sessions.sort_by_key(|session| session.start);
                let mut text = String::new();
                for session in sessions {
                    text.push_str(&session.to_string());
                    text.push('\n');
                }
                text
            }
            let router = Router::new()
                .route("/", get(metrics))
                .with_state(metrics_handle)
                .route("/sessions", get(sessions))
                .with_state(stream_session_table);
            let server = axum::Server::bind(&metrics_addr).serve(router.into_make_service());
            info!(
                "Metrics HTTP server listening addr: {}",
                server.local_addr()
            );
            server.await.unwrap();
        });
    } else {
        stream_session_table = StreamSessionTable::new_disabled();
    }

    let notify_rx = spawn_watch_tasks(&args.config_file_paths);
    let config_reader = MultiFileConfigReader::new(args.config_file_paths.into());
    serve(notify_rx, config_reader, stream_session_table.clone())
        .await
        .map_err(|e| e.into())
}
