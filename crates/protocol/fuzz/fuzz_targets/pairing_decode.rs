//! Fuzz postcard decoding of the pairing messages — the pre-authentication
//! attack surface (spec 06/13): the agent decodes `PairHello`/`PairConfirm`
//! from an *anonymous* peer before any trust is established. Must never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    use gsa_protocol::pairing::{PairConfirm, PairHello, PairResponse, PairResult};
    let _ = gsa_protocol::decode_msg::<PairHello>(data);
    let _ = gsa_protocol::decode_msg::<PairResponse>(data);
    let _ = gsa_protocol::decode_msg::<PairConfirm>(data);
    let _ = gsa_protocol::decode_msg::<PairResult>(data);
});
