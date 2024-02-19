use std::{marker::PhantomData, sync::Arc};

use common::{config::Merge, error::AnyError};
use serde::Deserialize;

use crate::ConfigReader;

use super::{toml::human_toml_error, ConfigWatcher};

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

pub struct MultiConfigReader<C> {
    config_file_paths: Arc<[Arc<str>]>,
    phantom_config: PhantomData<C>,
}

impl<C> MultiConfigReader<C> {
    pub fn new(config_file_paths: Arc<[Arc<str>]>) -> Self {
        Self {
            config_file_paths,
            phantom_config: PhantomData,
        }
    }
}

impl<C> ConfigReader for MultiConfigReader<C>
where
    for<'de> C: Deserialize<'de> + Send + Sync + 'static,
    C: Merge<Error = AnyError>,
{
    type Config = C;
    async fn read_config(&self) -> Result<Self::Config, AnyError> {
        let mut config = C::default();
        for path in self.config_file_paths.iter() {
            let src = tokio::fs::read_to_string(path.as_ref()).await?;
            let c: C = toml::from_str(&src).map_err(|e| human_toml_error(path, &src, e))?;
            config = config.merge(c)?;
        }
        Ok(config)
    }
}
