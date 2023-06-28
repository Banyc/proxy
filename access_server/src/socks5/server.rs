use async_trait::async_trait;
use common::{
    loading::Hook,
    stream::{IoAddr, IoStream, StreamServerHook},
};

pub struct Socks5Server {}

impl Hook for Socks5Server {}

#[async_trait]
impl StreamServerHook for Socks5Server {
    async fn handle_stream<S>(&self, _stream: S)
    where
        S: IoStream + IoAddr + std::fmt::Debug,
    {
        todo!()
    }
}
