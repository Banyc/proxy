use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SharableConfig<T> {
    SharingKey(Arc<str>),
    Private(T),
}
