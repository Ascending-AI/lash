use crate::ProcessId;
use crate::SessionId;
use crate::{ToolIntentIdentity, ToolIntentKind, ToolIntentRefusalReason};
use serde::{Deserialize, Serialize};

/// The only intent-to-command protocol understood by this build.
///
/// Version 3 replaces the start declaration's full `ProcessStartRequest` with
/// an id-less [`crate::ProcessStartDeclaration`]: the process id is derived
/// from the declaring attempt's intent identity instead of being carried and
/// then overwritten. A v2 batch or durable submission row therefore decodes to
/// a shape this build cannot realize, so both are refused before any
/// declaration effect rather than reinterpreted — the same treatment version 1
/// received when version 2 rebound `EmitTrigger` occurrence idempotency to the
/// declaration replay key.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_PROTOCOL_V3: u16 = 3;
/// Maximum declarations accepted from one recorded attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_COUNT: usize = 32;
/// Maximum canonical JSON bytes accepted from one recorded intent batch.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_CANONICAL_BYTES: usize = 64 * 1024;
/// Maximum declarations of any one kind accepted from one recorded attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_PER_KIND: usize = 16;

/// Recorded declarations returned by a leaf tool attempt.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolIntents {
    /// Version selecting the literal admission and realization contract.
    pub protocol_version: u16,
    /// Ordered declarations whose indexes participate in durable identity.
    pub intents: Vec<ToolIntent>,
}

impl Default for ToolIntents {
    fn default() -> Self {
        Self::v3(Vec::new())
    }
}

impl ToolIntents {
    /// Construct a version-3 declaration batch for protocol and process-engine implementors.
    pub fn v3(intents: Vec<ToolIntent>) -> Self {
        Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V3,
            intents,
        }
    }

    /// Test whether a declaration batch is empty before protocol admission.
    ///
    /// This is an **integrator class 3: protocol and process-engine implementor** seam.
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }
}

/// The payload type each generated [`ToolIntent`] variant carries.
///
/// One arm per variant of [`lash_sansio::tool_intent_variants!`]: a variant
/// added to that list without a payload here is a compile error, so the enum
/// and the kind set cannot diverge.
macro_rules! tool_intent_payload {
    (StartProcess) => { Box<StartProcessIntent> };
    (SignalProcess) => { SignalProcessIntent };
    (CancelProcess) => { CancelProcessIntent };
    (EmitProcessEvent) => { EmitProcessEventIntent };
    (EmitTrigger) => { EmitTriggerIntent };
    (RegisterProcessDefinition) => { Box<RegisterProcessDefinitionIntent> };
    (RegisterTrigger) => { Box<RegisterTriggerIntent> };
}

macro_rules! define_tool_intent {
    ($($variant:ident $wire:literal,)*) => {
        /// Durable follow-on work a recorded leaf attempt may request.
        ///
        /// Generated from [`lash_sansio::tool_intent_variants!`] together with
        /// [`ToolIntentKind`], so `kind()` is a projection of one list rather
        /// than a hand-kept mirror.
        /// **Integrator class 3: protocol and process-engine implementors.**
        #[derive(Clone, Debug, Serialize, Deserialize)]
        #[serde(tag = "kind", content = "intent", rename_all = "snake_case")]
        pub enum ToolIntent {
            $($variant(tool_intent_payload!($variant)),)*
        }

        impl ToolIntent {
            /// Return the literal command kind used by protocol and process-engine implementors.
            pub fn kind(&self) -> ToolIntentKind {
                match self {
                    $(Self::$variant(_) => ToolIntentKind::$variant,)*
                }
            }

            /// Return the session binding checked by protocol and process-engine implementors.
            pub fn session_id(&self) -> &str {
                match self {
                    $(Self::$variant(intent) => &intent.session_id,)*
                }
            }
        }
    };
}

lash_sansio::tool_intent_variants!(define_tool_intent);

/// Durable first-submission row for one runtime-owned tool-intent identity.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam. Process registries persist it so independent facade handles and
/// crash redrives consult the same first writer before realization.
#[derive(Clone, Debug, Serialize)]
pub struct ToolIntentSubmissionRecord {
    /// Version selecting the admission and realization contract.
    pub protocol_version: u16,
    /// Canonical `(session, scope, call, index)` identity and replay key.
    pub identity: ToolIntentIdentity,
    /// First submitted command kind.
    pub kind: ToolIntentKind,
    /// Hash of the first serialized payload.
    pub payload_hash: String,
    /// First payload retained for crash redrive.
    pub intent: ToolIntent,
    /// First typed realization outcome, absent while admission is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<crate::ToolIntentExecutionOutcome>,
}

impl ToolIntentSubmissionRecord {
    /// Builds the canonical first-submission row for protocol and
    /// process-engine implementors before any intent realization occurs.
    pub fn new(
        identity: ToolIntentIdentity,
        intent: ToolIntent,
    ) -> Result<Self, serde_json::Error> {
        let kind = intent.kind();
        let payload_hash = crate::stable_hash::blake3_hex(
            "lash-tool-intent-payload/v3",
            &serde_json::to_vec(&intent)?,
        );
        Ok(Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V3,
            identity,
            kind,
            payload_hash,
            intent,
            outcome: None,
        })
    }
}

#[derive(Deserialize)]
struct ToolIntentSubmissionRecordWire {
    protocol_version: Option<u16>,
    identity: ToolIntentIdentity,
    kind: ToolIntentKind,
    payload_hash: String,
    intent: ToolIntent,
    #[serde(default)]
    outcome: Option<crate::ToolIntentExecutionOutcome>,
}

impl<'de> Deserialize<'de> for ToolIntentSubmissionRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ToolIntentSubmissionRecordWire::deserialize(deserializer)?;
        Ok(Self {
            // Rows written before the protocol discriminator existed are
            // classified as v1 solely so ingress can refuse them before
            // realization. They are never upgraded or accepted implicitly.
            protocol_version: wire.protocol_version.unwrap_or(1),
            identity: wire.identity,
            kind: wire.kind,
            payload_hash: wire.payload_hash,
            intent: wire.intent,
            outcome: wire.outcome,
        })
    }
}

/// Atomic result of claiming a runtime-owned tool-intent identity.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam returned by [`crate::ProcessRegistry`] implementations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolIntentSubmissionAdmission {
    /// This caller durably installed the first submission.
    Admitted,
    /// Another caller or an earlier crash installed the returned first submission.
    Existing(Box<ToolIntentSubmissionRecord>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Start declaration consumed by protocol and process-engine implementors.
///
/// The declaration carries no process id. Realization derives it with
/// [`ProcessId::from_intent_identity`] from this declaration's own intent
/// identity, so the id a leaf attempt returns before commit and the id the
/// executor starts under are the same value on the first run and on every
/// redrive (FIG-2994).
pub struct StartProcessIntent {
    /// Session whose authority owns the child.
    pub session_id: SessionId,
    /// Durable process-start declaration, minus the derived id.
    pub declaration: crate::ProcessStartDeclaration,
}

impl StartProcessIntent {
    /// Bind this declaration to the process id its identity derives.
    ///
    /// Both realization routes call this and nothing else, so neither can
    /// substitute a freshly minted id.
    pub fn into_request(&self, identity: &ToolIntentIdentity) -> crate::ProcessStartRequest {
        self.declaration
            .clone()
            .into_request(ProcessId::from_intent_identity(identity))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Process-definition registration declaration consumed by protocol and
/// process-engine implementors.
///
/// The definition registry table is a separate child of FIG-2990; until it
/// lands this declaration is admitted, identified and journaled like any other
/// intent and its realization is refused with a typed
/// `process_definition_registry_unavailable` command failure rather than
/// silently succeeding. The declaration shape is what a leaf `register` tool
/// binds against, which is why it exists ahead of its table.
pub struct RegisterProcessDefinitionIntent {
    /// Session whose authority owns the registration.
    pub session_id: SessionId,
    /// Engine that owns the definition, e.g. the value an engine registry keys on.
    pub engine_kind: String,
    /// Engine-owned definition value.
    pub definition: serde_json::Value,
    /// Immutable execution environment the definition resolves against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_spec: Option<crate::ProcessExecutionEnvSpec>,
    /// Host-facing label, never part of the definition's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Trigger-registration declaration consumed by protocol and process-engine
/// implementors.
///
/// Distinct from [`EmitTriggerIntent`], which fires an occurrence: this one
/// installs the subscription. A leaf attempt cannot register synchronously for
/// the same reason it cannot emit synchronously — a subscription that outlived
/// a failed attempt would wake a target the attempt never committed.
pub struct RegisterTriggerIntent {
    /// Session whose authority owns the subscription.
    pub session_id: SessionId,
    /// Complete validated subscription draft.
    pub draft: crate::TriggerSubscriptionDraft,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Signal declaration consumed by protocol and process-engine implementors.
pub struct SignalProcessIntent {
    /// Session whose authority owns the signal.
    pub session_id: SessionId,
    /// Target process id.
    pub process_id: ProcessId,
    /// Declared signal name.
    pub signal_name: String,
    /// Signal payload validated by the target event schema.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Cancellation declaration consumed by protocol and process-engine implementors.
#[serde(deny_unknown_fields)]
pub struct CancelProcessIntent {
    /// Session whose authority owns the cancellation.
    pub session_id: SessionId,
    /// Target process id.
    pub process_id: ProcessId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Event declaration consumed by protocol and process-engine implementors.
pub struct EmitProcessEventIntent {
    /// Session whose authority owns the append.
    pub session_id: SessionId,
    /// Target process id.
    pub process_id: ProcessId,
    /// Registered event type.
    pub event_type: String,
    /// Event payload validated by the process registry.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Trigger declaration consumed by protocol and process-engine implementors.
///
/// A leaf attempt cannot emit a trigger synchronously: an emission that
/// outlives a failed attempt would advertise a cause that never committed.
/// Declaring this intent instead moves the emission behind the attempt's own
/// commit, where the shared realization router stamps the recorded occurrence's
/// `idempotency_key` with this declaration's replay key as the exactly-once
/// backstop for redrive.
///
/// Three consequences of carrying a whole [`crate::TriggerOccurrenceRequest`]:
///
/// - A submission's payload hash covers this struct's entire serde shape.
///   Adding a field to `TriggerOccurrenceRequest` without
///   `skip_serializing_if` changes the hash of an unchanged declaration, so a
///   submission recorded before the change is refused as `DuplicateIdentity`
///   after it. New fields belong behind `skip_serializing_if` unless a
///   deliberate identity break is the point.
/// - `request.idempotency_key` remains caller-supplied declaration material. It
///   feeds the serialized first-writer payload hash and therefore submission
///   identity/conflict detection. At the shared realization boundary the
///   router replaces it with the declaration replay key as the occurrence's
///   store-side dedupe key, so distinct declarations cannot collapse while
///   redriving the same declaration remains exactly-once.
/// - `session_id` here is the authority the intent executor validates the
///   declaration against; `request.session_id` is the occurrence's own routing
///   scope, which the router carries onto the occurrence record and never
///   checks against it.
pub struct EmitTriggerIntent {
    /// Session whose authority owns the emission. Validated: a declaration
    /// naming another session is refused before it reaches the router.
    pub session_id: SessionId,
    /// Complete durable trigger-occurrence request. At realization the router
    /// replaces its caller-supplied `idempotency_key` with the declaration
    /// replay key. Its own `session_id` is the occurrence's routing scope, not
    /// an authority.
    pub request: crate::TriggerOccurrenceRequest,
}

const TOOL_INTENT_IDENTITY_FAMILY_VERSION: u8 = 2;

/// The public identity seam for host-submitted intents.
///
/// Runtime-minted declarations use the same v2 family through
/// [`derive_tool_intent_identity_under`], which additionally binds the identity
/// to the durable invocation that minted the declaration.
pub fn derive_tool_intent_identity(
    session_id: &SessionId,
    execution_scope_id: &str,
    tool_call_id: Option<&str>,
    intent_index: usize,
) -> Result<ToolIntentIdentity, ToolIntentRefusalReason> {
    derive_tool_intent_identity_inner(
        session_id,
        execution_scope_id,
        tool_call_id,
        intent_index,
        None,
    )
}

/// The one derivation every runtime-side declaration site uses.
///
/// A declaration minted under a durable invocation binds that invocation's
/// replay key; one minted outside any invocation binds nothing. Both the
/// attempt's own `AttemptContext::intent_identity` and the intent executor's
/// realization pass their parent invocation here, so the identity an attempt
/// reports and the identity the executor realizes under cannot be derived by
/// two rules (FIG-2994).
pub fn derive_tool_intent_identity_under(
    session_id: &SessionId,
    execution_scope_id: &str,
    tool_call_id: Option<&str>,
    intent_index: usize,
    parent_invocation: Option<&crate::RuntimeInvocation>,
) -> Result<ToolIntentIdentity, ToolIntentRefusalReason> {
    derive_tool_intent_identity_inner(
        session_id,
        execution_scope_id,
        tool_call_id,
        intent_index,
        parent_invocation.and_then(crate::RuntimeInvocation::replay_key),
    )
}

fn derive_tool_intent_identity_inner(
    session_id: &SessionId,
    execution_scope_id: &str,
    tool_call_id: Option<&str>,
    intent_index: usize,
    minting_emission_replay_key: Option<&str>,
) -> Result<ToolIntentIdentity, ToolIntentRefusalReason> {
    let tool_call_id = tool_call_id.ok_or(ToolIntentRefusalReason::MissingToolCallId)?;
    let intent_index =
        u32::try_from(intent_index).map_err(|_| ToolIntentRefusalReason::IntentIndexOverflow)?;

    let mut encoder = crate::stable_identity::IdentityEncoder::new(
        "lash.tool-intent",
        TOOL_INTENT_IDENTITY_FAMILY_VERSION,
    );
    encoder.string(session_id);
    encoder.string(execution_scope_id);
    encoder.string(tool_call_id);
    encoder.u32(intent_index);
    encoder.optional(minting_emission_replay_key, |encoder, replay_key| {
        encoder.string(replay_key)
    });
    let replay_key = crate::stable_identity::rendered_hash(
        "tool-intent",
        TOOL_INTENT_IDENTITY_FAMILY_VERSION,
        &encoder.finish(),
    );
    Ok(ToolIntentIdentity {
        session_id: SessionId::from(session_id.to_string()),
        execution_scope_id: execution_scope_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        intent_index,
        replay_key,
        minting_emission_replay_key: minting_emission_replay_key.map(str::to_string),
    })
}

/// Re-derive an identity from its own durable fields.
///
/// The v2 replay key hashes the minting-emission replay key, so the record
/// retains that input; a record whose `replay_key` does not equal the
/// re-derived one carries a forged or corrupted identity.
pub fn rederive_tool_intent_identity(
    identity: &ToolIntentIdentity,
) -> Result<ToolIntentIdentity, ToolIntentRefusalReason> {
    derive_tool_intent_identity_inner(
        &identity.session_id,
        &identity.execution_scope_id,
        Some(&identity.tool_call_id),
        identity.intent_index as usize,
        identity.minting_emission_replay_key.as_deref(),
    )
}

pub(crate) fn derive_legacy_tool_intent_v1_replay_key(identity: &ToolIntentIdentity) -> String {
    let mut encoder = crate::stable_identity::IdentityEncoder::new("lash.tool-intent", 1);
    encoder.string(&identity.session_id);
    encoder.string(&identity.execution_scope_id);
    encoder.string(&identity.tool_call_id);
    encoder.u32(identity.intent_index);
    crate::stable_identity::rendered_hash("tool-intent", 1, &encoder.finish())
}

pub(crate) fn has_v2_tool_intent_replay_key(identity: &ToolIntentIdentity) -> bool {
    let Some(hash) = identity.replay_key.strip_prefix("tool-intent:v2:blake3:") else {
        return false;
    };
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

pub fn legacy_tool_intent_v1_lookup_key(
    invocation: &crate::RuntimeEffectInvocation,
) -> Option<String> {
    let crate::RuntimeReplayAttribution::ToolIntent(identity) =
        invocation.replay_attribution.as_ref()?;
    if !has_v2_tool_intent_replay_key(identity) {
        return None;
    }
    let legacy_identity = derive_legacy_tool_intent_v1_replay_key(identity);
    let replay_key = invocation.replay_key();
    let legacy_lookup = replay_key.replace(&identity.replay_key, &legacy_identity);
    (legacy_lookup != replay_key).then_some(legacy_lookup)
}

/// A completed leaf-provider value. Unlike [`crate::ToolOutcome`], this type has
/// no deferred variant, so a completed result can be paired with intents
/// without making `Pending + intents` representable.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcomeDone(Box<crate::ToolCallOutput>);

impl ToolOutcomeDone {
    /// Wrap a completed tool output for protocol and process-engine implementors.
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self(Box::new(output))
    }

    /// Construct a successful completed value for protocol and process-engine implementors.
    pub fn ok(result: serde_json::Value) -> Self {
        Self::from_output(crate::ToolCallOutput::success(result))
    }

    /// Construct a failed completed value for protocol and process-engine implementors.
    pub fn failure(failure: crate::ToolFailure) -> Self {
        Self::from_output(crate::ToolCallOutput::failure(failure))
    }

    /// Recover the terminal output for protocol and process-engine implementors.
    pub fn into_output(self) -> crate::ToolCallOutput {
        *self.0
    }
}

/// Value returned by a leaf provider body before the attempt effect records it.
/// The enum is the law: only the completed variant has an intents field.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug)]
pub enum ToolAttemptOutcome {
    /// Terminal provider output with ordered durable declarations.
    Done {
        /// Completed provider result.
        result: ToolOutcomeDone,
        /// Follow-on declarations admitted only after the attempt is recorded.
        intents: ToolIntents,
    },
    /// Deferred provider output with no representable declarations.
    Pending(crate::PendingCompletion),
}

impl ToolAttemptOutcome {
    /// Pair a completed result with its declarations for protocol and process-engine implementors.
    pub fn done(result: ToolOutcomeDone, intents: ToolIntents) -> Self {
        Self::Done { result, intents }
    }

    /// Construct a completed attempt with no declarations for protocol and process-engine implementors.
    pub fn done_without_intents(result: ToolOutcomeDone) -> Self {
        Self::done(result, ToolIntents::default())
    }

    /// Construct a pending attempt for protocol and process-engine implementors.
    pub fn pending(pending: crate::PendingCompletion) -> Self {
        Self::Pending(pending)
    }

    pub(crate) fn from_tool_result(result: crate::ToolOutcome) -> Self {
        match result {
            crate::ToolOutcome::Done(output) => Self::done_without_intents(ToolOutcomeDone(output)),
            crate::ToolOutcome::Pending(pending) => Self::Pending(*pending),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_intent(kind: ToolIntentKind) -> ToolIntent {
        let session_id = SessionId::from("session");
        // An exhaustive match, so a variant added to the generated kind set
        // without a matching `ToolIntent` payload fails to compile here.
        match kind {
            ToolIntentKind::StartProcess => {
                ToolIntent::StartProcess(Box::new(StartProcessIntent {
                    session_id,
                    declaration: crate::ProcessStartDeclaration::external(
                        crate::ProcessOriginator::host(),
                        serde_json::Value::Null,
                        crate::ProcessLifecyclePolicy::new(
                            crate::ParentScope::Host,
                            crate::OnParentEnd::Abandon,
                        ),
                    ),
                }))
            }
            ToolIntentKind::SignalProcess => ToolIntent::SignalProcess(SignalProcessIntent {
                session_id,
                process_id: ProcessId::from("process"),
                signal_name: "go".to_string(),
                payload: serde_json::Value::Null,
            }),
            ToolIntentKind::CancelProcess => ToolIntent::CancelProcess(CancelProcessIntent {
                session_id,
                process_id: ProcessId::from("process"),
            }),
            ToolIntentKind::EmitProcessEvent => {
                ToolIntent::EmitProcessEvent(EmitProcessEventIntent {
                    session_id,
                    process_id: ProcessId::from("process"),
                    event_type: "note".to_string(),
                    payload: serde_json::Value::Null,
                })
            }
            ToolIntentKind::EmitTrigger => ToolIntent::EmitTrigger(EmitTriggerIntent {
                session_id,
                request: crate::TriggerOccurrenceRequest::new(
                    "source",
                    "source-key",
                    serde_json::Value::Null,
                    "idempotency-key",
                ),
            }),
            ToolIntentKind::RegisterProcessDefinition => {
                ToolIntent::RegisterProcessDefinition(Box::new(RegisterProcessDefinitionIntent {
                    session_id,
                    engine_kind: "engine".to_string(),
                    definition: serde_json::Value::Null,
                    env_spec: None,
                    label: None,
                }))
            }
            ToolIntentKind::RegisterTrigger => {
                ToolIntent::RegisterTrigger(Box::new(RegisterTriggerIntent {
                    session_id,
                    draft: crate::TriggerSubscriptionDraft::for_process(
                        "subscription",
                        crate::ProcessExecutionEnvRef::new("env-ref"),
                        "source",
                        "source-key",
                        crate::ProcessInput::External {
                            metadata: serde_json::Value::Null,
                        },
                        crate::ProcessIdentity::new("engine"),
                    ),
                }))
            }
        }
    }

    /// `ToolIntentKind` is generated from the same variant list as `ToolIntent`
    /// rather than mirrored by hand: every generated kind is produced by a real
    /// declaration, the projection is injective, and the wire spelling the kind
    /// prints is the tag serde writes for the declaration (FIG-2994).
    #[test]
    fn every_generated_kind_is_produced_by_exactly_one_tool_intent_variant() {
        let mut seen = std::collections::HashSet::new();
        for kind in ToolIntentKind::ALL.iter().copied() {
            let intent = sample_intent(kind);
            assert_eq!(intent.kind(), kind, "kind projection is not the identity");
            assert!(seen.insert(kind), "two samples claim the same kind");
            let encoded = serde_json::to_value(&intent).expect("declarations encode");
            assert_eq!(
                encoded.get("kind").and_then(serde_json::Value::as_str),
                Some(kind.as_str()),
                "the serde tag and `as_str` disagree for {kind:?}"
            );
        }
        assert_eq!(
            seen.len(),
            ToolIntentKind::ALL.len(),
            "the generated kind set and the declaration set have different sizes"
        );
    }

    /// The new declarations of FIG-2994 are members of the protocol, not just
    /// types: they carry a session binding the batch admitter can check.
    #[test]
    fn the_registration_declarations_carry_their_session_authority() {
        for kind in [
            ToolIntentKind::RegisterProcessDefinition,
            ToolIntentKind::RegisterTrigger,
        ] {
            assert_eq!(sample_intent(kind).session_id(), "session");
        }
    }

    #[test]
    fn intent_identity_has_a_literal_stable_oracle() {
        let identity = derive_tool_intent_identity(
            &SessionId::from("session-fig1292"),
            "turn-7",
            Some("call-3"),
            2,
        )
        .expect("identity");
        assert_eq!(
            identity,
            ToolIntentIdentity {
                session_id: SessionId::from("session-fig1292"),
                execution_scope_id: "turn-7".to_string(),
                tool_call_id: "call-3".to_string(),
                intent_index: 2,
                replay_key: "tool-intent:v2:blake3:11066b1512aa6d126c635d3b3dde273ad9e3dfeeea7ae985894652309c7da31c".to_string(),
                minting_emission_replay_key: None,
            }
        );
    }

    #[test]
    fn emitted_intent_identity_is_scoped_by_the_minting_replay_key() {
        let first = derive_tool_intent_identity_inner(
            &SessionId::from("session"),
            "process",
            Some("call"),
            0,
            Some("turn:7:child:0:call:attempt:1"),
        )
        .expect("first emission identity");
        let second = derive_tool_intent_identity_inner(
            &SessionId::from("session"),
            "process",
            Some("call"),
            0,
            Some("turn:8:child:0:call:attempt:1"),
        )
        .expect("second emission identity");

        assert!(first.replay_key.starts_with("tool-intent:v2:blake3:"));
        assert!(second.replay_key.starts_with("tool-intent:v2:blake3:"));
        assert_ne!(first.replay_key, second.replay_key);
    }

    #[test]
    fn missing_call_id_is_a_typed_refusal() {
        assert_eq!(
            derive_tool_intent_identity(&SessionId::from("session"), "turn", None, 0),
            Err(ToolIntentRefusalReason::MissingToolCallId)
        );
    }

    #[test]
    fn intent_identity_is_distinct_across_turn_and_process_execution_scopes() {
        let turn_7 =
            derive_tool_intent_identity(&SessionId::from("session"), "turn-7", Some("call"), 0)
                .expect("turn 7 identity");
        let turn_8 =
            derive_tool_intent_identity(&SessionId::from("session"), "turn-8", Some("call"), 0)
                .expect("turn 8 identity");
        let process =
            derive_tool_intent_identity(&SessionId::from("session"), "process-7", Some("call"), 0)
                .expect("process identity");
        assert_eq!(turn_7.execution_scope_id, "turn-7");
        assert_eq!(turn_8.execution_scope_id, "turn-8");
        assert_eq!(process.execution_scope_id, "process-7");
        assert_ne!(turn_7.replay_key, turn_8.replay_key);
        assert_ne!(turn_7.replay_key, process.replay_key);
        assert_ne!(turn_8.replay_key, process.replay_key);
    }

    #[test]
    fn start_process_intent_without_a_lifecycle_policy_is_refused() {
        let declaration = crate::ProcessStartDeclaration::external(
            crate::ProcessOriginator::host(),
            serde_json::Value::Null,
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        );
        let mut payload = serde_json::to_value(declaration).expect("serialize start declaration");
        assert!(
            payload
                .as_object_mut()
                .expect("declaration object")
                .remove("lifecycle")
                .is_some()
        );
        let error = serde_json::from_value::<StartProcessIntent>(serde_json::json!({
            "session_id": "session", "declaration": payload,
        }))
        .expect_err("lifecycle absence must be refused");
        assert!(error.to_string().contains("lifecycle"));
    }
}
