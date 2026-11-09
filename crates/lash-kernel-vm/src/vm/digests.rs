//! Maps keyed by callable identities, hashed by their digests.

use std::collections::HashMap;

use lash_kernel_value::names::CallableId;

/// A map keyed by callable identities: a Link's and a function's digest.
pub(super) type Digests<V> = HashMap<CallableId, V, std::hash::BuildHasherDefault<DigestHasher>>;

/// Hashes a callable identity by its digests' first words: a SHA-256
/// digest is already uniform, so no further mixing is needed.
#[derive(Default)]
pub(super) struct DigestHasher(u64);

impl std::hash::Hasher for DigestHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut word = [0; 8];
        let n = bytes.len().min(8);
        word[..n].copy_from_slice(&bytes[..n]);
        self.0 ^= u64::from_le_bytes(word);
    }

    #[inline]
    fn write_usize(&mut self, _: usize) {}

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}
