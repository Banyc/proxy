use serde::{Deserialize, Serialize};

use crate::{
    addr::InternetAddr,
    crypto::XorCrypto,
    header::{ProxyConfig, RequestHeader},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdpProxyConfigBuilder {
    pub address: String,
    pub xor_key: Vec<u8>,
}

impl UdpProxyConfigBuilder {
    pub fn build(self) -> UdpProxyConfig {
        ProxyConfig {
            address: self.address.into(),
            crypto: XorCrypto::new(self.xor_key),
        }
    }
}

pub type UdpProxyConfig = ProxyConfig<InternetAddr>;
pub type UdpRequestHeader = RequestHeader<InternetAddr>;

#[cfg(test)]
mod tests {
    use std::{io, net::SocketAddr};

    use tracing::trace;

    use crate::{
        crypto::{tests::create_random_crypto, XorCryptoCursor},
        header::{read_header_async, write_header_async, MAX_HEADER_LEN},
    };

    use super::*;

    #[tokio::test]
    async fn test_request_header() {
        let mut buf = [0; 4 + MAX_HEADER_LEN];
        let mut stream = io::Cursor::new(&mut buf[..]);
        let crypto = create_random_crypto(MAX_HEADER_LEN);

        // Encode header
        let original_header: UdpRequestHeader = RequestHeader {
            upstream: "1.1.1.1:8080".parse::<SocketAddr>().unwrap().into(),
        };
        let mut crypto_cursor = XorCryptoCursor::new(&crypto);
        write_header_async(&mut stream, &original_header, &mut crypto_cursor)
            .await
            .unwrap();
        let len = stream.position();
        let buf = &buf[..len as usize];
        trace!(?original_header, ?len, "Encoded header");

        // Decode header
        let mut stream = io::Cursor::new(buf);
        let mut crypto_cursor = XorCryptoCursor::new(&crypto);
        let decoded_header = read_header_async(&mut stream, &mut crypto_cursor)
            .await
            .unwrap();
        assert_eq!(original_header, decoded_header);
    }
}
