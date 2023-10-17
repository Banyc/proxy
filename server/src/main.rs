use std::{net::SocketAddr, sync::Arc};

use axum::{extract::State, routing::get, Router};
use clap::Parser;
use common::error::AnyResult;
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
    if let Some(metrics_addr) = args.metrics {
        let metrics_handle = PrometheusBuilder::new().install_recorder().unwrap();
        tokio::spawn(async move {
            async fn metrics(metrics_handle: State<PrometheusHandle>) -> String {
                metrics_handle.render()
            }
            let router = Router::new()
                .route("/", get(metrics))
                .with_state(metrics_handle);
            let server = axum::Server::bind(&metrics_addr).serve(router.into_make_service());
            info!(
                "Metrics HTTP server listening addr: {}",
                server.local_addr()
            );
            server.await.unwrap();
        });
    }

    let notify_rx = spawn_watch_tasks(&args.config_file_paths);
    let config_reader = MultiFileConfigReader::new(args.config_file_paths.into());
    serve(notify_rx, config_reader).await.map_err(|e| e.into())
}
