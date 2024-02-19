use std::{net::SocketAddr, num::NonZeroUsize, path::PathBuf, sync::Arc};

use axum::Router;
use clap::Parser;
use common::error::AnyResult;
use server::{
    config::multi_file_config::{spawn_watch_tasks, MultiConfigReader},
    monitor::monitor_router,
    serve, ServeContext,
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
        csv_logger::init(
            path,
            csv_logger::RotationPolicy {
                max_records: NonZeroUsize::new(1024 * 64).unwrap(),
                max_epochs: 4,
            },
        );
    };

    #[cfg(feature = "dhat-heap")]
    let profiler = dhat::Profiler::new_heap();

    // Monitoring
    let serve_context: ServeContext;
    if let Some(monitor_addr) = args.monitor {
        let router = Router::new();

        let (session_tables, monitor_router) = monitor_router();
        let router = router.nest("/", monitor_router);

        #[cfg(feature = "dhat-heap")]
        let router = router.nest("/", server::profiling::profiler_router(profiler));

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
        };
    } else {
        serve_context = ServeContext {
            stream_session_table: None,
            udp_session_table: None,
        };
    }

    let notify_rx = spawn_watch_tasks(&args.config_file_paths);
    let config_reader = MultiConfigReader::new(args.config_file_paths.into());
    serve(notify_rx, config_reader, serve_context)
        .await
        .map_err(Into::into)
}
