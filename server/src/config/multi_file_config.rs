use std::{marker::PhantomData, sync::Arc};

use common::{config::Merge, error::AnyError};
use serde::Deserialize;

use crate::ReadConfig;

use super::toml::human_toml_error;

pub struct MultiFileConfigReader<Config> {
    config_file_paths: Arc<[Arc<str>]>,
    phantom_config: PhantomData<Config>,
}
impl<Config> MultiFileConfigReader<Config> {
    pub fn new(config_file_paths: Arc<[Arc<str>]>) -> Self {
        Self {
            config_file_paths,
            phantom_config: PhantomData,
        }
    }
}
impl<Config> ReadConfig for MultiFileConfigReader<Config>
where
    for<'de> Config: Deserialize<'de> + Send + Sync + 'static,
    Config: Merge<Error = AnyError>,
{
    type Config = Config;
    async fn read_config(&self) -> Result<Self::Config, AnyError> {
        let mut config = Config::default();
        for path in self.config_file_paths.iter() {
            // Name the path on a read failure. The parse below names it through
            // `human_toml_error`, but an `io::Error` carries only the OS
            // message (`Is a directory`, `No such file or directory`), so a
            // caller told the config could not be read would not be told
            // *which* of the configured paths it was.
            let src = tokio::fs::read_to_string(path.as_ref())
                .await
                .map_err(|e| {
                    AnyError::from(std::io::Error::new(e.kind(), format!("{path}: {e}")))
                })?;
            let c: Config = toml::from_str(&src).map_err(|e| human_toml_error(path, &src, e))?;
            config = config.merge(c)?;
        }
        Ok(config)
    }
}
