use std::io::{self, Write};

use metrics::counter;
use tokio::net::UdpSocket;
use tracing::warn;
use udp_listener::ConnWrite;

use crate::{
    anti_replay::{TimeValidator, ValidatorRef},
    header::{
        codec::{CodecError, read_header, write_header},
        route::RouteResponse,
    },
    proto::{conn::udp::UpstreamAddr, header::UdpRequestHeader},
};

pub async fn echo(
    buf: &[u8],
    dn_writer: &ConnWrite<UdpSocket>,
    header_crypto: &tokio_chacha20::config::Config,
) {
    let resp = RouteResponse { result: Ok(()) };
    let mut wtr = Vec::new();
    let mut crypto_cursor = tokio_chacha20::cursor::EncryptCursor::new_x(*header_crypto.key());
    write_header(&mut wtr, &resp, &mut crypto_cursor).unwrap();
    wtr.write_all(buf).unwrap();
    let dn_writer = dn_writer.clone();
    tokio::spawn(async move {
        if let Err(e) = dn_writer.send(&wtr).await {
            warn!(?e, ?dn_writer, "Failed to send response to downstream");
        };
    });
    counter!("udp.echoes").increment(1);
}

pub fn decode_route_header(
    buf: &mut io::Cursor<&[u8]>,
    header_crypto: &tokio_chacha20::config::Config,
    time_validator: &TimeValidator,
) -> Result<Option<UpstreamAddr>, CodecError> {
    // Decode header
    let mut crypto_cursor = tokio_chacha20::cursor::DecryptCursor::new_x(*header_crypto.key());
    let validator = ValidatorRef::Time(time_validator);
    let header: UdpRequestHeader = read_header(buf, &mut crypto_cursor, &validator)?;

    Ok(header.upstream.map(UpstreamAddr))
}
