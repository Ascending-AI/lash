/// version_surface = "coexist"
/// version_guard(items(TURN_PRELUDE_DOMAIN, of_store_bytes))
const TURN_PRELUDE_DOMAIN: &str = "lash-turn-prelude/v1";

/// The digest of one recorded prelude's store bytes: the only part of the
/// prelude the environment sync journals.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnPreludeRef(String);

impl TurnPreludeRef {
    /// The reference that addresses `bytes`.
    #[must_use]
    pub fn of_store_bytes(bytes: &[u8]) -> Self {
        Self(crate::stable_hash::blake3_hex(TURN_PRELUDE_DOMAIN, bytes))
    }

    /// The reference as stored and journaled.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `bytes` are exactly the bytes this reference addresses.
    #[must_use]
    pub fn matches_store_bytes(&self, bytes: &[u8]) -> bool {
        Self::of_store_bytes(bytes) == *self
    }
}

impl std::fmt::Display for TurnPreludeRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The store of recorded turn preludes: immutable bytes under their digest,
/// kept alive by referrer edges over the shared artifact edges and fences
/// (ADR 0113). Only the cleanup executor severs edges, through
/// [`Self::end_turn_prelude_referrer`].
#[async_trait::async_trait]
pub trait TurnPreludeStore: Send + Sync {
    /// Store `bytes` under `prelude_ref` if absent and add the claim's edge,
    /// in one transaction that first checks the referrer's fence
    /// (`ReferrerEnded`) and arms the claim's guard. Bytes that are not the
    /// ones `prelude_ref` addresses are `Immutable`.
    async fn publish_turn_prelude(
        &self,
        claim: &crate::ReferrerClaim,
        prelude_ref: &TurnPreludeRef,
        bytes: &[u8],
    ) -> Result<(), crate::ArtifactStoreError>;

    /// Apply one resolved cleanup in one transaction (ADR 0113 §2.3).
    async fn end_turn_prelude_referrer(
        &self,
        cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError>;

    /// The bytes stored under `prelude_ref`, or `None` when none are.
    async fn get_turn_prelude(
        &self,
        prelude_ref: &TurnPreludeRef,
    ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError>;
}
