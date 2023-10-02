use std::io::{self, Write};

use tracing::{trace, warn};

use crate::{
    crypto::{XorCrypto, XorCryptoCursor},
    header::{
        codec::{read_header, write_header, CodecError},
        route::{RouteErrorKind, RouteResponse},
    },
    udp::respond::respond_with_error,
};

use super::{header::UdpRequestHeader, UdpDownstreamWriter, UpstreamAddr};

pub async fn steer<'buf>(
    buf: &'buf [u8],
    downstream_writer: &UdpDownstreamWriter,
    header_crypto: &XorCrypto,
) -> Result<Option<(UpstreamAddr, &'buf [u8])>, CodecError> {
    let res = decode_header(buf, header_crypto).await;
    match res {
        Ok((upstream_addr, payload)) => {
            // Proxy
            if let Some(addr) = upstream_addr {
                return Ok(Some((addr, payload)));
            }

            // Echo
            let resp = RouteResponse { result: Ok(()) };
            let mut wtr = Vec::new();
            let mut crypto_cursor = XorCryptoCursor::new(header_crypto);
            write_header(&mut wtr, &resp, &mut crypto_cursor).unwrap();
            wtr.write_all(payload).unwrap();
            let downstream_writer = downstream_writer.clone();
            tokio::spawn(async move {
                if let Err(e) = downstream_writer.send(&wtr).await {
                    warn!(
                        ?e,
                        ?downstream_writer,
                        "Failed to send response to downstream"
                    );
                };
            });
            Ok(None)
        }
        Err(err) => {
            handle_steer_error(downstream_writer, &err, header_crypto).await;
            Err(err)
        }
    }
}

async fn decode_header<'buf>(
    buf: &'buf [u8],
    header_crypto: &XorCrypto,
) -> Result<(Option<UpstreamAddr>, &'buf [u8]), CodecError> {
    // Decode header
    let mut reader = io::Cursor::new(buf);
    let mut crypto_cursor = XorCryptoCursor::new(header_crypto);
    let header: UdpRequestHeader = read_header(&mut reader, &mut crypto_cursor)?;
    let header_len = reader.position() as usize;
    let payload = &buf[header_len..];

    Ok((header.upstream.map(UpstreamAddr), payload))
}

async fn handle_steer_error(
    downstream_writer: &UdpDownstreamWriter,
    error: &CodecError,
    header_crypto: &XorCrypto,
) {
    let peer_addr = downstream_writer.peer_addr();
    warn!(?error, ?peer_addr, "Failed to steer");
    let kind = error_kind_from_header_error(error);
    if let Err(e) = respond_with_error(downstream_writer, kind, header_crypto).await {
        trace!(?e, ?peer_addr, "Failed to respond with error to downstream");
    }
}

fn error_kind_from_header_error(e: &CodecError) -> RouteErrorKind {
    match e {
        CodecError::Io(_) => RouteErrorKind::Io,
        CodecError::Serialization(_) => RouteErrorKind::Codec,
    }
}
