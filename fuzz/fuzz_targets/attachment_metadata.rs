//! Fuzzes attachment metadata parsing: attachment identifiers and media
//! types, the two grammars every store trusts at its namespace boundary.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(id) = lash_sansio::AttachmentId::parse(text) {
            // A parsed id must round-trip unchanged; renormalization would
            // let two spellings alias one namespace entry.
            assert_eq!(id.as_str(), text);
        }
        let _ = lash_sansio::MediaType::parse(text);
    }
});
