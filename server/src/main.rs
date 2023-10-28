use std::{net::SocketAddr, sync::Arc};

use axum::{extract::State, routing::get, Router};
use clap::Parser;
use common::{
    error::AnyResult, stream::session_table::StreamSessionTable,
    udp::session_table::UdpSessionTable,
};
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
    let stream_session_table: StreamSessionTable;
    let udp_session_table: UdpSessionTable;
    if let Some(monitor_addr) = args.monitor {
        let metrics_handle = PrometheusBuilder::new().install_recorder().unwrap();
        stream_session_table = StreamSessionTable::new();
        udp_session_table = UdpSessionTable::new();
        let stream_session_table = stream_session_table.clone();
        let udp_session_table = udp_session_table.clone();
        tokio::spawn(async move {
            async fn metrics(metrics_handle: State<PrometheusHandle>) -> String {
                metrics_handle.render()
            }
            async fn sessions(
                State((stream_session_table, udp_session_table)): State<(
                    StreamSessionTable,
                    UdpSessionTable,
                )>,
            ) -> String {
                let mut text = String::new();

                text.push_str("Stream:\n");
                let mut sessions = stream_session_table
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
                let mut sessions = udp_session_table
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
                .with_state((stream_session_table, udp_session_table))
                .route("/health", get(|| async { Ok::<_, ()>(()) }));
            let server = axum::Server::bind(&monitor_addr).serve(router.into_make_service());
            info!(
                "Monitoring HTTP server listening addr: {}",
                server.local_addr()
            );
            server.await.unwrap();
        });
    } else {
        stream_session_table = StreamSessionTable::new_disabled();
        udp_session_table = UdpSessionTable::new_disabled();
    }

    let notify_rx = spawn_watch_tasks(&args.config_file_paths);
    let config_reader = MultiFileConfigReader::new(args.config_file_paths.into());
    serve(
        notify_rx,
        config_reader,
        stream_session_table,
        udp_session_table,
    )
    .await
    .map_err(|e| e.into())
}
