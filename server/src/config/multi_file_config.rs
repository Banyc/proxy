use std::{marker::PhantomData, sync::Arc};

use common::{config::Merge, error::AnyError};
use serde::Deserialize;

use crate::ReadConfig;

use super::toml::human_toml_error;

pub struct MultiConfigReader<Config> {
    config_file_paths: Arc<[Arc<str>]>,
    phantom_config: PhantomData<Config>,
}
impl<Config> MultiConfigReader<Config> {
    pub fn new(config_file_paths: Arc<[Arc<str>]>) -> Self {
        Self {
            config_file_paths,
            phantom_config: PhantomData,
        }
    }
}
impl<Config> ReadConfig for MultiConfigReader<Config>
where
    for<'de> Config: Deserialize<'de> + Send + Sync + 'static,
    Config: Merge<Error = AnyError>,
{
    type Config = Config;
    async fn read_config(&self) -> Result<Self::Config, AnyError> {
        let mut config = Config::default();
        for path in self.config_file_paths.iter() {
            let src = tokio::fs::read_to_string(path.as_ref()).await?;
            let c: Config = toml::from_str(&src).map_err(|e| human_toml_error(path, &src, e))?;
            config = config.merge(c)?;
        }
        Ok(config)
    }
}
