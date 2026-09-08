//! Fuzzes the remote-protocol wire DTO decoders: the envelope version probe
//! and the turn request/input bodies that arrive from an untrusted peer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = lash_remote_protocol::RemoteTurnRequest::decode_json(data);
    let _ = lash_remote_protocol::RemoteTurnInput::decode_json(data);
    let _ = lash_remote_protocol::RemoteTurnReport::decode_json(data);
});
