use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
use clap::Parser;
use common::{error::AnyResult, suspend::spawn_check_system_suspend};
use server::{
    ServeContext,
    config::{multi_file_config::MultiConfigReader, spawn_watch_tasks},
    monitor::monitor_router,
    serve,
};
use tracing::info;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

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
    if args.config_file_paths.is_empty() {
        tracing::error!("No config files provided. Check --help for usage.");
        std::process::abort();
    }
    if let Some(path) = args.csv_log_path {
        common::proto::log::stream::init_logger(path.clone());
        common::proto::log::udp::init_logger(path.clone());
    };

    let config_changed = spawn_watch_tasks(&args.config_file_paths);
    let system_suspended = spawn_check_system_suspend();

    #[cfg(feature = "dhat-heap")]
    let profiler = dhat::Profiler::new_heap();

    // Monitoring
    let serve_context: ServeContext;
    if let Some(monitor_addr) = args.monitor {
        let router = Router::new();

        let (session_tables, monitor_router) = monitor_router();
        let router = router.merge(monitor_router);

        #[cfg(feature = "dhat-heap")]
        let router = router.merge(server::profiling::profiler_router(profiler));

        let listener = tokio::net::TcpListener::bind(&monitor_addr).await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        let server = axum::serve(listener, router.into_make_service());
        info!("Monitoring HTTP server listening addr: {listen_addr}");
        tokio::spawn(async move {
            server.await.unwrap();
        });

        serve_context = ServeContext {
            stream_session_table: Some(session_tables.stream),
            udp_session_table: Some(session_tables.udp),
            config_changed,
            system_suspended,
        };
    } else {
        serve_context = ServeContext {
            stream_session_table: None,
            udp_session_table: None,
            config_changed,
            system_suspended,
        };
    }

    let config_reader = MultiConfigReader::new(args.config_file_paths.into());
    serve(config_reader, serve_context)
        .await
        .map_err(Into::into)
}
