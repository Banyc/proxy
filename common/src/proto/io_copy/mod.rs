use std::task::{Context, Poll, Waker};

use tokio_chacha20::{
    KEY_BYTES, X_NONCE_BYTES,
    stream::{
        NonceBuf, NonceCiphertextReader, NonceCiphertextReaderConfig, NonceCiphertextTagWriter,
        NonceCiphertextTagWriterConfig,
    },
};

pub mod stream;
pub mod udp;

pub fn same_key_nonce_ciphertext<R, W>(
    key: &[u8; KEY_BYTES],
    r: R,
    w: W,
) -> (NonceCiphertextReader<R>, NonceCiphertextTagWriter<W>) {
    let r = nonce_ciphertext_reader(key, r);
    let w = nonce_ciphertext_writer(key, w);
    (r, w)
}
fn nonce_ciphertext_reader<R>(key: &[u8; KEY_BYTES], r: R) -> NonceCiphertextReader<R> {
    let reader_config = NonceCiphertextReaderConfig { hash: false };
    let nonce_buf = NonceBuf::XNonce(Box::new([0; X_NONCE_BYTES]));
    NonceCiphertextReader::new(&reader_config, Box::new(*key), nonce_buf, r)
}
fn nonce_ciphertext_writer<W>(key: &[u8; KEY_BYTES], w: W) -> NonceCiphertextTagWriter<W> {
    let writer_config = NonceCiphertextTagWriterConfig {
        write_nonce: true,
        write_tag: false,
        key,
    };
    let nonce = NonceBuf::XNonce(Box::new(rand::random()));
    NonceCiphertextTagWriter::new(&writer_config, nonce, w)
}

fn noop_context() -> Context<'static> {
    Context::from_waker(Waker::noop())
}
fn unwrap_ready<T>(poll: Poll<T>) -> T {
    match poll {
        Poll::Ready(x) => x,
        Poll::Pending => panic!(),
    }
}
