use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    ops::Deref,
};

use common::addr::{InternetAddr, InternetAddrKind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod sub_negotiations;

const VERSION: u8 = 5;
const NO_ACCEPTABLE_METHODS: u8 = 0xff;

/// ```text
/// +--------+
/// | METHOD |
/// +--------+
/// |   1    |
/// +--------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodIdentifier {
    NoAuth,
    UsernamePassword,
    Other(u8),
}

impl From<MethodIdentifier> for u8 {
    fn from(value: MethodIdentifier) -> Self {
        match value {
            MethodIdentifier::NoAuth => 0x0,
            MethodIdentifier::UsernamePassword => 0x2,
            MethodIdentifier::Other(code) => code,
        }
    }
}

impl From<u8> for MethodIdentifier {
    fn from(code: u8) -> Self {
        match code {
            0x0 => MethodIdentifier::NoAuth,
            0x2 => MethodIdentifier::UsernamePassword,
            _ => MethodIdentifier::Other(code),
        }
    }
}

/// ```text
/// +-----+----------+----------+
/// | VER | NMETHODS | METHODS  |
/// +-----+----------+----------+
/// |  1  |    1     | 1 to 255 |
/// +-----+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiationRequest {
    pub methods: Vec<MethodIdentifier>,
}

impl NegotiationRequest {
    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let version = reader.read_u8().await?;
        map_version_error(version)?;
        let method_count = reader.read_u8().await?;
        let mut methods = Vec::with_capacity(method_count as usize);
        for _ in 0..method_count {
            methods.push(MethodIdentifier::from(reader.read_u8().await?));
        }
        Ok(Self { methods })
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        if self.methods.len() > u8::MAX as _ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Too many methods",
            ));
        }
        writer.write_u8(VERSION).await?;
        writer.write_u8(self.methods.len() as u8).await?;
        for method in &self.methods {
            writer.write_u8((*method).into()).await?;
        }
        Ok(())
    }
}

/// ```text
/// +-----+--------+
/// | VER | METHOD |
/// +-----+--------+
/// |  1  |   1    |
/// +-----+--------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiationResponse {
    pub method: Option<MethodIdentifier>,
}

impl NegotiationResponse {
    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let version = reader.read_u8().await?;
        map_version_error(version)?;
        let method = reader.read_u8().await?;
        match method {
            NO_ACCEPTABLE_METHODS => Ok(Self { method: None }),
            _ => Ok(Self {
                method: Some(MethodIdentifier::from(method)),
            }),
        }
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut buf = [0u8; 2];
        let mut internal_writer = io::Cursor::new(buf.as_mut_slice());
        internal_writer.write_u8(VERSION).await.unwrap();
        match self.method {
            Some(method) => internal_writer.write_u8(method.into()).await.unwrap(),
            None => internal_writer
                .write_u8(NO_ACCEPTABLE_METHODS)
                .await
                .unwrap(),
        }
        writer.write_all(&buf).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// ```text
/// +-----+
/// | CMD |
/// +-----+
/// |  1  |
/// +-----+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Connect,
    Bind,
    UdpAssociate,
}

impl From<Command> for u8 {
    fn from(value: Command) -> Self {
        match value {
            Command::Connect => 1,
            Command::Bind => 2,
            Command::UdpAssociate => 3,
        }
    }
}

impl TryFrom<u8> for Command {
    type Error = ();

    fn try_from(code: u8) -> Result<Self, ()> {
        match code {
            1 => Ok(Command::Connect),
            2 => Ok(Command::Bind),
            3 => Ok(Command::UdpAssociate),
            _ => Err(()),
        }
    }
}

/// ```text
/// +------+
/// | ATYP |
/// +------+
/// |  1   |
/// +------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressType {
    Ipv4,
    DomainName,
    Ipv6,
}

impl From<AddressType> for u8 {
    fn from(value: AddressType) -> Self {
        match value {
            AddressType::Ipv4 => 0x1,
            AddressType::DomainName => 0x3,
            AddressType::Ipv6 => 0x4,
        }
    }
}

impl TryFrom<u8> for AddressType {
    type Error = ();

    fn try_from(code: u8) -> Result<Self, ()> {
        match code {
            0x1 => Ok(AddressType::Ipv4),
            0x3 => Ok(AddressType::DomainName),
            0x4 => Ok(AddressType::Ipv6),
            _ => Err(()),
        }
    }
}

pub async fn decode_address<R>(reader: &mut R) -> io::Result<InternetAddr>
where
    R: AsyncRead + Unpin,
{
    let address_type = AddressType::try_from(reader.read_u8().await?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid address type"))?;
    let address = match address_type {
        AddressType::Ipv4 => {
            let mut buf = [0; 4];
            reader.read_exact(&mut buf).await?;
            let ip = Ipv4Addr::from(buf);
            let port = reader.read_u16().await?;
            let addr = SocketAddr::V4(SocketAddrV4::new(ip, port));
            InternetAddr::from(addr)
        }
        AddressType::Ipv6 => {
            let mut buf = [0; 16];
            reader.read_exact(&mut buf).await?;
            let ip = Ipv6Addr::from(buf);
            let port = reader.read_u16().await?;
            let addr = SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0));
            InternetAddr::from(addr)
        }
        AddressType::DomainName => {
            let len = reader.read_u8().await?;
            let mut buf = vec![0; len as usize];
            reader.read_exact(&mut buf).await?;
            let domain_name = String::from_utf8(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let port = reader.read_u16().await?;
            InternetAddr::from_host_and_port(domain_name, port)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
        }
    };
    Ok(address)
}

pub async fn encode_address<W>(addr: &InternetAddr, writer: &mut W) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match addr.deref() {
        InternetAddrKind::SocketAddr(addr) => match addr {
            SocketAddr::V4(addr) => {
                writer.write_u8(AddressType::Ipv4.into()).await?;
                writer.write_all(&addr.ip().octets()).await?;
                writer.write_u16(addr.port()).await?;
            }
            SocketAddr::V6(addr) => {
                writer.write_u8(AddressType::Ipv6.into()).await?;
                writer.write_all(&addr.ip().octets()).await?;
                writer.write_u16(addr.port()).await?;
            }
        },
        InternetAddrKind::DomainName { addr, port } => {
            writer.write_u8(AddressType::DomainName.into()).await?;
            let len = u8::try_from(addr.len())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            writer.write_u8(len).await?;
            writer.write_all(addr.as_bytes()).await?;
            writer.write_u16(*port).await?;
        }
    }
    Ok(())
}

/// ```text
/// +-----+-----+-------+------+----------+----------+
/// | VER | CMD |  RSV  | ATYP | DST.ADDR | DST.PORT |
/// +-----+-----+-------+------+----------+----------+
/// |  1  |  1  | 0x00  |  1   | Variable |    2     |
/// +-----+-----+-------+------+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRequest {
    pub command: Command,
    pub destination: InternetAddr,
}

impl RelayRequest {
    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let version = reader.read_u8().await?;
        map_version_error(version)?;
        let command = Command::try_from(reader.read_u8().await?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid command"))?;
        let _reserved = reader.read_u8().await?;
        let destination = decode_address(reader).await?;
        Ok(Self {
            command,
            destination,
        })
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        writer.write_u8(VERSION).await?;
        writer.write_u8(self.command.into()).await?;
        writer.write_u8(0x00).await?;
        encode_address(&self.destination, writer).await?;
        Ok(())
    }
}

/// ```text
/// +-----+
/// | REP |
/// +-----+
/// |  1  |
/// +-----+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Succeeded,
    GeneralSocksServerFailure,
    ConnectionNotAllowedByRuleset,
    CommandNotSupported,
    Other(u8),
}

impl From<Reply> for u8 {
    fn from(value: Reply) -> Self {
        match value {
            Reply::Succeeded => 0x0,
            Reply::GeneralSocksServerFailure => 0x1,
            Reply::ConnectionNotAllowedByRuleset => 0x2,
            Reply::CommandNotSupported => 0x7,
            Reply::Other(code) => code,
        }
    }
}

impl From<u8> for Reply {
    fn from(code: u8) -> Self {
        match code {
            0x0 => Reply::Succeeded,
            0x1 => Reply::GeneralSocksServerFailure,
            0x2 => Reply::ConnectionNotAllowedByRuleset,
            0x7 => Reply::CommandNotSupported,
            _ => Reply::Other(code),
        }
    }
}

/// ```text
/// +-----+-----+-------+------+----------+----------+
/// | VER | REP |  RSV  | ATYP | BND.ADDR | BND.PORT |
/// +-----+-----+-------+------+----------+----------+
/// |  1  |  1  | 0x00  |  1   | Variable |    2     |
/// +-----+-----+-------+------+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayResponse {
    pub reply: Reply,
    pub bind: InternetAddr,
}

impl RelayResponse {
    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let version = reader.read_u8().await?;
        map_version_error(version)?;
        let reply = Reply::from(reader.read_u8().await?);
        let _reserved = reader.read_u8().await?;
        let bind = decode_address(reader).await?;
        Ok(Self { reply, bind })
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let mut buf = [0u8; 1 + 1 + 1 + 1 + 1 + 0xff + 2];
        let mut internal_writer = io::Cursor::new(buf.as_mut_slice());
        internal_writer.write_u8(VERSION).await.unwrap();
        internal_writer.write_u8(self.reply.into()).await.unwrap();
        internal_writer.write_u8(0x00).await.unwrap();
        encode_address(&self.bind, &mut internal_writer).await?;
        let pos = internal_writer.position() as usize;
        writer.write_all(&buf[..pos]).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// ```text
/// +-----+------+------+----------+----------+----------+
/// | RSV | FRAG | ATYP | DST.ADDR | DST.PORT |   DATA   |
/// +-----+------+------+----------+----------+----------+
/// |  2  |  1   |  1   | Variable |    2     | Variable |
/// +-----+------+------+----------+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpRequestHeader {
    pub fragment: u8,
    pub destination: InternetAddr,
}

impl UdpRequestHeader {
    pub async fn decode<R>(reader: &mut R) -> io::Result<Self>
    where
        R: AsyncRead + Unpin,
    {
        let _reserved = reader.read_u16().await?;
        let fragment = reader.read_u8().await?;
        let destination = decode_address(reader).await?;
        Ok(Self {
            fragment,
            destination,
        })
    }

    pub async fn encode<W>(&self, writer: &mut W) -> io::Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        writer.write_u16(0x0000).await?;
        writer.write_u8(self.fragment).await?;
        encode_address(&self.destination, writer).await?;
        Ok(())
    }
}

fn map_version_error(version: u8) -> io::Result<()> {
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Invalid version: {}", version),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn negotiation_request() {
        let expected = NegotiationRequest {
            methods: vec![MethodIdentifier::NoAuth, MethodIdentifier::Other(0x1)],
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[VERSION, 0x2, MethodIdentifier::NoAuth.into(), 0x1]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = NegotiationRequest::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn negotiation_response_some() {
        let expected = NegotiationResponse {
            method: Some(MethodIdentifier::NoAuth),
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(wtr.get_ref(), &[VERSION, MethodIdentifier::NoAuth.into()]);
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = NegotiationResponse::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn negotiation_response_none() {
        let expected = NegotiationResponse { method: None };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(wtr.get_ref(), &[VERSION, NO_ACCEPTABLE_METHODS]);
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = NegotiationResponse::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn address_ipv4() {
        let expected = InternetAddr::from("1.2.3.4:5".parse::<SocketAddr>().unwrap());
        let mut wtr = io::Cursor::new(Vec::new());
        encode_address(&expected, &mut wtr).await.unwrap();
        assert_eq!(wtr.get_ref(), &[AddressType::Ipv4.into(), 1, 2, 3, 4, 0, 5]);
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = decode_address(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn address_ipv6() {
        let expected = InternetAddr::from("[::1.2.3.4]:5".parse::<SocketAddr>().unwrap());
        let mut wtr = io::Cursor::new(Vec::new());
        encode_address(&expected, &mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[
                AddressType::Ipv6.into(),
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                1,
                2,
                3,
                4,
                0,
                5
            ]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = decode_address(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn address_domain_name() {
        let expected = "a:5".parse::<InternetAddr>().unwrap();
        let mut wtr = io::Cursor::new(Vec::new());
        encode_address(&expected, &mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[AddressType::DomainName.into(), 0x1, b'a', 0, 5]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = decode_address(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn relay_request() {
        let expected = RelayRequest {
            command: Command::Connect,
            destination: "1.2.3.4:5".parse().unwrap(),
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[
                VERSION,
                Command::Connect.into(),
                0x0,
                AddressType::Ipv4.into(),
                1,
                2,
                3,
                4,
                0,
                5
            ]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = RelayRequest::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn relay_response() {
        let expected = RelayResponse {
            reply: Reply::GeneralSocksServerFailure,
            bind: "1.2.3.4:5".parse().unwrap(),
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[
                VERSION,
                Reply::GeneralSocksServerFailure.into(),
                0x0,
                AddressType::Ipv4.into(),
                1,
                2,
                3,
                4,
                0,
                5
            ]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = RelayResponse::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn udp_request_header() {
        let expected = UdpRequestHeader {
            fragment: 1,
            destination: "1.2.3.4:5".parse().unwrap(),
        };
        let mut wtr = io::Cursor::new(Vec::new());
        expected.encode(&mut wtr).await.unwrap();
        assert_eq!(
            wtr.get_ref(),
            &[0x0, 0x0, 0x1, AddressType::Ipv4.into(), 1, 2, 3, 4, 0, 5]
        );
        let mut rdr = io::Cursor::new(wtr.into_inner());
        let actual = UdpRequestHeader::decode(&mut rdr).await.unwrap();
        assert_eq!(expected, actual);
    }
}
