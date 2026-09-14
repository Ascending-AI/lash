use crate::SessionId;
use crate::TurnId;
use crate::{CheckpointKind, PluginMessage, TurnCause, TurnInput};







#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]




/// Generates a turn-input wire vocabulary and its complete variant list from one declaration.
///
/// The generated encoder and decoder matches are exhaustive, so adding a variant requires its
/// persisted spelling here and necessarily extends `ALL`.
macro_rules! turn_input_wire {
    ($type:ident, $visibility:vis, $encoder:ident, $decoder:ident {
        $($variant:ident => $wire:literal),+ $(,)?
    }) => {
        impl $type {
            #[allow(dead_code)]
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// Returns the stable wire spelling persisted by turn-input stores.
            $visibility fn $encoder(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }

            /// Parses a stable wire spelling persisted by turn-input stores.
            #[allow(dead_code)]
            $visibility fn $decoder(value: &str) -> Option<Self> {
                match value {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

turn_input_wire!(TurnInputCheckpointBoundary, pub(crate), as_wire_str, from_wire_str {
    AfterWork => "after_work",
    BeforeCompletion => "before_completion",
});

/// Generates the checkpoint enumeration a claim can name, from one variant list.
///
/// The generated match is exhaustive, so a new [`CheckpointKind`] variant fails to compile until it
/// is added here, which keeps `CLAIM_CHECKPOINTS` complete for the tests that sweep every
/// checkpoint.
macro_rules! turn_input_claim_checkpoints {
    ($($variant:ident),+ $(,)?) => {
        #[cfg(test)]
        pub(crate) const CLAIM_CHECKPOINTS: &[CheckpointKind] = &[$(CheckpointKind::$variant),+];

        /// Compile-time guard only: the exhaustive match below is what fails on a new variant.
        #[cfg(test)]
        #[allow(dead_code)]
        fn assert_claim_checkpoints_are_exhaustive(checkpoint: CheckpointKind) {
            match checkpoint {
                $(CheckpointKind::$variant => ()),+
            }
        }
    };
}

turn_input_claim_checkpoints!(AfterWork, BeforeCompletion);





turn_input_wire!(TurnInputState, pub, as_str, from_wire_str {
    PendingActive => "pending_active",
    DeferredNextTurn => "deferred_next_turn",
    Accepted => "accepted",
    Cancelled => "cancelled",
    Completed => "completed",
});











































/// Turn-input rows a turn accepted itself and drives without a claim.
///
/// The unclaimed half of a turn's drive: the rows exist durably before the
/// turn executes, exactly as a claimed row does, but no session-execution lease
/// fences them. Their settlement is decided by the head CAS alone
/// ([ADR 0069 §5](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0069-durable-acceptance-is-the-sole-turn-ingress.md)).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UnclaimedTurnInputs {
    pub session_id: SessionId,
    pub inputs: Vec<PendingTurnInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applications: Vec<TurnInputApplication>,
}

impl UnclaimedTurnInputs {
    /// Exposes settlement to store and durable-substrate implementors driving
    /// rows they accepted themselves.
    pub fn completion(&self) -> TurnInputCompletion {
        TurnInputCompletion {
            session_id: self.session_id.clone(),
            claim: None,
            data: TurnInputCompletionData {
                input_ids: self
                    .inputs
                    .iter()
                    .map(|input| input.input_id.clone())
                    .collect(),
                applications: self.applications.clone(),
            },
        }
    }

    /// Records the initial application evidence for rows driven without a
    /// claim, matching [`TurnInputClaim::record_initial_turn_application`].
    pub fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        if !self.applications.is_empty() {
            if !self.applications.iter().all(|application| {
                application.turn_id == *turn_id
                    && application.committed_message_id == committed_message_id
                    && application.checkpoint.is_none()
            }) {
                return Err(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::TurnInputRedriveSetUnavailable,
                    format!(
                        "cannot apply retained turn-input redrive set for session `{}` as the \
                         initial group of turn `{turn_id}`: its durable applications belong to \
                         another turn or checkpoint",
                        self.session_id
                    ),
                ));
            }
            return Ok(());
        }
        self.applications = initial_turn_applications(&self.inputs, turn_id, committed_message_id);
        Ok(())
    }
}

/// The turn-input rows one turn is driving, with the authority it will settle
/// them under.
///
/// Exactly two regimes, one authority: a claimed drive settles under its
/// generation-fenced claim, an unclaimed drive settles under the head CAS
/// alone. Everything between acceptance and settlement — materialization,
/// application evidence, the committed message's provenance — is identical, so
/// the runtime carries both through one list rather than two.
#[derive(Clone, Debug)]
pub(crate) enum TurnInputDrive {
    Claimed(TurnInputClaim),
    Unclaimed(UnclaimedTurnInputs),
}

impl TurnInputDrive {
    pub(crate) fn inputs(&self) -> &[PendingTurnInput] {
        match self {
            Self::Claimed(claim) => &claim.inputs,
            Self::Unclaimed(unclaimed) => &unclaimed.inputs,
        }
    }

    pub(crate) fn applications(&self) -> &[TurnInputApplication] {
        match self {
            Self::Claimed(claim) => &claim.applications,
            Self::Unclaimed(unclaimed) => &unclaimed.applications,
        }
    }

    pub(crate) fn completion(&self) -> TurnInputCompletion {
        match self {
            Self::Claimed(claim) => claim.completion(),
            Self::Unclaimed(unclaimed) => unclaimed.completion(),
        }
    }

    /// The lease generation this drive is fenced by, or `None` when it holds no
    /// claim. Only a claimed drive can be superseded by a later generation, so
    /// only a claimed drive can have its settlement dropped and retried.
    pub(crate) fn claim_generation(&self) -> Option<(String, u64)> {
        match self {
            Self::Claimed(claim) => Some((claim.claim_id.clone(), claim.session_lease_generation)),
            Self::Unclaimed(_) => None,
        }
    }

    pub(crate) fn as_claim(&self) -> Option<&TurnInputClaim> {
        match self {
            Self::Claimed(claim) => Some(claim),
            Self::Unclaimed(_) => None,
        }
    }

    pub(crate) fn materialize_turn_input(&self) -> TurnInput {
        match self {
            Self::Claimed(claim) => claim.materialize_turn_input(),
            Self::Unclaimed(unclaimed) => materialize_turn_input(&unclaimed.inputs),
        }
    }

    pub(crate) fn record_initial_turn_application(
        &mut self,
        turn_id: &crate::TurnId,
        committed_message_id: &str,
    ) -> Result<(), crate::RuntimeError> {
        match self {
            Self::Claimed(claim) => {
                claim.record_initial_turn_application(turn_id, committed_message_id);
                Ok(())
            }
            Self::Unclaimed(unclaimed) => {
                unclaimed.record_initial_turn_application(turn_id, committed_message_id)
            }
        }
    }
}

fn initial_turn_applications(
    inputs: &[PendingTurnInput],
    turn_id: &crate::TurnId,
    committed_message_id: &str,
) -> Vec<TurnInputApplication> {
    inputs
        .iter()
        .filter(|input| {
            input.input.items.iter().any(|item| match item {
                crate::InputItem::Text { text } => !text.is_empty(),
                crate::InputItem::Attachment { .. } => true,
            })
        })
        .map(|input| TurnInputApplication {
            input_id: input.input_id.clone(),
            source_key: input.source_key.clone(),
            turn_id: turn_id.clone(),
            committed_message_id: committed_message_id.to_string(),
            checkpoint: None,
        })
        .collect()
}

fn materialize_turn_input(inputs: &[PendingTurnInput]) -> TurnInput {
    let mut input_items = Vec::new();
    let mut protocol_turn_options = None;
    let mut trace_turn_id = None;
    for pending in inputs {
        input_items.extend(pending.input.items.clone());
        if protocol_turn_options.is_none() {
            protocol_turn_options = pending.input.protocol_turn_options.clone();
        }
        if trace_turn_id.is_none() {
            trace_turn_id = pending.input.trace_turn_id.clone();
        }
    }
    TurnInput {
        items: input_items,
        protocol_turn_options,
        trace_turn_id,
        protocol_extension: None,
        turn_context: crate::TurnContext::default(),
    }
}







#[derive(Clone, Debug, Default)]
pub struct QueuedCheckpointTurnInput {
    pub messages: Vec<crate::Message>,
    pub turn_causes: Vec<TurnCause>,
}

pub(crate) fn source_key_display_id(source: &str) -> String {
    source
        .strip_prefix("host:")
        .or_else(|| source.strip_prefix("injection:"))
        .unwrap_or(source)
        .to_string()
}

pub(crate) fn plugin_message_from_turn_input(input: &TurnInput) -> Option<PluginMessage> {
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for item in &input.items {
        match item {
            crate::InputItem::Text { text: item_text } if !item_text.is_empty() => {
                text.push(item_text.clone());
            }
            crate::InputItem::Text { .. } => {}
            crate::InputItem::Attachment { source } => attachments.push(source.clone()),
        }
    }
    if text.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(PluginMessage {
        id: None,
        role: crate::MessageRole::User,
        content: text.join("\n"),
        origin: None,
        parts: Vec::new(),
        attachments,
    })
}

async fn committed_message_from_pending_input(
    pending: &PendingTurnInput,
    turn_id: &crate::TurnId,
    attachment_store: &crate::SessionAttachmentStore,
    attachment_source_policy: &dyn crate::AttachmentSourcePolicy,
) -> Result<Option<crate::Message>, String> {
    let normalized = super::io::normalize_input_items(
        &pending.input.items,
        attachment_store,
        attachment_source_policy,
    )
    .await?;
    let message_id = ingress_message_id(&pending.input_id);
    let mut parts = Vec::new();
    for item in normalized {
        match item {
            super::NormalizedItem::Text(text) if !text.is_empty() => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part::text(part_id, text, None));
            }
            super::NormalizedItem::Text(_) => {}
            super::NormalizedItem::Attachment(source) => {
                let part_id = format!("{message_id}.p{}", parts.len());
                parts.push(crate::Part::attachment_part(
                    part_id,
                    String::new(),
                    Some(crate::session_model::message::PartAttachment { source }),
                ));
            }
        }
    }
    if parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(crate::Message {
        id: message_id,
        role: crate::MessageRole::User,
        // Same typed provenance the turn's opening input carries: the absorbing
        // turn plus the durable input this message came from (FIG-972).
        origin: Some(crate::MessageOrigin::TurnInput {
            turn_id: turn_id.clone(),
            input_id: Some(pending.input_id.clone()),
        }),
        parts: crate::shared_parts(parts),
    }))
}

impl crate::TurnInput {
    /// The part of this input a durable acceptance row can carry.
    ///
    /// `protocol_extension` and live `TurnContext` plugin inputs are
    /// process-local handles that no store can hold, so the acceptance commit
    /// records everything else and the caller driving the turn keeps the live
    /// state (ADR 0069). A worker that later recovers the row drives exactly
    /// this projection.
    ///
    /// `trace_turn_id` is dropped for the same reason: it labels one drive
    /// attempt, not the input. A recovered row is driven under the recovering
    /// worker's own execution scope, and a persisted trace id from the
    /// abandoned attempt would collide with it
    /// ([`RuntimeErrorCode::ExecutionScopeTurnIdMismatch`](crate::RuntimeErrorCode::ExecutionScopeTurnIdMismatch)),
    /// making an accepted direct turn unrecoverable — exactly the property
    /// ADR 0069 exists to guarantee.
    #[must_use]
    pub(crate) fn durable_projection(&self) -> Self {
        Self {
            items: self.items.clone(),
            protocol_turn_options: self.protocol_turn_options.clone(),
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: crate::TurnContext::default(),
        }
    }
}

pub fn ingress_message_id(input_id: &str) -> String {
    format!("m_ingress_{input_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_boundary_wire_values_match_the_persisted_ingress_encoding() {
        for boundary in TurnInputCheckpointBoundary::ALL.iter().copied() {
            assert_eq!(
                serde_json::to_value(boundary).expect("serialize boundary"),
                serde_json::Value::String(boundary.as_wire_str().to_string()),
                "the SQL literal a store filters on must equal the persisted wire value"
            );
        }
    }

    #[test]
    fn turn_input_state_wire_round_trips_every_variant() {
        for state in TurnInputState::ALL.iter().copied() {
            assert_eq!(TurnInputState::from_wire_str(state.as_str()), Some(state));
        }
    }

    #[test]
    fn turn_input_state_wire_values_match_the_persisted_ingress_encoding() {
        assert_eq!(TurnInputState::PendingActive.as_str(), "pending_active");
        assert_eq!(
            TurnInputState::DeferredNextTurn.as_str(),
            "deferred_next_turn"
        );
        assert_eq!(TurnInputState::Accepted.as_str(), "accepted");
        assert_eq!(TurnInputState::Cancelled.as_str(), "cancelled");
        assert_eq!(TurnInputState::Completed.as_str(), "completed");
    }

    #[test]
    fn turn_input_state_terminality_covers_exactly_settled_states() {
        for state in TurnInputState::ALL.iter().copied() {
            assert_eq!(
                state.is_terminal(),
                matches!(state, TurnInputState::Cancelled | TurnInputState::Completed),
                "terminality drifted for {state:?}"
            );
        }
    }

    #[test]
    fn every_checkpoint_admits_at_least_the_default_boundary() {
        for checkpoint in CLAIM_CHECKPOINTS.iter().copied() {
            let admitted = TurnInputCheckpointBoundary::ALL
                .iter()
                .filter(|boundary| boundary.admits(checkpoint))
                .collect::<Vec<_>>();
            assert!(
                admitted.contains(&&TurnInputCheckpointBoundary::default()),
                "an absent min_boundary reads as the default, which must stay admissible at {checkpoint:?}"
            );
            let predicate =
                crate::store_backend_support::admitted_min_boundary_sql("min_boundary", checkpoint);
            assert!(
                !predicate.contains("IN ()"),
                "an empty admitted set must not emit an `IN ()` syntax error at {checkpoint:?}"
            );
        }
        assert!(
            !TurnInputCheckpointBoundary::BeforeCompletion.admits(CheckpointKind::AfterWork),
            "before-completion ingress must be withheld at the after-work checkpoint"
        );
    }

    #[test]
    fn admitted_min_boundary_sql_spells_the_current_checkpoints_exactly() {
        assert_eq!(
            crate::store_backend_support::admitted_min_boundary_sql(
                "min_boundary",
                CheckpointKind::AfterWork
            ),
            "COALESCE(min_boundary, 'after_work') IN ('after_work')"
        );
        assert_eq!(
            crate::store_backend_support::admitted_min_boundary_sql(
                "min_boundary",
                CheckpointKind::BeforeCompletion
            ),
            "COALESCE(min_boundary, 'after_work') IN ('after_work', 'before_completion')"
        );
    }

    #[test]
    fn pending_turn_input_id_mint_preserves_the_fig_886_format() {
        assert_eq!(
            derive_pending_turn_input_id(&SessionId::from("session"), Some("source"), 123, 7),
            "ti:f876d5a24aeb836217de2df548afda96b5194380cfb630d3a4ececf306ec20eb"
        );
        assert_ne!(
            derive_pending_turn_input_id(&SessionId::from("session"), Some("source"), 123, 7),
            derive_pending_turn_input_id(&SessionId::from("session"), Some("source"), 123, 8)
        );
    }
}
