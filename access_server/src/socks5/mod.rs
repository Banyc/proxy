use common::addr::InternetAddr;

pub mod server;

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
    Other(u8),
}

impl From<MethodIdentifier> for u8 {
    fn from(value: MethodIdentifier) -> Self {
        match value {
            MethodIdentifier::NoAuth => 0x0,
            MethodIdentifier::Other(code) => code,
        }
    }
}

impl From<u8> for MethodIdentifier {
    fn from(code: u8) -> Self {
        match code {
            0x0 => MethodIdentifier::NoAuth,
            _ => MethodIdentifier::Other(code),
        }
    }
}

/// ```text
/// +----+----------+----------+
/// |VER | NMETHODS | METHODS  |
/// +----+----------+----------+
/// | 1  |    1     | 1 to 255 |
/// +----+----------+----------+
/// ```
#[derive(Debug, Clone)]
pub struct NegotiationRequest {
    pub methods: Vec<MethodIdentifier>,
}

/// ```text
/// +----+--------+
/// |VER | METHOD |
/// +----+--------+
/// | 1  |   1    |
/// +----+--------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiationResponse {
    pub method: Option<MethodIdentifier>,
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
/// +----+-----+-------+------+----------+----------+
/// |VER | CMD |  RSV  | ATYP | DST.ADDR | DST.PORT |
/// +----+-----+-------+------+----------+----------+
/// | 1  |  1  | 0x00  |  1   | Variable |    2     |
/// +----+-----+-------+------+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayRequest {
    pub command: Command,
    pub addr: InternetAddr,
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
    Other(u8),
}

impl From<Reply> for u8 {
    fn from(value: Reply) -> Self {
        match value {
            Reply::Succeeded => 0x0,
            Reply::GeneralSocksServerFailure => 0x1,
            Reply::Other(code) => code,
        }
    }
}

impl From<u8> for Reply {
    fn from(code: u8) -> Self {
        match code {
            0x0 => Reply::Succeeded,
            0x1 => Reply::GeneralSocksServerFailure,
            _ => Reply::Other(code),
        }
    }
}

/// ```text
/// +----+-----+-------+------+----------+----------+
/// |VER | REP |  RSV  | ATYP | BND.ADDR | BND.PORT |
/// +----+-----+-------+------+----------+----------+
/// | 1  |  1  | 0x00  |  1   | Variable |    2     |
/// +----+-----+-------+------+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayResponse {
    pub reply: Reply,
    pub addr: InternetAddr,
}

/// ```text
/// +----+------+------+----------+----------+----------+
/// |RSV | FRAG | ATYP | DST.ADDR | DST.PORT |   DATA   |
/// +----+------+------+----------+----------+----------+
/// | 2  |  1   |  1   | Variable |    2     | Variable |
/// +----+------+------+----------+----------+----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdpRequestHeader {
    pub fragment: u8,
    pub addr: InternetAddr,
}
