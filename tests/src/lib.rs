use common::{crypto::XorCrypto, header::MAX_HEADER_LEN};
use rand::Rng;

pub mod tcp;
pub mod udp;

pub fn create_random_crypto() -> XorCrypto {
    let mut rng = rand::thread_rng();
    let mut key = Vec::new();
    for _ in 0..MAX_HEADER_LEN {
        key.push(rng.gen());
    }
    XorCrypto::new(key)
}
