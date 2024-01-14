use std::{net::SocketAddr, num::NonZeroUsize, path::PathBuf, sync::Arc};

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

    /// CSV log path
    #[arg(short, long)]
    csv_log_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> AnyResult {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if let Some(path) = args.csv_log_path {
        csv_logger::init(
            path,
            csv_logger::RotationPolicy {
                max_records: NonZeroUsize::new(1024 * 64).unwrap(),
                max_epochs: 4,
            },
        );
    };

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
            fn sessions(
                Query(params): Query<SessionsParams>,
                State(session_table): State<BothSessionTables<ConcreteStreamType>>,
            ) -> anyhow::Result<String> {
                let mut text = String::new();
                {
                    let sql = &params.stream_sql;
                    text.push_str("Stream:\n");
                    let sessions = session_table
                        .stream()
                        .map(|s| s.to_view(sql).map(|s| s.to_string()))
                        .transpose()?;
                    if let Some(s) = sessions {
                        text.push_str(&s);
                    }
                    text.push('\n');
                }
                {
                    let sql = &params.udp_sql;
                    text.push_str("UDP:\n");
                    let sessions = session_table
                        .udp()
                        .map(|s| s.to_view(sql).map(|s| s.to_string()))
                        .transpose()?;
                    if let Some(s) = sessions {
                        text.push_str(&s);
                    }
                    text.push('\n');
                }
                Ok(text)
            }
            let router = Router::new()
                .route("/", get(metrics))
                .with_state(metrics_handle)
                .route(
                    "/sessions",
                    get(|params, state| async {
                        sessions(params, state).map_err(|e| format!("{e:#?}"))
                    }),
                )
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
    const SQL: &str = "sort start_ms select destination duration upstream_remote";
    SQL.to_string()
}
#[derive(Debug, Deserialize)]
struct SessionsParams {
    #[serde(default = "default_sql")]
    stream_sql: String,
    #[serde(default = "default_sql")]
    udp_sql: String,
}
