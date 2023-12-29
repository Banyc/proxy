use std::{marker::PhantomData, sync::Arc};

use common::error::AnyError;
use serde::Deserialize;

use crate::ConfigReader;

use super::ConfigWatcher;

pub fn spawn_watch_tasks(config_file_paths: &[Arc<str>]) -> Arc<tokio::sync::Notify> {
    let watcher = ConfigWatcher::new();
    let notify_rx = Arc::clone(watcher.notify_rx());
    let watcher = Arc::new(watcher);
    config_file_paths.iter().cloned().for_each(|path| {
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { file_watcher_tokio::watch_file(path.as_ref(), watcher).await });
    });
    notify_rx
}

pub struct MultiFileConfigReader<C> {
    config_file_paths: Arc<[Arc<str>]>,
    phantom_config: PhantomData<C>,
}

impl<C> MultiFileConfigReader<C> {
    pub fn new(config_file_paths: Arc<[Arc<str>]>) -> Self {
        Self {
            config_file_paths,
            phantom_config: PhantomData,
        }
    }
}

impl<C> ConfigReader for MultiFileConfigReader<C>
where
    for<'de> C: Deserialize<'de> + Send + Sync + 'static,
{
    type Config = C;
    async fn read_config(&self) -> Result<Self::Config, AnyError> {
        read_multi_file_config(&self.config_file_paths).await
    }
}

pub async fn read_multi_file_config<C>(config_file_paths: &[Arc<str>]) -> Result<C, AnyError>
where
    for<'de> C: Deserialize<'de>,
{
    let mut config_str = String::new();
    for path in config_file_paths {
        let src = tokio::fs::read_to_string(path.as_ref()).await?;
        config_str.push_str(&src);
    }
    let config: C = toml::from_str(&config_str)?;
    Ok(config)
}
