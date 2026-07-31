use protocol::socks5::messages::{
    NegotiationRequest, NegotiationResponse, RelayRequest, RelayResponse, UdpRequestHeader,
    decode_address, encode_address,
    sub_negotiations::{UsernamePasswordRequest, UsernamePasswordResponse},
};
use std::io::Cursor;

const ROUNDS: usize = 200_000;
const VERSION: u8 = 5;
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
}
const INTERESTING: &[u8] = &[0, 1, 2, 3, 4, 5, 0x7f, 0x80, 0xfe, 0xff];
fn address(rng: &mut Rng) -> Vec<u8> {
    let mut out = vec![];
    match rng.below(3) {
        0 => {
            out.push(0x1);
            out.extend(rng.bytes(4));
        }
        1 => {
            out.push(0x4);
            out.extend(rng.bytes(16));
        }
        _ => {
            out.push(0x3);
            let len = rng.below(256);
            out.push(len as u8);
            out.extend((0..len).map(|_| {
                if rng.next().is_multiple_of(4) {
                    rng.byte()
                } else {
                    b"abcdefghijklmnopqrstuvwxyz0123456789.-:[]"[rng.below(41)]
                }
            }));
        }
    }
    out.extend(rng.bytes(2));
    out
}
fn mutate(rng: &mut Rng, mut bytes: Vec<u8>) -> Vec<u8> {
    for _ in 0..rng.below(3) {
        if bytes.is_empty() {
            break;
        }
        let at = rng.below(bytes.len());
        bytes[at] = if rng.next().is_multiple_of(2) {
            INTERESTING[rng.below(INTERESTING.len())]
        } else {
            rng.byte()
        };
    }
    match rng.below(8) {
        0 if !bytes.is_empty() => bytes.truncate(rng.below(bytes.len())),
        1 => {
            let n = rng.below(16);
            bytes.extend(rng.bytes(n));
        }
        _ => {}
    }
    bytes
}
#[tokio::test]
async fn address_round_trip() {
    let mut rng = Rng(0x5eed);
    let mut decoded_count = 0_usize;
    for _ in 0..ROUNDS {
        let seed = address(&mut rng);
        let bytes = mutate(&mut rng, seed);
        let mut rdr = Cursor::new(&bytes[..]);
        let Ok(decoded) = decode_address(&mut rdr).await else {
            continue;
        };
        decoded_count += 1;
        let mut wtr = Cursor::new(Vec::new());
        encode_address(&decoded, &mut wtr)
            .await
            .unwrap_or_else(|e| {
                panic!("{decoded:?} decoded from {bytes:02x?} but does not encode: {e}")
            });
        let re = wtr.into_inner();
        let mut rdr = Cursor::new(&re[..]);
        let again = decode_address(&mut rdr).await.unwrap_or_else(|e| {
            panic!("{decoded:?} re-encoded to {re:02x?}, which does not decode: {e}")
        });
        assert_eq!(decoded, again, "input {bytes:02x?}, re-encoded {re:02x?}");
    }
    assert!(
        decoded_count * 4 > ROUNDS,
        "only {decoded_count}/{ROUNDS} inputs reached the encoder"
    );
}
macro_rules! round_trip {
    ($name:ident, $ty:ty, $seed:expr) => {
        #[tokio::test]
        async fn $name() {
            let mut rng = Rng(0x5eed);
            let mut decoded_count = 0_usize;
            for _ in 0..ROUNDS {
                let seed: Vec<u8> = $seed(&mut rng);
                let bytes = mutate(&mut rng, seed);
                let mut rdr = Cursor::new(&bytes[..]);
                let Ok(decoded) = <$ty>::decode(&mut rdr).await else {
                    continue;
                };
                decoded_count += 1;
                let mut wtr = Cursor::new(Vec::new());
                decoded.encode(&mut wtr).await.unwrap_or_else(|e| {
                    panic!("{decoded:?} decoded from {bytes:02x?} but does not encode: {e}")
                });
                let re = wtr.into_inner();
                let mut rdr = Cursor::new(&re[..]);
                let again = <$ty>::decode(&mut rdr).await.unwrap_or_else(|e| {
                    panic!("{decoded:?} re-encoded to {re:02x?}, which does not decode: {e}")
                });
                assert_eq!(decoded, again, "input {bytes:02x?}, re-encoded {re:02x?}");
            }
            assert!(decoded_count * 4 > ROUNDS, "only {decoded_count}/{ROUNDS} inputs reached the encoder; the generator stopped producing anything this parser accepts",);
        }
    };
}
round_trip!(negotiation_request, NegotiationRequest, |rng: &mut Rng| {
    let count = rng.below(256);
    let mut out = vec![VERSION, count as u8];
    out.extend(rng.bytes(count));
    out
});
round_trip!(
    negotiation_response,
    NegotiationResponse,
    |rng: &mut Rng| { vec![VERSION, rng.byte()] }
);
round_trip!(relay_request, RelayRequest, |rng: &mut Rng| {
    let mut out = vec![VERSION, 1 + rng.below(3) as u8, 0];
    out.extend(address(rng));
    out
});
round_trip!(relay_response, RelayResponse, |rng: &mut Rng| {
    let mut out = vec![VERSION, rng.byte(), 0];
    out.extend(address(rng));
    out
});
round_trip!(udp_request_header, UdpRequestHeader, |rng: &mut Rng| {
    let mut out = vec![0, 0, rng.byte()];
    out.extend(address(rng));
    out
});
round_trip!(
    username_password_request,
    UsernamePasswordRequest,
    |rng: &mut Rng| {
        let mut out = vec![0x1];
        for _ in 0..2 {
            let len = rng.below(256);
            out.push(len as u8);
            out.extend(rng.bytes(len));
        }
        out
    }
);
round_trip!(
    username_password_response,
    UsernamePasswordResponse,
    |rng: &mut Rng| vec![0x1, rng.byte()]
);
