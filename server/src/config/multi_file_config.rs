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
            // A zero-byte file is not a configuration: it is what a *truncated*
            // write looks like from the reader's side. Read in that window — a
            // writer that truncates and then writes, a stalled or aborted
            // deployment, a slow filesystem — an empty file parses as the
            // default configuration, which has no listeners at all, so
            // applying it would retire every listener of the live generation
            // and drop every session on them; the write that follows would
            // then restore a listener nobody is connected to. Refusing it
            // leaves the live configuration serving and reports the path, the
            // way any other unreadable config does. No configuration of this
            // product is zero bytes — an intentionally listenerless config is
            // a comment — so this costs an operator nothing.
            if src.is_empty() {
                return Err(AnyError::from(std::io::Error::other(format!(
                    "{path}: the config file is empty, which is what a truncated write looks \
                     like from the reader's side, so it is refused rather than applied (write \
                     at least a comment to mean an intentionally empty configuration)"
                ))));
            }
            let c: Config = toml::from_str(&src).map_err(|e| human_toml_error(path, &src, e))?;
            config = config.merge(c)?;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerConfig;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "proxy-multi-file-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    async fn read(paths: &[Arc<str>]) -> Result<ServerConfig, AnyError> {
        MultiFileConfigReader::<ServerConfig>::new(paths.to_vec().into())
            .read_config()
            .await
    }

    /// A zero-byte file is refused, with the path named, however many files the
    /// set has: the caller is told which file it has to look at. This is the
    /// guard that stops a *truncated* write — a writer that truncates and then
    /// writes leaves exactly this file on disk — from being applied as the
    /// empty (and listenerless) configuration, which would retire every
    /// listener of the live generation and drop every session on them.
    #[tokio::test]
    async fn a_zero_byte_config_file_is_refused_and_named() {
        let empty = temp_path("empty");
        let other = temp_path("other");
        std::fs::write(&empty, "").unwrap();
        // A valid sibling, so the refusal cannot be about a missing second
        // file: the empty one is what must be rejected, and by name.
        std::fs::write(
            &other,
            "[access_server.stream.conn_selector]\n\"default\" = { chains = [] }\n",
        )
        .unwrap();
        let paths: Vec<Arc<str>> = [&empty, &other]
            .iter()
            .map(|p| Arc::<str>::from(p.to_str().unwrap()))
            .collect();

        let error = read(&paths)
            .await
            .expect_err("a zero-byte config file must be refused, not applied");
        let text = error.to_string();
        assert!(
            text.contains(empty.to_str().unwrap()),
            "the refusal must name the empty file: {text}"
        );
        assert!(
            text.contains("empty"),
            "the refusal must say the file is empty: {text}"
        );

        std::fs::remove_file(&empty).ok();
        std::fs::remove_file(&other).ok();
    }

    /// The control for the test above: the refusal is about the *bytes* being
    /// absent, not about a configuration that names no listener. A comment-only
    /// file is this workspace's idiom for "no configuration" — it is what the
    /// lifecycle soak writes to retire a generation — and it must still be
    /// accepted, or the guard would have banned a legitimate config shape.
    #[tokio::test]
    async fn a_comment_only_config_file_is_accepted_as_an_empty_configuration() {
        let path = temp_path("comment");
        std::fs::write(&path, "# intentionally empty\n").unwrap();
        let watched: Arc<str> = Arc::from(path.to_str().unwrap());

        read(&[watched])
            .await
            .expect("a comment-only config file is a valid empty configuration");

        std::fs::remove_file(&path).ok();
    }
}
