use std::{
    io::{self, Read, Write},
    time::Duration,
};

use duplicate::duplicate_item;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

use crate::crypto::XorCryptoCursor;

pub const MAX_HEADER_LEN: usize = 1024;

pub trait Header {}

#[duplicate_item(
    read_header         async   reader_bounds       add_await(code) ;
    [read_header]       []      [Read]              [code]          ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn read_header<'crypto, R, H>(
    reader: &mut R,
    crypto: &mut XorCryptoCursor,
) -> Result<H, CodecError>
where
    R: reader_bounds,
    H: for<'de> Deserialize<'de> + std::fmt::Debug + Header,
{
    // Decode header length
    let len = {
        let mut buf = [0; 4];
        let res = reader.read_exact(&mut buf);
        add_await([res])?;
        crypto.xor(&mut buf);
        u32::from_be_bytes(buf) as usize
    };
    trace!(len, "Read header length");

    // Decode header
    let header = {
        let mut buf = [0; MAX_HEADER_LEN];
        if len > MAX_HEADER_LEN {
            return Err(CodecError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Header too long",
            )));
        }
        let hdr = &mut buf[..len];
        let res = reader.read_exact(hdr);
        add_await([res])?;
        crypto.xor(hdr);
        bincode::deserialize(hdr)?
    };
    trace!(?header, "Read header");

    Ok(header)
}

#[duplicate_item(
    write_header         async   writer_bounds        add_await(code) ;
    [write_header]       []      [Write]              [code]          ;
    [write_header_async] [async] [AsyncWrite + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn write_header<'crypto, W, H>(
    writer: &mut W,
    header: &H,
    crypto: &mut XorCryptoCursor,
) -> Result<(), CodecError>
where
    W: writer_bounds,
    H: Serialize + std::fmt::Debug + Header,
{
    let mut buf = [0; MAX_HEADER_LEN];
    let mut hdr_wtr = io::Cursor::new(&mut buf[..]);

    // Encode header
    let hdr = {
        bincode::serialize_into(&mut hdr_wtr, header)?;
        let len = hdr_wtr.position();
        let hdr = &mut buf[..len as usize];
        trace!(?header, ?len, "Encoded header");
        hdr
    };

    // Write header length
    let len = hdr.len() as u32;
    let mut len = len.to_be_bytes();
    crypto.xor(&mut len);
    add_await([writer.write_all(&len)])?;

    // Write header
    crypto.xor(hdr);
    add_await([writer.write_all(hdr)])?;

    Ok(())
}

pub async fn timed_read_header_async<'crypto, R, H>(
    reader: &mut R,
    crypto: &mut XorCryptoCursor,
    timeout: Duration,
) -> Result<H, CodecError>
where
    R: AsyncRead + Unpin,
    H: for<'de> Deserialize<'de> + std::fmt::Debug + Header,
{
    let res = tokio::time::timeout(timeout, read_header_async(reader, crypto)).await;
    match res {
        Ok(res) => res,
        Err(_) => Err(CodecError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "Timed out",
        ))),
    }
}

pub async fn timed_write_header_async<'crypto, W, H>(
    writer: &mut W,
    header: &H,
    crypto: &mut XorCryptoCursor,
    timeout: Duration,
) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
    H: Serialize + std::fmt::Debug + Header,
{
    let res = tokio::time::timeout(timeout, write_header_async(writer, header, crypto)).await;
    match res {
        Ok(res) => res,
        Err(_) => Err(CodecError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "Timed out",
        ))),
    }
}

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] bincode::Error),
}
