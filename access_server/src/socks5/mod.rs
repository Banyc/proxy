use common::addr::InternetAddr;

pub mod server;

pub enum MethodIdentifier {
    NoAuth,
}

pub struct NegotiationRequest {
    pub methods: Vec<MethodIdentifier>,
}

pub struct NegotiationResponse {
    pub method: Option<MethodIdentifier>,
}

pub enum Command {
    Connect,
    Bind,
    UdpAssociate,
}

pub struct RelayRequest {
    pub command: Command,
    pub addr: InternetAddr,
}

pub enum Reply {
    Succeeded,
    GeneralSocksServerFailure,
}

pub struct RelayResponse {
    pub reply: Reply,
    pub addr: InternetAddr,
}

pub struct UdpRequestHeader {
    pub fragment: u8,
    pub addr: InternetAddr,
}
