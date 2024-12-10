use std::{
    io::{self, Write},
    sync::Arc,
};

use base64::prelude::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

type Key = Arc<[u8]>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct XorCryptoBuilder(pub String);
impl XorCryptoBuilder {
    pub fn build(&self) -> Result<XorCrypto, XorCryptoBuildError> {
        let key = BASE64_STANDARD_NO_PAD
            .decode(&self.0)
            .map_err(|e| XorCryptoBuildError {
                source: e,
                key: self.0.clone(),
            })?;
        Ok(XorCrypto::new(key.into()))
    }
}
#[derive(Debug, Error)]
#[error("{source}, key = `{key}`")]
pub struct XorCryptoBuildError {
    #[source]
    pub source: base64::DecodeError,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct XorCrypto {
    key: Key,
}
impl XorCrypto {
    pub fn new(key: Key) -> Self {
        Self { key }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct XorCryptoCursor {
    key: Key,
    pos: usize,
}
impl XorCryptoCursor {
    pub fn new(config: &XorCrypto) -> Self {
        Self {
            key: Arc::clone(&config.key),
            pos: 0,
        }
    }
}
impl XorCryptoCursor {
    pub fn xor(&mut self, buf: &mut [u8]) {
        if self.key.is_empty() {
            return;
        }
        buf.iter_mut().enumerate().for_each(|(i, b)| {
            let i = i + self.pos;
            let xor_b = *b ^ self.key[i % self.key.len()];
            *b = xor_b;
        });
        self.pos = (self.pos + buf.len()) % self.key.len();
    }

    pub fn xor_to<W>(&mut self, buf: &[u8], to: &mut W) -> io::Result<()>
    where
        W: Write,
    {
        if self.key.is_empty() {
            return Ok(());
        }
        for (i, b) in buf.iter().enumerate() {
            let i = i + self.pos;
            let xor_b = *b ^ self.key[i % self.key.len()];
            to.write_all(&[xor_b])?;
        }
        self.pos = (self.pos + buf.len()) % self.key.len();
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use rand::Rng;

    use super::*;

    pub fn create_random_crypto(len: usize) -> XorCrypto {
        let mut rng = rand::thread_rng();
        let mut key = Vec::new();
        for _ in 0..len {
            key.push(rng.gen());
        }
        XorCrypto::new(key.into())
    }
}
