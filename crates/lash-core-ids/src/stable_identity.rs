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
pub(crate) const GLOBAL_SALT: u8 = 2;

/// Reserved durable identity family domains. Entries are append-only: retired
/// families remain reserved so a later projection cannot silently reuse them.
pub(crate) const FAMILY_DOMAINS: &[&str] = &[
    "lash.aggregate-content",
    "lash.aggregate-with-timers",
    "lash.append-request",
    "lash.await-event",
    "lash.await-event-auth",
    "lash.direct-effect-discriminator",
    "lash.direct-effect-replay-key",
    "lash.history-node",
    "lash.intent",
    "lash.process-cancellation-request",
    "lash.process-definition-reference",
    "lash.process-registration-definition",
    "lash.process-transfer-set",
    "lash.process-wake",
    "lash.queued-work-claim-lease",
    "lash.runtime-usage-payload",
    "lash.session-ingress-submission",
    "lash.tool-invocation-batch",
    "lash.tool-intent",
    "lash.turn-cancel-peek",
    "lash.trigger-command",
    "lash.trigger-delivery-process",
    "lash.trigger-operation-address",
    "lash.trigger-source",
    "lash.trigger-subscription-address",
    "lash.trigger-subscription-definition",
    "lash.trigger-subscription-key",
    "lash.turn-input-submission",
    // FIG-3607: the idempotency key of one process start. The registration
    // fingerprint (`lash.process-registration-definition`) and the derived
    // trigger-delivery process id (`lash.trigger-delivery-process`) are
    // retired with it and stay reserved above.
    "lash.process-start-key",
];

/// Grandfathered families whose preimages omit the framing header (ADR 0097).
///
/// `lash.append-request`, `lash.history-node`, and `lash.intent` predate this
/// kit: their digests are persisted as opaque equality-compared evidence —
/// append-receipt request hashes, history node ids, and turn-commit hashes —
/// so their preimage bytes are frozen and can never gain the
/// `magic || salt || family-version || domain` header. They are registered in
/// `FAMILY_DOMAINS` so no later family claims the same semantic domain, and
/// they are the only domains [`IdentityEncoder::new_unframed`] will mint.
///
/// Entries are permanent and append-only: a frozen grammar stays frozen.
pub(crate) const FROZEN_UNFRAMED_DOMAINS: &[&str] =
    &["lash.append-request", "lash.history-node", "lash.intent"];

/// Append-only builder for one family-owned durable identity preimage.
pub struct IdentityEncoder {
    bytes: Vec<u8>,
}

impl IdentityEncoder {
    pub fn new(domain: &str, family_version: u8) -> Self {
        debug_assert!(
            domain == "test" || FAMILY_DOMAINS.contains(&domain),
            "durable identity family domain `{domain}` is missing from FAMILY_DOMAINS"
        );
        let mut encoder = Self { bytes: Vec::new() };
        encoder.raw_bytes(MAGIC);
        encoder.u8(GLOBAL_SALT);
        encoder.u8(family_version);
        encoder.bytes(domain.as_bytes());
        encoder
    }

    /// The emitted bytes are the family's frozen legacy grammar; the
    /// registration check still applies so the unframed path cannot leak into
    /// a new family. New durable identity families must use
    /// [`IdentityEncoder::new`].
    pub fn new_unframed(domain: &str) -> Self {
        debug_assert!(
            FROZEN_UNFRAMED_DOMAINS.contains(&domain),
            "unframed durable identity preimage `{domain}` is not a registered \
             frozen family; new families must use IdentityEncoder::new"
        );
        Self { bytes: Vec::new() }
    }

    /// Tags are permanent and must never be reused after a variant or field is retired.
    pub fn tag(&mut self, tag: u8) {
        self.u8(tag);
    }

    pub fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn u32(&mut self, value: u32) {
        self.raw_bytes(&value.to_be_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.raw_bytes(&value.to_be_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.raw_bytes(&value.to_be_bytes());
    }

    pub fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.u64(value.len() as u64);
        self.raw_bytes(value);
    }

    /// Emits the universal optional-value presence tags: 0 = absent, 1 =
    /// present. Family projections append the present value immediately.
    pub fn optional<T>(&mut self, value: Option<T>, present: impl FnOnce(&mut Self, T)) {
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
    pub fn sequence<T>(
        &mut self,
        values: impl IntoIterator<Item = T>,
        mut element: impl FnMut(&mut Self, T),
    ) {
        let values = values.into_iter().collect::<Vec<_>>();
        self.u64(values.len() as u64);
        for value in values {
            element(self, value);
        }
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn raw_bytes(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }
}

pub fn provider_route(
    identity: &mut IdentityEncoder,
    route: &lash_sansio::llm::types::ProviderRouteIdentity,
) {
    identity.string(&route.provider);
    identity.string(&route.endpoint);
    identity.string(&route.model);
}

pub fn rendered_hash(prefix: &str, family_version: u8, preimage: &[u8]) -> String {
    format!(
        "{prefix}:v{family_version}:blake3:{}",
        crate::stable_hash::blake3_hex("lash-stable-identity/v2", preimage)
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
        encoder.i64(-2);
        encoder.string("a:b");
        encoder.bytes(&[0, 1]);
        encoder.optional::<u8>(None, |_, _| unreachable!());
        encoder.optional(Some(9_u8), IdentityEncoder::u8);
        encoder.sequence([4_u8, 5], IdentityEncoder::u8);

        assert_eq!(
            hex(&encoder.finish()),
            "6c6173682d737461626c652d6964656e74697479020700000000000000047465737403010203040102030405060708fffffffffffffffe0000000000000003613a620000000000000002000100010900000000000000020405"
        );
    }

    #[test]
    fn frozen_unframed_domains_are_registered_family_domains() {
        for domain in FROZEN_UNFRAMED_DOMAINS {
            assert!(
                FAMILY_DOMAINS.contains(domain),
                "frozen unframed family `{domain}` must also be reserved in FAMILY_DOMAINS"
            );
        }
        let mut encoder = IdentityEncoder::new_unframed("lash.intent");
        encoder.string("preimage");
        assert_eq!(encoder.finish(), b"\0\0\0\0\0\0\0\x08preimage");
    }

    #[test]
    fn durable_identity_family_domains_are_unique() {
        let unique = FAMILY_DOMAINS
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            unique.len(),
            FAMILY_DOMAINS.len(),
            "durable family domains are permanently reserved and must be unique"
        );
    }
}
