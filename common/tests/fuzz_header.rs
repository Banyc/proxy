use ae::anti_replay::{TimeValidator, ValidatorRef};
use common::header::codec::{MAX_HEADER_LEN, read_header_async};
use common::proto::header::StreamRequestHeader;
use std::io::Cursor;
use std::time::Duration;

mod support;
use support::SplitMix64;

const ROUNDS: usize = 20_000;
fn key() -> [u8; tokio_chacha20::KEY_BYTES] {
    [0x42; tokio_chacha20::KEY_BYTES]
}
fn validator() -> TimeValidator {
    TimeValidator::new(Duration::from_secs(60))
}
#[tokio::test]
async fn framing_chosen_by_someone_without_the_key() {
    let mut rng = SplitMix64::new(0x5eed);
    let time_validator = validator();
    let validator = ValidatorRef::Time(&time_validator);
    for _ in 0..ROUNDS {
        let len = match rng.below(4) {
            0 => rng.below(8),
            1 => rng.below(64),
            2 => MAX_HEADER_LEN - 4 + rng.below(8),
            _ => rng.below(MAX_HEADER_LEN * 2 + 16),
        };
        let bytes = rng.bytes(len);
        let mut rdr = Cursor::new(&bytes[..]);
        let res = read_header_async::<_, StreamRequestHeader>(&mut rdr, key(), &validator).await;
        assert!(res.is_err(), "{bytes:02x?} decoded to {res:?}");
    }
}
#[tokio::test]
async fn a_payload_chosen_by_a_client_that_has_the_key() {
    let mut rng = SplitMix64::new(0x5eed);
    let time_validator = validator();
    let validator = ValidatorRef::Time(&time_validator);
    let mut decoded_count = 0_usize;
    for _ in 0..ROUNDS {
        let payload_len = rng.below(MAX_HEADER_LEN);
        let payload = rng.bytes(payload_len);
        let mut wire = Cursor::new(Vec::new());
        let mut scratch = [0; MAX_HEADER_LEN * 2];
        let written = ae::message::encode_message(
            &mut wire,
            key(),
            true,
            &mut scratch,
            |wtr: &mut Cursor<&mut [u8]>| Ok(std::io::Write::write_all(wtr, &payload)?),
        );
        if written.is_err() {
            continue;
        }
        let wire = wire.into_inner();
        let mut rdr = Cursor::new(&wire[..]);
        if read_header_async::<_, StreamRequestHeader>(&mut rdr, key(), &validator)
            .await
            .is_ok()
        {
            decoded_count += 1;
        }
    }
    assert!(
        decoded_count > 0,
        "no payload ever reached the deserializer, so nothing was exercised"
    );
}
