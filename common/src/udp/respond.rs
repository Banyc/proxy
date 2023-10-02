use std::io;

use tracing::trace;

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    header::{
        codec::write_header,
        route::{RouteError, RouteErrorKind, RouteResponse},
    },
};

use super::UdpDownstreamWriter;

pub async fn respond_with_error(
    downstream_writer: &UdpDownstreamWriter,
    kind: RouteErrorKind,
    header_crypto: &XorCrypto,
) -> Result<(), io::Error> {
    // Respond with error
    let resp = RouteResponse {
        result: Err(RouteError { kind }),
    };
    let mut buf = Vec::new();
    let mut crypto_cursor = XorCryptoCursor::new(header_crypto);
    write_header(&mut buf, &resp, &mut crypto_cursor).unwrap();
    downstream_writer.send(&buf).await.map_err(|e| {
        let peer_addr = downstream_writer.peer_addr();
        trace!(?e, ?peer_addr, "Failed to send response to downstream");
        e
    })?;

    Ok(())
}
