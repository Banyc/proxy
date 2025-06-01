use std::{
    io::{self, Read, Write},
    time::Duration,
};

use ae::anti_replay::ValidatorRef;
use duplicate::duplicate_item;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{instrument, trace};

pub const MAX_HEADER_LEN: usize = 1024;
const BINCODE_CONFIG: bincode::config::Configuration<
    bincode::config::LittleEndian,
    bincode::config::Varint,
    bincode::config::NoLimit,
> = bincode::config::standard();

pub trait AsHeader {}

#[duplicate_item(
    read_header         async   reader_bounds       add_await(code) decode_message         ;
    [read_header]       []      [Read]              [code]          [decode_message]       ;
    [read_header_async] [async] [AsyncRead + Unpin] [code.await]    [decode_message_async] ;
)]
#[instrument(skip_all)]
pub async fn read_header<Reader, Header>(
    reader: &mut Reader,
    key: [u8; tokio_chacha20::KEY_BYTES],
    validator: &ValidatorRef<'_>,
) -> Result<Header, CodecError>
where
    Reader: reader_bounds,
    Header: std::fmt::Debug + AsHeader + bincode::Decode<()>,
{
    let mut buf = [0; MAX_HEADER_LEN * 2];
    let mut start_pos = 0;
    let mut end_pos = 0;
    let mut write_msg = ae::message::WriteBuf {
        buf: &mut buf,
        start_pos: &mut start_pos,
        end_pos: &mut end_pos,
    };

    match add_await([ae::message::decode_message(
        reader,
        &mut write_msg,
        key,
        Some(validator),
    )]) {
        Ok(()) => (),
        Err(e) => match e {
            ae::message::AeCodecError::Io(error) => return Err(CodecError::Io(error)),
            ae::message::AeCodecError::NotEnoughWriteBuf { required_len: _ } => {
                return Err(CodecError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Header too long",
                )));
            }
            ae::message::AeCodecError::Integrity => return Err(CodecError::Integrity),
        },
    };
    let hdr_buf = &buf[start_pos..end_pos];
    let mut rdr = io::Cursor::new(hdr_buf);
    let header = bincode::decode_from_std_read(&mut rdr, BINCODE_CONFIG)?;
    trace!(?header, "Read header");

    Ok(header)
}

#[duplicate_item(
    write_header         async   writer_bounds        add_await(code) encode_message         ;
    [write_header]       []      [Write]              [code]          [encode_message]       ;
    [write_header_async] [async] [AsyncWrite + Unpin] [code.await]    [encode_message_async] ;
)]
#[instrument(skip_all)]
pub async fn write_header<Writer, Header>(
    writer: &mut Writer,
    header: &Header,
    key: [u8; tokio_chacha20::KEY_BYTES],
) -> Result<(), CodecError>
where
    Writer: writer_bounds,
    Header: std::fmt::Debug + AsHeader + bincode::Encode,
{
    let mut hdr_buf = [0; MAX_HEADER_LEN];
    let mut hdr_wtr = io::Cursor::new(&mut hdr_buf[..]);
    bincode::encode_into_std_write(header, &mut hdr_wtr, BINCODE_CONFIG)?;
    let len = hdr_wtr.position();
    let hdr_buf = &hdr_buf[..len as usize];

    let timestamped = true;
    let mut msg_buf = [0; ae::timestamp::TIMESTAMP_SIZE + MAX_HEADER_LEN];
    let mut ciphertext_buf = [0; MAX_HEADER_LEN * 2];
    let write_message = |wtr: &mut io::Cursor<&mut [u8]>| {
        Write::write_all(wtr, hdr_buf).unwrap();
        Ok(())
    };
    match add_await([ae::message::encode_message(
        writer,
        key,
        timestamped,
        &mut msg_buf,
        &mut ciphertext_buf,
        write_message,
    )]) {
        Ok(()) => (),
        Err(e) => match e {
            ae::message::AeCodecError::Io(error) => return Err(CodecError::Io(error)),
            ae::message::AeCodecError::NotEnoughWriteBuf { required_len: _ } => panic!(),
            ae::message::AeCodecError::Integrity => return Err(CodecError::Integrity),
        },
    }

    Ok(())
}

pub async fn timed_read_header_async<Reader, Header>(
    reader: &mut Reader,
    key: [u8; tokio_chacha20::KEY_BYTES],
    validator: &ValidatorRef<'_>,
    timeout: Duration,
) -> Result<Header, CodecError>
where
    Reader: AsyncRead + Unpin,
    Header: std::fmt::Debug + AsHeader + bincode::Decode<()>,
{
    let res = tokio::time::timeout(timeout, read_header_async(reader, key, validator)).await;
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
    key: [u8; tokio_chacha20::KEY_BYTES],
    timeout: Duration,
) -> Result<(), CodecError>
where
    Writer: AsyncWrite + Unpin,
    Header: std::fmt::Debug + AsHeader + bincode::Encode,
{
    let res = tokio::time::timeout(timeout, write_header_async(writer, header, key)).await;
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
