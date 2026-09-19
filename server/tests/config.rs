//! Exercise the multi-file config reader: it merges the files in order and
//! surfaces per-file parse/merge errors.
//!
//! The real on-disk watcher is exercised in `config_watch.rs`, which must
//! leak its runtime so the notify callback channel never closes (see that
//! test for the reason).

use std::{collections::BTreeMap, sync::Arc};

use common::{config::Merge, error::AnyError};
use serde::Deserialize;
use server::config::{ReadConfig, multi_file_config::MultiFileConfigReader};

/// A minimal mergeable config: files contribute disjoint entries, and a
/// repeated key is an error — the same contract `ServerConfig` implements.
#[derive(Debug, Default, Deserialize, PartialEq)]
struct Fragment {
    #[serde(default)]
    entries: BTreeMap<String, u32>,
}

impl Merge for Fragment {
    type Error = AnyError;

    fn merge(mut self, other: Self) -> Result<Self, Self::Error> {
        for (key, value) in other.entries {
            if self.entries.insert(key.clone(), value).is_some() {
                return Err(AnyError::from(common::config::RepeatKeyMergeError(key)));
            }
        }
        Ok(self)
    }
}

/// A unique temp directory for one test run.
fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "proxy-config-test-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ))
}

fn write_file(dir: &std::path::Path, name: &str, body: &str) -> Arc<str> {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    Arc::from(path.to_str().unwrap())
}

#[tokio::test]
async fn multi_file_reader_merges_each_file_in_order() {
    let dir = unique_temp_dir("merge");
    std::fs::create_dir_all(&dir).unwrap();
    let first = write_file(&dir, "a.toml", "[entries]\nalpha = 1\n");
    let second = write_file(&dir, "b.toml", "[entries]\nbeta = 2\n");

    let reader = MultiFileConfigReader::<Fragment>::new(vec![first, second].into());
    let config = reader.read_config().await.unwrap();
    let expected = BTreeMap::from([("alpha".to_string(), 1), ("beta".to_string(), 2)]);
    assert_eq!(
        config.entries, expected,
        "both files' entries must survive the merge"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn multi_file_reader_rejects_a_key_defined_in_two_files() {
    let dir = unique_temp_dir("repeat");
    std::fs::create_dir_all(&dir).unwrap();
    let first = write_file(&dir, "a.toml", "[entries]\ndup = 1\n");
    let second = write_file(&dir, "b.toml", "[entries]\ndup = 2\n");

    let reader = MultiFileConfigReader::<Fragment>::new(vec![first, second].into());
    let error = reader
        .read_config()
        .await
        .expect_err("a key defined in two files must be rejected");
    let text = format!("{error}");
    assert!(
        text.contains("dup"),
        "the merge error must name the repeated key: {text}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn multi_file_reader_reports_the_path_of_a_malformed_file() {
    let dir = unique_temp_dir("malformed");
    std::fs::create_dir_all(&dir).unwrap();
    let bad = write_file(&dir, "broken.toml", "entries = = =\n");

    let reader = MultiFileConfigReader::<Fragment>::new(vec![bad.clone()].into());
    let error = reader
        .read_config()
        .await
        .expect_err("a malformed file must not be accepted");
    let text = format!("{error}");
    assert!(
        text.contains(bad.as_ref()),
        "the parse error must name the offending file: {text}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
