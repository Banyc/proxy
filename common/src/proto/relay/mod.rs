use monitor_table::table::RowOwnedGuard;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll, Waker},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_chacha20::{
    KEY_BYTES, X_NONCE_BYTES,
    stream::{
        NonceBuf, NonceCiphertextReader, NonceCiphertextReaderConfig, NonceCiphertextWriter,
        NonceCiphertextWriterConfig,
    },
};

pub mod copy;
pub mod stream;
pub mod udp;

pub const DEAD_SESSION_RETENTION_DURATION: Duration = Duration::from_secs(5);

pub async fn retain_dead_session<Session: Send + Sync + 'static>(
    session: RowOwnedGuard<Session>,
    retention: &crate::lifecycle::retention::RetentionActorSender,
) {
    retention
        .retain(
            Box::new(session),
            std::time::Instant::now() + DEAD_SESSION_RETENTION_DURATION,
        )
        .await;
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

pub(crate) fn encrypt_packet_payload<'a>(
    packet: &[u8],
    output: &'a mut [u8],
    crypto: &tokio_chacha20::config::Config,
) -> io::Result<&'a [u8]> {
    let required = packet
        .len()
        .checked_add(X_NONCE_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "packet length overflow"))?;
    if required > output.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "encrypted packet does not fit in output buffer",
        ));
    }
    let mut cursor = io::Cursor::new(&mut output[..required]);
    let mut writer = nonce_ciphertext_writer(crypto.key(), &mut cursor);
    let written = unwrap_ready(Pin::new(&mut writer).poll_write(&mut noop_context(), packet))?;
    if written != packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "payload cipher did not consume the whole packet",
        ));
    }
    let output_len = usize::try_from(cursor.position()).unwrap();
    Ok(&output[..output_len])
}

pub(crate) fn decrypt_packet_payload<'a>(
    packet: &[u8],
    output: &'a mut [u8],
    crypto: &tokio_chacha20::config::Config,
) -> io::Result<&'a [u8]> {
    let output_len = packet.len().checked_sub(X_NONCE_BYTES).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "encrypted packet has no nonce",
        )
    })?;
    if output_len > output.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "decrypted packet does not fit in output buffer",
        ));
    }
    let mut reader = nonce_ciphertext_reader(crypto.key(), packet);
    let mut read_buf = ReadBuf::new(&mut output[..output_len]);
    unwrap_ready(Pin::new(&mut reader).poll_read(&mut noop_context(), &mut read_buf))?;
    if read_buf.filled().len() != output_len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "payload cipher did not produce the whole packet",
        ));
    }
    Ok(&output[..output_len])
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
