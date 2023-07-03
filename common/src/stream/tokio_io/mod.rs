mod copy;
mod copy_bidirectional;
mod timed_copy_bidirectional;
mod timeout_stream;

pub use copy_bidirectional::{copy_bidirectional, CopyBiError};
pub use timed_copy_bidirectional::timed_copy_bidirectional;
pub use timeout_stream::TimeoutStreamShared;

const DEFAULT_BUF_SIZE: usize = 8 * 1024;
