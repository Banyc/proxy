use std::sync::Arc;

use clap::Parser;
use server::{
    config::multi_file_config::{spawn_watch_tasks, MultiFileConfigReader},
    serve,
};

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_files: Vec<Arc<str>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let notify_rx = spawn_watch_tasks(&args.config_files);
    let config_reader = MultiFileConfigReader::new(args.config_files.into());
    serve(notify_rx, config_reader).await;
}
