//! A turn's recorded preparation, stored content-addressed beside the turn
//! (FIG-5133).
//!
//! The sync journals only the prelude digest. The stored prelude names the
//! committed history window and records only changes to its two views
//! (FIG-5961): real history over that window, then the prepared prompt over
//! that history. Restore reads the named window; it never reruns preparation
//! hooks or attachment policies. The journal's guarded execution edge keeps
//! the prelude until settlement (ADR 0113 §2.1).

use crate::runtime::effect::RuntimeEffectControllerError;
use lash_sansio::core_support::{MessageCoreSupport as _, MessageSequenceCoreSupport as _};

/// version_surface = "coexist"
/// version_guard(items(TURN_PRELUDE_DOMAIN, of_store_bytes))
const TURN_PRELUDE_DOMAIN: &str = "lash-turn-prelude/v1";

/// Preparation facts retained with the environment render. Pressure hooks
/// replay through nested effects; consumers adopt these recorded facts.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TurnPrelude {
    pub configuration: crate::EffectAddress,
    pub pressure: Vec<crate::plugin::DecidedContextPressure>,
    /// The committed graph window preparation started from. Its leaf stays
    /// on the active ancestry while the run can resume; checkpoint tails
    /// are recorded in the history view rather than naming a collectable
    /// checkpoint.
    pub history: crate::store::SessionHeadRef,
    history_view: MessageView,
    prompt_view: MessageView,
    pub before_turn: Option<crate::EffectAddress>,
}

/// Changes to one message view. Unchanged messages stay in the referenced
/// window. A prompt records its omissions against real history, avoiding a
/// second copy of both the committed prefix and the turn's additions.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct MessageView {
    truncate_to: Option<usize>,
    replacements: Vec<(usize, crate::Message)>,
    appended: Vec<crate::Message>,
}

impl MessageView {
    fn between(base: &crate::MessageSequence, next: &crate::MessageSequence) -> Self {
        if let Some(delta) = base.preserved_extension_delta(next) {
            return Self {
                appended: delta.to_vec(),
                ..Self::default()
            };
        }
        Self {
            truncate_to: (next.len() < base.len()).then_some(next.len()),
            replacements: base
                .iter()
                .zip(next.iter())
                .enumerate()
                .filter(|(_, (before, after))| !before.content_equals(after))
                .map(|(index, (_, message))| (index, message.clone()))
                .collect(),
            appended: next.iter().skip(base.len()).cloned().collect(),
        }
    }

    fn restore(
        &self,
        base: &crate::MessageSequence,
    ) -> Result<crate::MessageSequence, RuntimeEffectControllerError> {
        let retained = self.truncate_to.unwrap_or(base.len());
        if retained > base.len()
            || self
                .replacements
                .iter()
                .any(|(index, _)| *index >= retained)
            || self
                .replacements
                .windows(2)
                .any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                "the recorded prelude's message edits do not address its window",
            ));
        }
        let mut messages = base.clone();
        if self.truncate_to.is_some() || !self.replacements.is_empty() {
            let changed = messages.make_mut();
            changed.truncate(retained);
            for (index, message) in &self.replacements {
                changed[*index] = message.clone();
            }
        }
        messages.extend(self.appended.clone());
        Ok(messages)
    }
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
    /// Start preparation over the named committed window. Capture the live
    /// views with [`Self::capture_messages`] before recording it.
    #[must_use]
    pub fn new(
        configuration: crate::EffectAddress,
        pressure: Vec<crate::plugin::DecidedContextPressure>,
        mut history: crate::store::SessionHeadRef,
        before_turn: Option<crate::EffectAddress>,
    ) -> Self {
        // Only graph facts are the base. A phase checkpoint can be retired
        // after the head advances, independently of the prelude's lifetime.
        history.checkpoint = None;
        Self {
            configuration,
            pressure,
            history,
            history_view: MessageView::default(),
            prompt_view: MessageView::default(),
            before_turn,
        }
    }

    /// Retain only the changes preparation made over the committed window,
    /// and the prepared prompt's changes over real history.
    pub fn capture_messages(
        &mut self,
        base: &crate::MessageSequence,
        history: &crate::MessageSequence,
        prompt: &crate::MessageSequence,
    ) {
        self.history_view = MessageView::between(base, history);
        self.prompt_view = MessageView::between(history, prompt);
    }

    /// Reconstruct the pinned base, recorded history and prepared context
    /// from the named window. No preparation hook or policy runs on restore.
    ///
    /// # Errors
    /// Missing or corrupt window state refuses restore; store faults retain
    /// their typed cause and retry classification.
    pub async fn restore(
        &self,
        store: &crate::store::SessionStore,
    ) -> Result<
        (
            crate::MessageSequence,
            crate::MessageSequence,
            crate::session_model::context::PreparedContext,
        ),
        RuntimeEffectControllerError,
    > {
        if self.configuration.execution_scope.session_id() != Some(store.session_id()) {
            return Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                "the recorded prelude names another session's history",
            ));
        }
        let base = crate::store::load_session_window_state(
            store,
            crate::store::WindowSelector::Admitted(self.history.clone()),
        )
        .await
        .map_err(|error| {
            let settled = RuntimeEffectControllerError::from(crate::PluginError::from(error));
            if settled.turn_failure_cause() == crate::TurnFailureCause::LiveFault {
                settled.retryable_uncommitted_derivation()
            } else {
                settled
            }
        })?
        .ok_or_else(|| {
            RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::ArtifactMissing,
                "the recorded prelude's history window is not stored",
            )
        })?;
        let base = crate::MessageSequence::from_base(base.state.read_model().messages);
        let history = self.history_view.restore(&base)?;
        let prompt = self.prompt_view.restore(&history)?;
        Ok((
            base,
            history,
            crate::session_model::context::PreparedContext { messages: prompt },
        ))
    }

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
