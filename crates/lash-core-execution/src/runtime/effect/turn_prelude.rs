//! A turn's recorded preparation, stored content-addressed beside the turn
//! (FIG-5133).
//!
//! The environment sync records the prelude so a replay never re-runs the
//! transforms that produced it, but the prelude carries the whole history,
//! so journaling it verbatim grows every turn's journal with its
//! transcript. The sync's step body writes the prelude's bytes to the store
//! set before its outcome completes, and the outcome journals only their
//! digest. The bytes are held by the turn journal's `execution` referrer
//! under a journal guard (ADR 0113 §2.1), so the cleanup relay releases
//! them once the turn's run settles and the journal cannot replay.

use crate::runtime::effect::RuntimeEffectControllerError;

/// version_surface = "coexist"
/// version_guard(items(TURN_PRELUDE_DOMAIN, of_store_bytes))
const TURN_PRELUDE_DOMAIN: &str = "lash-turn-prelude/v1";

/// Preparation facts retained with the environment render. Pressure hooks
/// replay through nested effects; consumers adopt these recorded facts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnPrelude {
    pub configuration: crate::EffectAddress,
    pub pressure: Vec<crate::plugin::DecidedContextPressure>,
    /// Real turn history, independent of the ephemeral Prompt View.
    pub history: crate::MessageSequence,
    pub context: crate::session_model::context::PreparedContext,
    pub before_turn: Option<crate::EffectAddress>,
}

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

impl TurnPrelude {
    /// Write this prelude to `store`, held by the journal of `scope`, and
    /// answer the reference the outcome journals. The write completes before
    /// the step's outcome does, so a recorded reference always names stored
    /// bytes until the journal that recorded it has settled.
    ///
    /// # Errors
    ///
    /// A store that did not answer is a live fault the step does not record;
    /// a fenced referrer or a refused write settles as its typed code.
    pub async fn record(
        &self,
        store: &dyn TurnPreludeStore,
        scope: &crate::ExecutionScope,
    ) -> Result<TurnPreludeRef, RuntimeEffectControllerError> {
        let bytes = serde_json::to_vec(self).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("the turn prelude could not be encoded: {error}"),
            )
        })?;
        let prelude_ref = TurnPreludeRef::of_store_bytes(&bytes);
        let claim = crate::session::execution_claim_of(scope)
            .map_err(|error| store_failure(&prelude_ref, error))?;
        store
            .publish_turn_prelude(&claim, &prelude_ref, &bytes)
            .await
            .map_err(|error| store_failure(&prelude_ref, error.into()))?;
        Ok(prelude_ref)
    }
}

impl TurnPreludeRef {
    /// The prelude this reference recorded, read from `store` and verified
    /// against the digest. A replay never re-derives a prelude: bytes that
    /// are gone are `ArtifactMissing`, and bytes that are not the recorded
    /// ones, or do not decode, are `RuntimeStoreCorrupt`.
    ///
    /// # Errors
    ///
    /// The typed refusals above; a store that did not answer is a live
    /// fault, so a redrive reads again.
    pub async fn read(
        &self,
        store: &dyn TurnPreludeStore,
    ) -> Result<Box<TurnPrelude>, RuntimeEffectControllerError> {
        let bytes = store
            .get_turn_prelude(self)
            .await
            .map_err(|error| store_failure(self, error.into()))?
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::ArtifactMissing,
                    format!("the recorded turn prelude `{self}` is not stored"),
                )
            })?;
        if !self.matches_store_bytes(&bytes) {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("the stored turn prelude `{self}` is not the recorded one"),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("the stored turn prelude `{self}` does not decode: {error}"),
            )
        })
    }
}

/// A store error met writing or reading a prelude, settled by its own
/// cause: a live fault stays unrecorded and retryable.
fn store_failure(
    prelude_ref: &TurnPreludeRef,
    error: crate::PluginError,
) -> RuntimeEffectControllerError {
    let mut settled = RuntimeEffectControllerError::from(error);
    settled.message = format!("turn prelude `{prelude_ref}`: {}", settled.message);
    if settled.turn_failure_cause() == crate::TurnFailureCause::LiveFault {
        settled.retryable_uncommitted_derivation()
    } else {
        settled
    }
}
