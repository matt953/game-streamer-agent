//! Fuzz the hot-path audio datagram parser: must never panic or over-read on
//! attacker-controlled bytes (spec 06/13). Audio datagrams ride the same
//! unauthenticated path as video.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((header, payload)) = gsa_protocol::AudioDatagramHeader::parse(data) {
        // Re-encode/re-parse must agree with the first parse.
        let wire = header.encode_with_payload(payload);
        let (h2, p2) = gsa_protocol::AudioDatagramHeader::parse(&wire).expect("round trip");
        assert_eq!(header, h2);
        assert_eq!(payload, p2);
    }
});
