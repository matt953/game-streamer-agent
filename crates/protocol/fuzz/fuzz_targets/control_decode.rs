//! Fuzz postcard decoding of both control-message directions: must never
//! panic on attacker-controlled bytes (spec 06/13).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = gsa_protocol::decode_msg::<gsa_protocol::C2A>(data);
    let _ = gsa_protocol::decode_msg::<gsa_protocol::A2C>(data);
});
