mod copy;
mod copy_bidirectional;
mod timed_copy_bidirectional;

pub use copy_bidirectional::{copy_bidirectional, CopyBiError};
pub use timed_copy_bidirectional::{timed_copy_bidirectional, TimedCopyBiError};

const DEFAULT_BUF_SIZE: usize = 8 * 1024;
