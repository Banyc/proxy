use async_trait::async_trait;
use common::{
    loading::Hook,
    stream::{IoAddr, IoStream, StreamServerHook},
};

pub struct Socks5ServerTcpAccess {}

impl Hook for Socks5ServerTcpAccess {}

#[async_trait]
impl StreamServerHook for Socks5ServerTcpAccess {
    async fn handle_stream<S>(&self, _stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        todo!()
    }
}
