use std::{
    io::{self, Read, Write},
    time::Duration,
};

use duplicate::duplicate_item;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_chacha20::cursor::{DecryptResult, EncryptResult};
use tracing::{instrument, trace};

use crate::{anti_replay::ValidatorRef, header::timestamp::TimestampMsg};

pub const MAX_HEADER_LEN: usize = 1024;
const BINCODE_CONFIG: bincode::config::Configuration<
    bincode::config::LittleEndian,
    bincode::config::Varint,
    bincode::config::NoLimit,
> = bincode::config::standard();

pub trait AsHeader {}

#[duplicate_item(
    read_header         async   reader_bounds       add_await(code) ;
    [read_header]       []      [Read]              [code]          ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    ;
)]
#[instrument(skip_all)]
pub async fn read_header<Reader, Header>(
    reader: &mut Reader,
    cursor: &mut tokio_chacha20::cursor::DecryptCursor,
    validator: &ValidatorRef<'_>,
) -> Result<Header, CodecError>
where
    Reader: reader_bounds,
    Header: std::fmt::Debug + AsHeader + bincode::Decode<()>,
{
    let mut buf = [0; MAX_HEADER_LEN * 2];

    // Read nonce
    {
        let size = cursor.remaining_nonce_size();
        let buf = &mut buf[..size];
        let res = reader.read_exact(buf);
        add_await([res])?;
        match validator {
            ValidatorRef::Replay(replay_validator) => {
                if !replay_validator.nonce_validates(buf.try_into().unwrap()) {
                    return Err(CodecError::Integrity);
                }
            }
            ValidatorRef::Time(_) => {}
        }
        match cursor.decrypt(buf) {
            DecryptResult::StillAtNonce => (),
            DecryptResult::WithUserData { user_data_start: _ } => {
                return Err(CodecError::Integrity);
            }
        }
    }
    // Decode header length
    let len = {
        let size = core::mem::size_of::<u32>();
        let buf = &mut buf[..size];
        let res = reader.read_exact(buf);
        add_await([res])?;
        let i = match cursor.decrypt(buf) {
            DecryptResult::StillAtNonce => panic!(),
            DecryptResult::WithUserData { user_data_start } => user_data_start,
        };
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
        match cursor.decrypt(hdr_buf) {
            DecryptResult::StillAtNonce => panic!(),
            DecryptResult::WithUserData { user_data_start } => assert_eq!(user_data_start, 0),
        }
        let mut rdr = io::Cursor::new(&hdr_buf[..]);
        let mut timestamp_buf = [0; TimestampMsg::SIZE];
        Read::read_exact(&mut rdr, &mut timestamp_buf).unwrap();
        let timestamp = TimestampMsg::decode(timestamp_buf);
        if !validator.time_validates(timestamp.timestamp()) {
            return Err(CodecError::Integrity);
        }
        let header = bincode::decode_from_std_read(&mut rdr, BINCODE_CONFIG)?;
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
pub async fn write_header<Writer, Header>(
    writer: &mut Writer,
    header: &Header,
    cursor: &mut tokio_chacha20::cursor::EncryptCursor,
) -> Result<(), CodecError>
where
    Writer: writer_bounds,
    Header: std::fmt::Debug + AsHeader + bincode::Encode,
{
    // Encode header
    let mut hdr_buf = [0; TimestampMsg::SIZE + MAX_HEADER_LEN];
    let hdr_buf: &[u8] = {
        let mut hdr_wtr = io::Cursor::new(&mut hdr_buf[..]);
        let timestamp = TimestampMsg::now();
        Write::write_all(&mut hdr_wtr, &timestamp.encode()).unwrap();
        bincode::encode_into_std_write(header, &mut hdr_wtr, BINCODE_CONFIG)?;
        let len = hdr_wtr.position();
        let hdr = &mut hdr_buf[..len as usize];
        trace!(?timestamp, ?header, ?len, "Encoded header");
        hdr
    };

    let mut buf = [0; MAX_HEADER_LEN * 2];
    let mut pos = 0;

    // Write header length
    {
        let len = hdr_buf.len() as u32;
        let len = len.to_be_bytes();
        let EncryptResult { read, written } = cursor.encrypt(&len, &mut buf[pos..]);
        assert_eq!(len.len(), read);
        pos += written;
    }
    // Write header
    let encrypted_hdr = {
        let EncryptResult { read, written } = cursor.encrypt(hdr_buf, &mut buf[pos..]);
        assert_eq!(hdr_buf.len(), read);
        let encrypted_hdr = &buf[pos..pos + written];
        pos += written;
        encrypted_hdr
    };
    // Write tag
    let key = cursor.poly1305_key();
    let tag = tokio_chacha20::mac::poly1305_mac(key, encrypted_hdr);
    buf[pos..pos + tag.len()].copy_from_slice(&tag);
    pos += tag.len();

    add_await([writer.write_all(&buf[..pos])])?;

    Ok(())
}

pub async fn timed_read_header_async<Reader, Header>(
    reader: &mut Reader,
    cursor: &mut tokio_chacha20::cursor::DecryptCursor,
    validator: &ValidatorRef<'_>,
    timeout: Duration,
) -> Result<Header, CodecError>
where
    Reader: AsyncRead + Unpin,
    Header: std::fmt::Debug + AsHeader + bincode::Decode<()>,
{
    let res = tokio::time::timeout(timeout, read_header_async(reader, cursor, validator)).await;
    match res {
        Ok(res) => res,
        Err(_) => Err(CodecError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "Timed out",
        ))),
    }
}

pub async fn timed_write_header_async<Writer, Header>(
    writer: &mut Writer,
    header: &Header,
    cursor: &mut tokio_chacha20::cursor::EncryptCursor,
    timeout: Duration,
) -> Result<(), CodecError>
where
    Writer: AsyncWrite + Unpin,
    Header: std::fmt::Debug + AsHeader + bincode::Encode,
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
    #[error("Encode error: {0}")]
    Encode(#[from] bincode::error::EncodeError),
    #[error("Decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),
    #[error("Data tempered")]
    Integrity,
}
