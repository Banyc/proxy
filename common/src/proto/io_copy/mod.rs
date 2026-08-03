use std::{
    io,
    task::{Context, Poll, Waker},
    time::Duration,
};

use monitor_table::table::RowOwnedGuard;
use tokio_chacha20::{
    KEY_BYTES, X_NONCE_BYTES,
    stream::{
        NonceBuf, NonceCiphertextReader, NonceCiphertextReaderConfig, NonceCiphertextWriter,
        NonceCiphertextWriterConfig,
    },
};

pub mod stream;
pub mod udp;

pub const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub fn retain_dead_session<Session: Send + Sync + 'static>(session: RowOwnedGuard<Session>) {
    tokio::spawn(async move {
        let _session = session;
        tokio::time::sleep(DEAD_SESSION_RETENTION_DURATION).await;
    });
}

#[derive(Debug, Clone, Copy)]
pub enum EncryptionDirection {
    Encrypt,
    Decrypt,
}
impl EncryptionDirection {
    pub fn flip(&self) -> Self {
        match self {
            Self::Encrypt => Self::Decrypt,
            Self::Decrypt => Self::Encrypt,
        }
    }
}

pub fn same_key_nonce_ciphertext<R, W>(
    key: &[u8; KEY_BYTES],
    r: R,
    w: W,
) -> (NonceCiphertextReader<R>, NonceCiphertextWriter<W>) {
    let r = nonce_ciphertext_reader(key, r);
    let w = nonce_ciphertext_writer(key, w);
    (r, w)
}
fn nonce_ciphertext_reader<R>(key: &[u8; KEY_BYTES], r: R) -> NonceCiphertextReader<R> {
    let reader_config = NonceCiphertextReaderConfig { hash: false };
    let nonce_buf = NonceBuf::XNonce(Box::new([0; X_NONCE_BYTES]));
    NonceCiphertextReader::new(&reader_config, Box::new(*key), nonce_buf, r)
}
fn nonce_ciphertext_writer<W>(key: &[u8; KEY_BYTES], w: W) -> NonceCiphertextWriter<W> {
    let writer_config = NonceCiphertextWriterConfig {
        write_nonce: true,
        key,
        hash: false,
    };
    let nonce = NonceBuf::XNonce(Box::new(rand::random()));
    NonceCiphertextWriter::new(&writer_config, nonce, w)
}

fn noop_context() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn unwrap_ready<T>(poll: Poll<io::Result<T>>) -> io::Result<T> {
    match poll {
        Poll::Ready(x) => x,
        Poll::Pending => Err(io::Error::from(io::ErrorKind::WouldBlock)),
    }
}
