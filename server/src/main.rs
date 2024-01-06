use std::{net::SocketAddr, sync::Arc};

use axum::{
    extract::{Query, State},
    routing::get,
    Router,
};
use clap::Parser;
use common::{
    error::AnyResult, session_table::BothSessionTables, stream::concrete::addr::ConcreteStreamType,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::Deserialize;
use server::{
    config::multi_file_config::{spawn_watch_tasks, MultiFileConfigReader},
    serve,
};
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
    let session_table: BothSessionTables<ConcreteStreamType>;
    if let Some(monitor_addr) = args.monitor {
        let metrics_handle = PrometheusBuilder::new().install_recorder().unwrap();
        session_table = BothSessionTables::new();
        let session_table = session_table.clone();
        tokio::spawn(async move {
            async fn metrics(metrics_handle: State<PrometheusHandle>) -> String {
                metrics_handle.render()
            }
            async fn sessions(
                Query(params): Query<SessionsParams>,
                State(session_table): State<BothSessionTables<ConcreteStreamType>>,
            ) -> String {
                let mut text = String::new();

                let sql = &params.sql;
                text.push_str("Stream:\n");
                let sessions = session_table
                    .stream()
                    .and_then(|s| s.to_view(sql).map(|s| s.to_string()));
                if let Some(s) = sessions {
                    text.push_str(&s);
                }
                text.push('\n');

                text.push_str("UDP:\n");
                let sessions = session_table
                    .udp()
                    .and_then(|s| s.to_view(sql).map(|s| s.to_string()));
                if let Some(s) = sessions {
                    text.push_str(&s);
                }
                text.push('\n');

                text
            }
            let router = Router::new()
                .route("/", get(metrics))
                .with_state(metrics_handle)
                .route("/sessions", get(sessions))
                .with_state(session_table)
                .route("/health", get(|| async { Ok::<_, ()>(()) }));
            let listener = tokio::net::TcpListener::bind(&monitor_addr).await.unwrap();
            let listen_addr = listener.local_addr().unwrap();
            let server = axum::serve(listener, router.into_make_service());
            info!("Monitoring HTTP server listening addr: {listen_addr}");
            server.await.unwrap();
        });
    } else {
        session_table = BothSessionTables::empty();
    }

    let notify_rx = spawn_watch_tasks(&args.config_file_paths);
    let config_reader = MultiFileConfigReader::new(args.config_file_paths.into());
    serve(notify_rx, config_reader, session_table)
        .await
        .map_err(|e| e.into())
}

fn default_sql() -> String {
    const SQL: &str = "sort start_ms select destination duration";
    SQL.to_string()
}
#[derive(Debug, Deserialize)]
struct SessionsParams {
    #[serde(default = "default_sql")]
    sql: String,
}
