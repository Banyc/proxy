use std::io;

use tokio::net::UdpSocket;
use tracing::trace;
use udp_listener::ConnWrite;

use crate::header::{
    codec::write_header,
    route::{RouteError, RouteErrorKind, RouteResponse},
};

pub async fn respond_with_error(
    dn_writer: &ConnWrite<UdpSocket>,
    kind: RouteErrorKind,
    header_crypto: &tokio_chacha20::config::Config,
) -> Result<(), io::Error> {
    // Respond with error
    let resp = RouteResponse {
        result: Err(RouteError { kind }),
    };
    let mut buf = Vec::new();
    write_header(&mut buf, &resp, *header_crypto.key()).unwrap();
    dn_writer.send(&buf).await.map_err(|e| {
        let peer_addr = dn_writer.peer_addr();
        trace!(?e, ?peer_addr, "Failed to send response to downstream");
        e
    })?;

    Ok(())
}
