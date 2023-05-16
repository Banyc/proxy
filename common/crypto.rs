use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub struct XorCrypto {
    key: Vec<u8>,
}

impl XorCrypto {
    pub fn new(key: Vec<u8>) -> Self {
        Self { key }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct XorCryptoCursor<'key> {
    key: &'key [u8],
    pos: usize,
}

impl<'config> XorCryptoCursor<'config> {
    pub fn new(config: &'config XorCrypto) -> Self {
        Self {
            key: &config.key,
            pos: 0,
        }
    }
}

impl XorCryptoCursor<'_> {
    pub fn xor(&mut self, buf: &mut [u8]) {
        if self.key.is_empty() {
            return;
        }
        for (i, b) in buf.iter_mut().enumerate() {
            let i = i + self.pos;
            *b ^= self.key[i % self.key.len()];
        }
        self.pos = (self.pos + buf.len()) % self.key.len()
    }
}
