use std::{
    io::{self, Read, Write},
    time::Duration,
};

use duplicate::duplicate_item;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{instrument, trace};

use crate::header::timestamp::{ReplayValidator, TimestampMsg, TIME_FRAME};

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
    let mut buf = [0; MAX_HEADER_LEN * 2];

    // Read nonce
    {
        let size = cursor.remaining_nonce_size();
        let buf = &mut buf[..size];
        let res = reader.read_exact(buf);
        add_await([res])?;
        let i = cursor.decrypt(buf);
        if i.is_some() {
            return Err(CodecError::Integrity);
        }
    }
    // Decode header length
    let len = {
        let size = core::mem::size_of::<u32>();
        let buf = &mut buf[..size];
        let res = reader.read_exact(buf);
        add_await([res])?;
        let i = cursor.decrypt(buf).unwrap();
        if i != 0 {
            return Err(CodecError::Integrity);
        }
        let len = u32::from_be_bytes(buf.try_into().unwrap()) as usize;
        trace!(len, "Read header length");
        if len > MAX_HEADER_LEN {
            return Err(CodecError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Header too long",
            )));
        }
        len
    };
    // Read header and tag
    let (hdr_buf, tag) = {
        let buf = &mut buf[..len + tokio_chacha20::mac::BLOCK_BYTES];
        let res = reader.read_exact(buf);
        add_await([res])?;
        let (hdr, tag) = buf.split_at_mut(len);
        let tag: &[u8] = tag;
        (hdr, tag)
    };
    // Check MAC
    {
        let key = cursor.poly1305_key().unwrap();
        let expected_tag = tokio_chacha20::mac::poly1305_mac(key, hdr_buf);
        if tag != expected_tag {
            return Err(CodecError::Integrity);
        }
    }
    // Decode header
    let header = {
        let i = cursor.decrypt(hdr_buf).unwrap();
        assert_eq!(i, 0);
        let mut rdr = io::Cursor::new(&hdr_buf[..]);
        let mut timestamp_buf = [0; TimestampMsg::SIZE];
        Read::read_exact(&mut rdr, &mut timestamp_buf).unwrap();
        let timestamp = TimestampMsg::decode(timestamp_buf);
        if !ReplayValidator::new(TIME_FRAME).validates(timestamp.timestamp()) {
            return Err(CodecError::Integrity);
        }
        let header = bincode::deserialize_from(&mut rdr)?;
        trace!(?timestamp, ?header, "Read header");
        header
    };

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
    // Encode header
    let mut hdr_buf = [0; TimestampMsg::SIZE + MAX_HEADER_LEN];
    let hdr_buf: &[u8] = {
        let mut hdr_wtr = io::Cursor::new(&mut hdr_buf[..]);
        let timestamp = TimestampMsg::now();
        Write::write_all(&mut hdr_wtr, &timestamp.encode()).unwrap();
        bincode::serialize_into(&mut hdr_wtr, header)?;
        let len = hdr_wtr.position();
        let hdr = &mut hdr_buf[..len as usize];
        trace!(?timestamp, ?header, ?len, "Encoded header");
        hdr
    };

    let mut buf = [0; MAX_HEADER_LEN * 2];
    let mut n = 0;

    // Write header length
    {
        let len = hdr_buf.len() as u32;
        let len = len.to_be_bytes();
        let (f, t) = cursor.encrypt(&len, &mut buf[n..]);
        assert_eq!(len.len(), f);
        n += t;
    }
    // Write header
    let encrypted_hdr = {
        let (f, t) = cursor.encrypt(hdr_buf, &mut buf[n..]);
        assert_eq!(hdr_buf.len(), f);
        let encrypted_hdr = &buf[n..n + t];
        n += t;
        encrypted_hdr
    };
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

pub async fn timed_write_header_async<W, H>(
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
