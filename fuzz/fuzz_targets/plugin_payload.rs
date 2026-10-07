//! Fuzzes plugin payload parsing: tool grants, including the grant
//! validation and call-path derivation that run before a payload is trusted.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(grants) = serde_json::from_slice::<Vec<lash_remote_protocol::RemoteToolGrant>>(data) {
        let _ = lash_remote_protocol::RemoteToolGrant::validate_all(&grants);
        for grant in &grants {
            let _ = grant.call_path_bindings();
        }
    }
});
