use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SharableConfig<T> {
    SharingKey(Arc<str>),
    Private(T),
}

pub trait Merge: Default {
    type Error;
    /// # Error
    ///
    /// - Attempted key overriding
    fn merge(self, other: Self) -> Result<Self, Self::Error>
    where
        Self: Sized;
}

pub fn merge_map<K, V>(
    mut a: HashMap<K, V>,
    b: HashMap<K, V>,
) -> Result<HashMap<K, V>, RepeatKeyMergeError<K>>
where
    K: Eq + std::hash::Hash,
{
    for (k, v) in b {
        if a.contains_key(&k) {
            return Err(RepeatKeyMergeError(k));
        }
        a.insert(k, v);
    }
    Ok(a)
}
#[derive(Debug, Error)]
#[error("Repeated key: {0}")]
pub struct RepeatKeyMergeError<K>(pub K);
