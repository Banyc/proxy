use std::sync::Arc;

use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SharableConfig<T> {
    SharingKey(Arc<str>),
    Private(T),
}
