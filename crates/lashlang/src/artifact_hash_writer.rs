use sha2::{Digest, Sha256};

use super::ContentHash;

#[derive(Default)]
pub(super) struct HashWriter {
    bytes: Vec<u8>,
}

impl HashWriter {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn atom(&mut self, value: &str) {
        self.bytes
            .extend_from_slice(value.len().to_string().as_bytes());
        self.bytes.push(b':');
        self.bytes.extend_from_slice(value.as_bytes());
        self.bytes.push(b';');
    }

    pub(super) fn bool(&mut self, value: bool) {
        self.atom(if value { "true" } else { "false" });
    }

    pub(super) fn usize(&mut self, value: usize) {
        self.atom(&value.to_string());
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.atom(&value.to_string());
    }

    pub(super) fn u64(&mut self, value: u64) {
        self.atom(&value.to_string());
    }

    pub(super) fn finish(self) -> ContentHash {
        ContentHash::new(hex_digest(&Sha256::digest(self.bytes)))
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
