use std::{
    io::{self, Read, Write},
    time::Duration,
};

use duplicate::duplicate_item;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

pub const MAX_HEADER_LEN: usize = 1024;

pub trait Header {}

#[duplicate_item(
    read_header         async   reader_bounds       add_await(code) ;
    [read_header]       []      [Read]              [code]          ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn read_header<'cursor, R, H>(
    reader: &mut R,
    cursor: &mut tokio_chacha20::cursor::DecryptCursor,
) -> Result<H, CodecError>
where
    R: reader_bounds,
    H: for<'de> Deserialize<'de> + std::fmt::Debug + Header,
{
    let mut buf = [0; MAX_HEADER_LEN + 16];

    // Decode header length
    let len = {
        let n = 4 + cursor.remaining_nonce_size();
        let res = reader.read_exact(&mut buf[..n]);
        add_await([res])?;
        let i = cursor.decrypt(&mut buf[..n]).unwrap();
        u32::from_be_bytes(buf[i..n].try_into().unwrap()) as usize
    };
    trace!(len, "Read header length");

    // Read header and tag
    if len > MAX_HEADER_LEN {
        return Err(CodecError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "Header too long",
        )));
    }
    let hdr_tag = &mut buf[..len + 16];
    let res = reader.read_exact(hdr_tag);
    add_await([res])?;
    let (hdr, tag) = hdr_tag.split_at_mut(len);
    let tag: &[u8] = tag;

    // Check MAC
    let key = cursor.poly1305_key().unwrap();
    let expected_tag = tokio_chacha20::mac::poly1305_mac(key, hdr);
    if tag != expected_tag {
        return Err(CodecError::Integrity);
    }

    // Decode header
    let header = {
        let i = cursor.decrypt(hdr).unwrap();
        assert_eq!(i, 0);
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
pub async fn write_header<'cursor, W, H>(
    writer: &mut W,
    header: &H,
    cursor: &mut tokio_chacha20::cursor::EncryptCursor,
) -> Result<(), CodecError>
where
    W: writer_bounds,
    H: Serialize + std::fmt::Debug + Header,
{
    let mut hdr_buf = [0; MAX_HEADER_LEN];
    let mut hdr_wtr = io::Cursor::new(&mut hdr_buf[..]);
    let mut buf = [0; MAX_HEADER_LEN * 2];

    // Encode header
    let hdr = {
        bincode::serialize_into(&mut hdr_wtr, header)?;
        let len = hdr_wtr.position();
        let hdr = &mut hdr_buf[..len as usize];
        trace!(?header, ?len, "Encoded header");
        hdr
    };

    let mut n = 0;

    // Write header length
    let len = hdr.len() as u32;
    let len = len.to_be_bytes();
    let (f, t) = cursor.encrypt(&len, &mut buf[n..]);
    assert_eq!(len.len(), f);
    n += t;

    // Write header
    let (f, t) = cursor.encrypt(hdr, &mut buf[n..]);
    assert_eq!(hdr.len(), f);
    let encrypted_hdr = &buf[n..n + t];
    n += t;

    // Write tag
    let key = cursor.poly1305_key();
    let tag = tokio_chacha20::mac::poly1305_mac(key, encrypted_hdr);
    buf[n..n + tag.len()].copy_from_slice(&tag);
    n += tag.len();

    add_await([writer.write_all(&buf[..n])])?;

    Ok(())
}

pub async fn timed_read_header_async<'cursor, R, H>(
    reader: &mut R,
    cursor: &mut tokio_chacha20::cursor::DecryptCursor,
    timeout: Duration,
) -> Result<H, CodecError>
where
    R: AsyncRead + Unpin,
    H: for<'de> Deserialize<'de> + std::fmt::Debug + Header,
{
    let res = tokio::time::timeout(timeout, read_header_async(reader, cursor)).await;
    match res {
        Ok(res) => res,
        Err(_) => Err(CodecError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "Timed out",
        ))),
    }
}

pub async fn timed_write_header_async<'cursor, W, H>(
    writer: &mut W,
    header: &H,
    cursor: &mut tokio_chacha20::cursor::EncryptCursor,
    timeout: Duration,
) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
    H: Serialize + std::fmt::Debug + Header,
{
    let res = tokio::time::timeout(timeout, write_header_async(writer, header, cursor)).await;
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
    #[error("Data tempered")]
    Integrity,
}
