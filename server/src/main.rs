use std::sync::Arc;

use clap::Parser;
use server::{multi_file_config::MultiFileConfigReader, serve, ConfigWatcher};

#[derive(Debug, Parser)]
struct Args {
    /// Paths to the configuration files.
    config_files: Vec<Arc<str>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let watcher = ConfigWatcher::new();
    let notify_rx = Arc::clone(watcher.notify_rx());
    let watcher = Arc::new(watcher);
    for file in &args.config_files {
        let file = file.clone();
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { file_watcher_tokio::watch_file(file.as_ref(), watcher).await });
    }
    let config_reader = MultiFileConfigReader::new(args.config_files.into());
    serve(notify_rx, config_reader).await;
}
