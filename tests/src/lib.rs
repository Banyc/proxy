use common::{crypto::XorCrypto, header::codec::MAX_HEADER_LEN};
use rand::Rng;

pub mod stream;
pub mod udp;

pub const STRESS_PARALLEL: usize = 2;
pub const STRESS_SERIAL: usize = 2;
pub const STRESS_CHAINS: usize = 2;

pub fn create_random_crypto() -> XorCrypto {
    let mut rng = rand::rng();
    let mut key = Vec::new();
    for _ in 0..MAX_HEADER_LEN {
        key.push(rng.random());
    }
    XorCrypto::new(key.into())
}
