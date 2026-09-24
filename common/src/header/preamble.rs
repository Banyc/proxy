use std::time::Duration;

use ae::anti_replay::ValidatorRef;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use super::codec::{AsHeader, CodecError, read_header_async, write_header_async};

pub async fn send_keep_alive<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), PreambleError>
where
    Stream: AsyncWrite + Unpin,
{
    let req = Preamble::KeepAlive;
    let res = tokio::time::timeout(timeout, write_header_async(stream, &req, *crypto.key())).await;
    res.map_err(|_| PreambleError::Timeout(timeout))??;
    Ok(())
}

pub async fn send_upgrade<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
) -> Result<(), PreambleError>
where
    Stream: AsyncWrite + Unpin,
{
    let req = Preamble::Upgrade;
    let res = tokio::time::timeout(timeout, write_header_async(stream, &req, *crypto.key())).await;
    res.map_err(|_| PreambleError::Timeout(timeout))??;
    Ok(())
}

pub async fn wait_upgrade<Stream>(
    stream: &mut Stream,
    timeout: Duration,
    crypto: &tokio_chacha20::config::Config,
    validator: &ValidatorRef<'_>,
) -> Result<(), PreambleError>
where
    Stream: AsyncRead + Unpin,
{
    loop {
        let res =
            tokio::time::timeout(timeout, read_header_async(stream, *crypto.key(), validator))
                .await;
        let header: Preamble = res.map_err(|_| PreambleError::Timeout(timeout))??;
        if header == Preamble::Upgrade {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preamble {
    KeepAlive,
    Upgrade,
}
impl AsHeader for Preamble {}

#[derive(Debug, Error)]
pub enum PreambleError {
    #[error("Failed to read/write header: {0}")]
    Header(#[from] CodecError),
    #[error("Timeout: {0:?}")]
    Timeout(Duration),
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use ae::anti_replay::{ReplayValidator, ValidatorRef};

    use super::*;
    use crate::anti_replay::{VALIDATOR_CAPACITY, VALIDATOR_TIME_FRAME};
    use crate::header::codec::write_header;

    /// `wait_upgrade` waits for the peer's upgrade and *skips* every other
    /// preamble. The pool's liveness heartbeat is a `KeepAlive`, so a wait that
    /// ended at the first preamble it read would take a heartbeat for the
    /// upgrade and hand its caller a stream positioned at the bytes that follow
    /// it — the proxy server would parse the next preamble as a relay header
    /// instead of waiting for the real upgrade. Asserting the consumed byte
    /// count pins both directions of the guard at once: only `Upgrade` ends the
    /// wait, and every keep-alive before it is consumed.
    #[tokio::test]
    async fn wait_upgrade_skips_keep_alives_and_stops_at_the_upgrade() {
        let key = [7u8; tokio_chacha20::KEY_BYTES];
        let crypto = tokio_chacha20::config::Config::new(key.into());
        let wire_key = *crypto.key();
        let mut wire = Vec::new();
        for _ in 0..2 {
            let before = wire.len();
            write_header(&mut wire, &Preamble::KeepAlive, wire_key).unwrap();
            assert!(
                wire.len() > before,
                "a keep-alive must be written to the wire"
            );
        }
        write_header(&mut wire, &Preamble::Upgrade, wire_key).unwrap();
        let boundary = wire.len();
        wire.extend_from_slice(b"relay header");

        let validator = ReplayValidator::new(VALIDATOR_TIME_FRAME, VALIDATOR_CAPACITY);
        let mut reader = Cursor::new(&wire[..]);
        wait_upgrade(
            &mut reader,
            Duration::from_secs(1),
            &crypto,
            &ValidatorRef::Replay(&validator),
        )
        .await
        .unwrap();
        assert_eq!(
            reader.position() as usize,
            boundary,
            "the wait must consume the keep-alives and the upgrade, and nothing more"
        );
        assert_eq!(&wire[reader.position() as usize..], b"relay header");
    }
}
