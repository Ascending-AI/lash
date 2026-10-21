//! A turn's recorded preparation, stored content-addressed beside the turn
//! (FIG-5133).
//!
//! The sync journals only the prelude digest. The stored prelude names the
//! committed history window and records only changes to its two views
//! (FIG-5961): real history over that window, then the prepared prompt over
//! that history. Restore reads the named window, or takes it from a caller
//! that already holds it (FIG-6634); it never reruns preparation hooks or
//! attachment policies. The journal's guarded execution edge keeps
//! the prelude until settlement (ADR 0113 §2.1).

use lash_core_store::runtime_error::RuntimeEffectControllerError;
use lash_execution_contracts::turn_prelude::{TurnPreludeRef, TurnPreludeStore};
use lash_protocol_types::core_support::MessageCoreSupport as _;

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
    pub history: lash_core_store::store::SessionHeadRef,
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
                lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt,
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

impl TurnPrelude {
    /// Start preparation over the named committed window. Capture the live
    /// views with [`Self::capture_messages`] before recording it.
    #[must_use]
    pub fn new(
        configuration: crate::EffectAddress,
        pressure: Vec<crate::plugin::DecidedContextPressure>,
        mut history: lash_core_store::store::SessionHeadRef,
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
    /// from the named window, read from `store`. No preparation hook or
    /// policy runs on restore.
    ///
    /// # Errors
    /// Missing or corrupt window state refuses restore; store faults retain
    /// their typed cause and retry classification.
    pub async fn restore(
        &self,
        store: &lash_core_store::store::SessionStore,
    ) -> Result<
        (
            crate::MessageSequence,
            crate::MessageSequence,
            crate::session_model::context::PreparedContext,
        ),
        RuntimeEffectControllerError,
    > {
        self.check_session(store.session_id())?;
        let base = crate::store::load_session_window_state(
            store,
            lash_core_store::store::WindowSelector::Admitted(self.history.clone()),
        )
        .await
        .map_err(|error| {
            let settled = RuntimeEffectControllerError::from(
                lash_execution_contracts::plugin_error::PluginError::from(error),
            );
            if settled.turn_failure_cause() == crate::TurnFailureCause::LiveFault {
                settled.retryable_uncommitted_derivation()
            } else {
                settled
            }
        })?
        .ok_or_else(|| {
            RuntimeEffectControllerError::new(
                lash_core_store::runtime_error::RuntimeErrorCode::ArtifactMissing,
                "the recorded prelude's history window is not stored",
            )
        })?;
        let read = base.state.read_model();
        self.restore_over(
            store.session_id(),
            lash_protocol_types::MessageSequence::from_base(read.messages)
                .with_base_render_cache(read.prompt_render_cache),
        )
    }

    /// Reconstruct the recorded history and prepared context over `base`,
    /// the messages of the window this prelude names in `session` as a
    /// store read of it gives them: the read model's messages over its
    /// render cache. The caller answers for `base` being that window.
    ///
    /// # Errors
    /// A prelude of another session, or message edits that do not address
    /// `base`, refuse restore.
    pub fn restore_over(
        &self,
        session: &lash_vocabulary::identity::SessionId,
        base: crate::MessageSequence,
    ) -> Result<
        (
            crate::MessageSequence,
            crate::MessageSequence,
            crate::session_model::context::PreparedContext,
        ),
        RuntimeEffectControllerError,
    > {
        self.check_session(session)?;
        let history = self.history_view.restore(&base)?;
        let prompt = self.prompt_view.restore(&history)?;
        Ok((
            base,
            history,
            crate::session_model::context::PreparedContext { messages: prompt },
        ))
    }

    fn check_session(
        &self,
        session: &lash_vocabulary::identity::SessionId,
    ) -> Result<(), RuntimeEffectControllerError> {
        if self.configuration.execution_scope.session_id() != Some(session) {
            return Err(RuntimeEffectControllerError::new(
                lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt,
                "the recorded prelude names another session's history",
            ));
        }
        Ok(())
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
                lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt,
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

impl TurnPrelude {
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
        reference: &TurnPreludeRef,
        store: &dyn TurnPreludeStore,
    ) -> Result<Box<TurnPrelude>, RuntimeEffectControllerError> {
        let bytes = store
            .get_turn_prelude(reference)
            .await
            .map_err(|error| store_failure(reference, error.into()))?
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    lash_core_store::runtime_error::RuntimeErrorCode::ArtifactMissing,
                    format!("the recorded turn prelude `{reference}` is not stored"),
                )
            })?;
        if !reference.matches_store_bytes(&bytes) {
            return Err(RuntimeEffectControllerError::new(
                lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("the stored turn prelude `{reference}` is not the recorded one"),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            RuntimeEffectControllerError::new(
                lash_core_store::runtime_error::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("the stored turn prelude `{reference}` does not decode: {error}"),
            )
        })
    }
}

/// A store error met writing or reading a prelude, settled by its own
/// cause: a live fault stays unrecorded and retryable.
fn store_failure(
    prelude_ref: &TurnPreludeRef,
    error: lash_execution_contracts::plugin_error::PluginError,
) -> RuntimeEffectControllerError {
    let mut settled = RuntimeEffectControllerError::from(error);
    settled.message = format!("turn prelude `{prelude_ref}`: {}", settled.message);
    if settled.turn_failure_cause() == crate::TurnFailureCause::LiveFault {
        settled.retryable_uncommitted_derivation()
    } else {
        settled
    }
}
