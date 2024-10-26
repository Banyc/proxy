use std::io;

use tracing::trace;
use udp_listener::ConnWrite;

use crate::header::{
    codec::write_header,
    route::{RouteError, RouteErrorKind, RouteResponse},
};

pub async fn respond_with_error(
    dn_writer: &ConnWrite,
    kind: RouteErrorKind,
    header_crypto: &tokio_chacha20::config::Config,
) -> Result<(), io::Error> {
    // Respond with error
    let resp = RouteResponse {
        result: Err(RouteError { kind }),
    };
    let mut buf = Vec::new();
    let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new(*header_crypto.key());
    write_header(&mut buf, &resp, &mut crypto_cursor).unwrap();
    dn_writer.send(&buf).await.map_err(|e| {
        let peer_addr = dn_writer.peer_addr();
        trace!(?e, ?peer_addr, "Failed to send response to downstream");
        e
    })?;

    Ok(())
}
