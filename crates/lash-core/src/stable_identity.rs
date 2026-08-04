//! Byte framing for durable identities.
//!
//! Durable identity projections must be reductive, family-owned allowlists.
//! They may call only the primitives in this module: live domain types never
//! implement a shared encoding trait, and serde is deliberately absent from
//! this dependency path.
//!
//! Every family version selects its complete preimage grammar. Changing a
//! projection or fixing its framing requires a new family version, new golden
//! vectors, and an explicit policy for rows written by the old version. The
//! global salt is the scheme-wide escape hatch for repudiating every family
//! after a framing defect; it is not a substitute for family versioning.

const MAGIC: &[u8] = b"lash-stable-identity";

/// Scheme-wide salt folded into every framed family header.
pub(crate) const GLOBAL_SALT: u8 = 1;

/// Append-only builder for one family-owned durable identity preimage.
pub(crate) struct IdentityEncoder {
    bytes: Vec<u8>,
}

impl IdentityEncoder {
    /// Starts a preimage with `magic || salt || family-version || domain`.
    pub(crate) fn new(domain: &str, family_version: u8) -> Self {
        let mut encoder = Self { bytes: Vec::new() };
        encoder.raw_bytes(MAGIC);
        encoder.u8(GLOBAL_SALT);
        encoder.u8(family_version);
        encoder.bytes(domain.as_bytes());
        encoder
    }

    /// Emits a reserved integer tag. Tags are permanent and must never be
    /// reused after a variant or field is retired.
    pub(crate) fn tag(&mut self, tag: u8) {
        self.u8(tag);
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.raw_bytes(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.raw_bytes(&value.to_be_bytes());
    }

    pub(crate) fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.raw_bytes(value);
    }

    /// Emits the universal optional-value presence tags: 0 = absent, 1 =
    /// present. Family projections append the present value immediately.
    pub(crate) fn optional<T>(&mut self, value: Option<T>, present: impl FnOnce(&mut Self, T)) {
        match value {
            None => self.tag(0),
            Some(value) => {
                self.tag(1);
                present(self, value);
            }
        }
    }

    /// Emits an ordered sequence as a fixed-width element count followed by
    /// the family projection of each element in source order.
    pub(crate) fn sequence<T>(
        &mut self,
        values: impl IntoIterator<Item = T>,
        len: usize,
        mut element: impl FnMut(&mut Self, T),
    ) {
        self.u64(len as u64);
        for value in values {
            element(self, value);
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn raw_bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }
}

pub(crate) fn rendered_hash(prefix: &str, family_version: u8, preimage: &[u8]) -> String {
    format!(
        "{prefix}:v{family_version}:sha256:{}",
        crate::stable_hash::sha256_hex(preimage)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn framing_primitives_have_one_unambiguous_golden_grammar() {
        let mut encoder = IdentityEncoder::new("test", 7);
        encoder.tag(3);
        encoder.u32(0x0102_0304);
        encoder.u64(0x0102_0304_0506_0708);
        encoder.string("a:b");
        encoder.bytes(&[0, 1]);
        encoder.optional::<u8>(None, |_, _| unreachable!());
        encoder.optional(Some(9_u8), IdentityEncoder::u8);
        encoder.sequence([4_u8, 5], 2, IdentityEncoder::u8);

        assert_eq!(
            hex(&encoder.finish()),
            "6c6173682d737461626c652d6964656e746974790107000000000000000474657374030102030401020304050607080000000000000003613a620000000000000002000100010900000000000000020405"
        );
    }
}
