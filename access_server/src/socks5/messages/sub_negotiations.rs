use std::{io, num::NonZeroU8};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const USERNAME_PASSWORD_VERSION: u8 = 0x1;

/// ```text
/// +-----+------+----------+------+----------+
/// | VER | ULEN |  UNAME   | PLEN |  PASSWD  |
/// +-----+------+----------+------+----------+
/// |  1  |  1   | 1 to 255 |  1   | 1 to 255 |
/// +-----+------+----------+------+----------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsernamePasswordRequest {
    username_buf: [u8; u8::MAX as _],
    username_len: u8,
    password_buf: [u8; u8::MAX as _],
    password_len: u8,
}

impl UsernamePasswordRequest {
    pub fn new(username: &[u8], password: &[u8]) -> io::Result<Self> {
        let username_len: u8 = username.len().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "username length is too big")
        })?;
        let mut username_buf = [0u8; u8::MAX as _];
        username_buf[..username_len as usize].copy_from_slice(username);
        let password_len: u8 = password.len().try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "password length is too big")
        })?;
        let mut password_buf = [0u8; u8::MAX as _];
        password_buf[..password_len as usize].copy_from_slice(password);
        Ok(Self {
            username_buf,
            username_len,
            password_buf,
            password_len,
        })
    }

    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let version = reader.read_u8().await?;
        if version != USERNAME_PASSWORD_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Unsupported version: {}", version),
            ));
        }
        let mut username_buf = [0u8; u8::MAX as _];
        let username_len = reader.read_u8().await?;
        reader
            .read_exact(&mut username_buf[..username_len as _])
            .await?;
        let mut password_buf = [0u8; u8::MAX as _];
        let password_len = reader.read_u8().await?;
        reader
            .read_exact(&mut password_buf[..password_len as _])
            .await?;
        Ok(Self {
            username_buf,
            username_len,
            password_buf,
            password_len,
        })
    }

    pub fn username(&self) -> &[u8] {
        &self.username_buf[..self.username_len as _]
    }

    pub fn password(&self) -> &[u8] {
        &self.password_buf[..self.password_len as _]
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        writer.write_u8(USERNAME_PASSWORD_VERSION).await?;
        writer.write_u8(self.username_len).await?;
        writer.write_all(self.username()).await?;
        writer.write_u8(self.password_len).await?;
        writer.write_all(self.password()).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// ```text
/// +-----+--------+
/// | VER | STATUS |
/// +-----+--------+
/// |  1  |   1    |
/// +-----+--------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsernamePasswordResponse {
    pub status: UsernamePasswordStatus,
}

impl UsernamePasswordResponse {
    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let version = reader.read_u8().await?;
        if version != USERNAME_PASSWORD_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("Unsupported version: {}", version),
            ));
        }
        let status = reader.read_u8().await?;
        Ok(Self {
            status: UsernamePasswordStatus::from(status),
        })
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut buf = [0u8; 2];
        let mut internal_writer = io::Cursor::new(buf.as_mut_slice());
        internal_writer
            .write_u8(USERNAME_PASSWORD_VERSION)
            .await
            .unwrap();
        internal_writer.write_u8(self.status.into()).await.unwrap();
        writer.write_all(&buf).await?;
        writer.flush().await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsernamePasswordStatus {
    Success,
    Failure(NonZeroU8),
}

impl From<u8> for UsernamePasswordStatus {
    fn from(status: u8) -> Self {
        match status {
            0x0 => UsernamePasswordStatus::Success,
            _ => UsernamePasswordStatus::Failure(NonZeroU8::new(status).unwrap()),
        }
    }
}

impl From<UsernamePasswordStatus> for u8 {
    fn from(status: UsernamePasswordStatus) -> Self {
        match status {
            UsernamePasswordStatus::Success => 0x0,
            UsernamePasswordStatus::Failure(status) => status.get(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn username_password_request() {
        let expected = UsernamePasswordRequest::new(b"foo", b"bar").unwrap();
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[
                USERNAME_PASSWORD_VERSION,
                3,
                b'f',
                b'o',
                b'o',
                3,
                b'b',
                b'a',
                b'r'
            ]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = UsernamePasswordRequest::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn username_password_response_success() {
        let expected = UsernamePasswordResponse {
            status: UsernamePasswordStatus::Success,
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[
                USERNAME_PASSWORD_VERSION,
                UsernamePasswordStatus::Success.into(),
            ]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = UsernamePasswordResponse::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn username_password_response_failure() {
        let expected = UsernamePasswordResponse {
            status: UsernamePasswordStatus::Failure(NonZeroU8::new(1).unwrap()),
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[
                USERNAME_PASSWORD_VERSION,
                UsernamePasswordStatus::Failure(NonZeroU8::new(1).unwrap()).into(),
            ]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = UsernamePasswordResponse::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }
}
