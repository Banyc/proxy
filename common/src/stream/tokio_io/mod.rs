mod copy;
mod copy_bidirectional;

pub use copy_bidirectional::copy_bidirectional;

const DEFAULT_BUF_SIZE: usize = 8 * 1024;
