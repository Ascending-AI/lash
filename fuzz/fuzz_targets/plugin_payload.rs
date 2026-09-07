//! Fuzzes tool/plugin payload parsing: plugin-emitted messages and tool
//! grants, including the grant validation and call-path derivation that run
//! before a payload is trusted.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(message) = serde_json::from_slice::<lash_remote_protocol::RemotePluginMessage>(data) {
        let _ = serde_json::to_vec(&message);
    }
    if let Ok(grants) = serde_json::from_slice::<Vec<lash_remote_protocol::RemoteToolGrant>>(data) {
        let _ = lash_remote_protocol::RemoteToolGrant::validate_all(&grants);
        for grant in &grants {
            let _ = grant.call_path_bindings();
        }
    }
});
