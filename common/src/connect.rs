use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

use crate::{addr::DualStackBind, config::Merge, notify::Notify};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConfig {
    pub bind: DualStackBind,
    /// Optional datagram obfuscation key for the rtp/rtpmux connectors:
    /// when set, every RTP datagram is prefixed with a 24-byte random nonce
    /// and chacha20-encrypted with this key. The peer server must use the
    /// same key; `None` (the default) sends datagrams in the clear.
    #[serde(default)]
    pub obfuscation_key: Option<[u8; 32]>,
}
impl Default for ConnectorConfig {
    fn default() -> Self {
        Self {
            bind: DualStackBind { v4: None, v6: None },
            obfuscation_key: None,
        }
    }
}
impl Merge for ConnectorConfig {
    type Error = String;
    fn merge(mut self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        self.bind.v4 = option_merge(self.bind.v4, other.bind.v4)
            .map_err(|()| String::from("repeated bind.v4"))?;
        self.bind.v6 = option_merge(self.bind.v6, other.bind.v6)
            .map_err(|()| String::from("repeated bind.v6"))?;
        self.obfuscation_key = option_merge(self.obfuscation_key, other.obfuscation_key)
            .map_err(|()| String::from("repeated obfuscation_key"))?;
        Ok(self)
    }
}

fn option_merge<T>(a: Option<T>, b: Option<T>) -> Result<Option<T>, ()> {
    Ok(match (a, b) {
        (Some(_), Some(_)) => {
            return Err(());
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    })
}

/// The process connector configuration, split into two capabilities:
///
/// - [`ConnectorConfigReader`] — cloneable, shared by the stream connector
///   table, the UDP connector, and every mux UDP dialer, exposing only
///   [`Self::current`].
/// - [`ConnectorConfigUpdater`] — not cloneable, held solely by the server
///   reload path, exposing only [`Self::replace`].
///
/// Both halves share one `Arc<RwLock<...>>` cell that is never exposed, so
/// consumers cannot fork the configuration into a divergent cell, and the
/// replacement authority cannot be duplicated through a clone.
#[derive(Debug, Clone)]
pub struct ConnectorConfigReader(Arc<RwLock<ConnectorConfig>>);

impl ConnectorConfigReader {
    /// A snapshot of the current configuration.
    pub fn current(&self) -> ConnectorConfig {
        self.0.read().unwrap().clone()
    }
}

#[derive(Debug)]
pub struct ConnectorConfigUpdater(Arc<RwLock<ConnectorConfig>>);

impl ConnectorConfigUpdater {
    /// Replace the shared configuration in a single write, visible to every
    /// reader. The sole updater is retained by the server reload path.
    pub fn replace(&self, config: ConnectorConfig) {
        *self.0.write().unwrap() = config;
    }
}

/// Create the shared connector-configuration cell, handing out the reader
/// capability (cloneable, for every connector) and the sole updater
/// capability (for the server reload path).
pub fn connector_config_cell(
    config: ConnectorConfig,
) -> (ConnectorConfigReader, ConnectorConfigUpdater) {
    let cell = Arc::new(RwLock::new(config));
    (
        ConnectorConfigReader(Arc::clone(&cell)),
        ConnectorConfigUpdater(cell),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reader_clone_observes_the_same_replacement() {
        let (reader, updater) = connector_config_cell(ConnectorConfig::default());
        let stream_reader = reader.clone();
        let udp_reader = reader.clone();
        let replaced = ConnectorConfig {
            bind: DualStackBind {
                v4: Some("192.0.2.1".parse().unwrap()),
                v6: None,
            },
            obfuscation_key: None,
        };
        updater.replace(replaced);
        assert_eq!(
            stream_reader.current().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
        assert_eq!(
            udp_reader.current().bind.v4,
            Some("192.0.2.1".parse().unwrap())
        );
    }
}

#[derive(Debug, Clone)]
pub struct ConnectorResetSignal(pub Notify);
