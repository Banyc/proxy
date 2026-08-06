use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::Router;
use clap::Parser;
use common::{
    error::AnyResult, process::ProcessTaskExit, retention::RetentionActor,
    suspend::spawn_check_system_suspend,
};
use server::{
    ServeContext,
    config::{multi_file_config::MultiFileConfigReader, spawn_watch_tasks},
    monitor::monitor_router,
    serve,
};
use tracing::{error, info};

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_file_paths: Vec<Arc<str>>,

    /// Listen address for monitoring
    #[arg(short, long, alias = "monitor")]
    monitor_listen_addr: Option<SocketAddr>,

    /// Record directory for CSV logs
    #[arg(short, long, alias = "csv-log-path")]
    record_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> AnyResult {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if args.config_file_paths.is_empty() {
        tracing::error!("No config files provided. Check --help for usage.");
        std::process::exit(1);
    }
    if let Some(path) = args.record_dir {
        common::proto::log::stream::init_logger(path.clone());
        common::proto::log::udp::init_logger(path.clone());
    };

    let mut process_tasks: tokio::task::JoinSet<ProcessTaskExit> = tokio::task::JoinSet::new();

    let (retention_actor, retention) = RetentionActor::new();
    process_tasks.spawn(retention_actor.run());

    let config_changed = spawn_watch_tasks(&mut process_tasks, &args.config_file_paths);
    let system_suspended = spawn_check_system_suspend(&mut process_tasks);

    #[cfg(feature = "dhat-heap")]
    let profiler = dhat::Profiler::new_heap();

    // Monitoring
    let serve_context: ServeContext;
    if let Some(monitor_addr) = args.monitor_listen_addr {
        let router = Router::new();

        let (session_tables, monitor_router) = monitor_router();
        let router = router.merge(monitor_router);

        #[cfg(feature = "dhat-heap")]
        let router = router.merge(server::profiling::profiler_router(profiler));

        let listener = tokio::net::TcpListener::bind(&monitor_addr).await.unwrap();
        let listen_addr = listener.local_addr().unwrap();
        let server = axum::serve(listener, router.into_make_service());
        info!("Monitoring HTTP server listening addr: {listen_addr}");
        process_tasks.spawn(async move {
            match server.await {
                Ok(()) => ProcessTaskExit::Completed {
                    task: "monitor_server",
                },
                Err(error) => ProcessTaskExit::Failed {
                    task: "monitor_server",
                    detail: error.to_string(),
                },
            }
        });

        serve_context = ServeContext {
            stream_session_table: Some(session_tables.stream),
            udp_session_table: Some(session_tables.udp),
            config_changed,
            system_suspended,
            retention,
        };
    } else {
        serve_context = ServeContext {
            stream_session_table: None,
            udp_session_table: None,
            config_changed,
            system_suspended,
            retention,
        };
    }

    let config_reader = MultiFileConfigReader::new(args.config_file_paths.into());
    let serving = serve(config_reader, serve_context);
    tokio::pin!(serving);
    loop {
        tokio::select! {
            res = &mut serving => return res.map_err(Into::into),
            Some(res) = process_tasks.join_next() => {
                match res {
                    Ok(ProcessTaskExit::Completed { task }) => {
                        error!(task, "Root process task completed unexpectedly");
                    }
                    Ok(ProcessTaskExit::Failed { task, detail }) => {
                        error!(task, detail, "Root process task failed");
                    }
                    Err(error) if error.is_panic() => {
                        error!(?error, "Root process task panicked");
                        std::panic::resume_unwind(error.into_panic());
                    }
                    Err(error) if error.is_cancelled() => {
                        info!(?error, "Root process task cancelled (normal exit)");
                    }
                    Err(error) => {
                        error!(?error, "Root process task failed to join");
                    }
                }
            }
        }
    }
}
