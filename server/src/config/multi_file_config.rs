use std::{marker::PhantomData, sync::Arc};

use async_trait::async_trait;
use common::error::AnyError;
use serde::Deserialize;

use crate::ConfigReader;

use super::ConfigWatcher;

pub fn spawn_watch_tasks(config_files: &[Arc<str>]) -> Arc<tokio::sync::Notify> {
    let watcher = ConfigWatcher::new();
    let notify_rx = Arc::clone(watcher.notify_rx());
    let watcher = Arc::new(watcher);
    for file in config_files {
        let file = file.clone();
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { file_watcher_tokio::watch_file(file.as_ref(), watcher).await });
    }
    notify_rx
}

pub struct MultiFileConfigReader<C> {
    config_files: Arc<[Arc<str>]>,
    phantom_config: PhantomData<C>,
}

impl<C> MultiFileConfigReader<C> {
    pub fn new(config_files: Arc<[Arc<str>]>) -> Self {
        Self {
            config_files,
            phantom_config: PhantomData,
        }
    }
}

#[async_trait]
impl<C> ConfigReader for MultiFileConfigReader<C>
where
    for<'de> C: Deserialize<'de> + Send + Sync + 'static,
{
    type Config = C;
    async fn read_config(&self) -> Result<Self::Config, AnyError> {
        read_multi_file_config(&self.config_files).await
    }
}

pub async fn read_multi_file_config<C>(config_files: &[Arc<str>]) -> Result<C, AnyError>
where
    for<'de> C: Deserialize<'de>,
{
    let mut config_str = String::new();
    for config_file in config_files {
        let src = tokio::fs::read_to_string(config_file.as_ref()).await?;
        config_str.push_str(&src);
    }
    let config: C = toml::from_str(&config_str)?;
    Ok(config)
}
