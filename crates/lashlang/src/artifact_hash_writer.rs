use lash_sansio::core_support::Blake3DomainHasher;

use super::ContentHash;

/// Longest decimal rendering of a `u64`.
const DECIMAL_CAPACITY: usize = 20;

fn decimal(value: u64, buffer: &mut [u8; DECIMAL_CAPACITY]) -> &[u8] {
    let mut index = DECIMAL_CAPACITY;
    let mut remaining = value;
    loop {
        index -= 1;
        buffer[index] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    &buffer[index..]
}

/// The atoms stream straight into the hasher. Collecting them into a `Vec<u8>`
/// first, and rendering every length prefix and integer through `to_string()`,
/// cost hundreds of allocations and two copies of the program's hash input per
/// hash - which is what made publish-time verification expensive enough to blow
/// the `artifact_roundtrip` budgets (FIG-3088). The byte stream is unchanged, so
/// no content hash moves.
pub(super) struct HashWriter {
    hasher: Blake3DomainHasher,
}

impl HashWriter {
    pub(super) fn new() -> Self {
        Self {
            // The domain is written as a literal here: the workspace's
            // `blake3_domains_are_unique_and_match_workspace_usage` gate scans
            // constructor call sites for the string, and a named constant is
            // invisible to it.
            hasher: Blake3DomainHasher::new("lash-lashlang-content/v2"),
        }
    }

    pub(super) fn atom(&mut self, value: &str) {
        self.integer(value.len() as u64);
        self.hasher.update(b":");
        self.hasher.update(value.as_bytes());
        self.hasher.update(b";");
    }

    /// One atom whose content is `prefix` followed by `value`.
    pub(super) fn prefixed_atom(&mut self, prefix: &str, value: &str) {
        self.integer((prefix.len() + value.len()) as u64);
        self.hasher.update(b":");
        self.hasher.update(prefix.as_bytes());
        self.hasher.update(value.as_bytes());
        self.hasher.update(b";");
    }

    pub(super) fn bool(&mut self, value: bool) {
        self.atom(if value { "true" } else { "false" });
    }

    pub(super) fn usize(&mut self, value: usize) {
        self.u64(value as u64);
    }

    pub(super) fn u32(&mut self, value: u32) {
        self.u64(u64::from(value));
    }

    pub(super) fn u64(&mut self, value: u64) {
        // `atom` of the decimal rendering, written without materialising it:
        // the length prefix is the digit count.
        let mut buffer = [0u8; DECIMAL_CAPACITY];
        let digits = decimal(value, &mut buffer);
        self.integer(digits.len() as u64);
        self.hasher.update(b":");
        self.hasher.update(digits);
        self.hasher.update(b";");
    }

    fn integer(&mut self, value: u64) {
        let mut buffer = [0u8; DECIMAL_CAPACITY];
        self.hasher.update(decimal(value, &mut buffer));
    }

    pub(super) fn finish(self) -> ContentHash {
        ContentHash::new(self.hasher.finalize_hex())
    }
}
