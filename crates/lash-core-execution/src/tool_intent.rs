/// version_surface = "coexist"
/// version_guard(items(LASH_TOOL_INTENT_PAYLOAD_DOMAIN_VERSION, new))
const LASH_TOOL_INTENT_PAYLOAD_DOMAIN_VERSION: &str = "lash-tool-intent-payload/v3";

use crate::ProcessId;
use crate::RuntimeOwner;
use crate::{ToolIntentIdentity, ToolIntentKind};
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
/// version_surface = "coexist"
/// version_guard(roots(ToolIntents, ToolIntentSubmissionRecord))
pub const TOOL_INTENT_PROTOCOL_V3: u16 = 3;
pub const TOOL_INTENT_MAX_COUNT: usize = 32;
/// Maximum canonical JSON bytes one recorded intent batch may declare.
/// Captured environments are stored separately; their digest references count.
/// **Integrator class 3: protocol and process-engine implementors.**
pub const TOOL_INTENT_MAX_CANONICAL_BYTES: usize = 64 * 1024;
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
    pub fn v3(intents: Vec<ToolIntent>) -> Self {
        Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V3,
            intents,
        }
    }

    /// This is an **integrator class 3: protocol and process-engine implementor** seam.
    pub fn is_empty(&self) -> bool {
        self.intents.is_empty()
    }

    /// The canonical JSON size of the complete declaration batch.
    pub fn declared_canonical_bytes(&self) -> Result<usize, serde_json::Error> {
        serde_json::to_vec(self).map(|bytes| bytes.len())
    }
}

impl ToolIntent {
    /// Environments the declaration needs, without their stored bytes.
    pub fn execution_env_ref(&self) -> Option<&crate::ProcessExecutionEnvRef> {
        match self {
            Self::StartProcess(start) => start.declaration.env_ref.as_ref(),
            Self::RegisterTrigger(registration) => Some(&registration.draft.env_ref),
            Self::PublishDefinition(_)
            | Self::GetDefinition(_)
            | Self::SignalProcess(_)
            | Self::CancelProcess(_)
            | Self::EmitProcessEvent(_)
            | Self::EmitTrigger(_) => None,
        }
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
    (GetDefinition) => { GetDefinitionIntent };
    (PublishDefinition) => { Box<PublishDefinitionIntent> };
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
            pub fn kind(&self) -> ToolIntentKind {
                match self {
                    $(Self::$variant(_) => ToolIntentKind::$variant,)*
                }
            }

            /// The runtime whose authority declared this intent.
            pub fn owner(&self) -> &RuntimeOwner {
                match self {
                    $(Self::$variant(intent) => &intent.owner,)*
                }
            }
        }
    };
}

lash_sansio::tool_intent_variants!(define_tool_intent);

/// Durable submission-ledger row for one host-submitted tool-intent identity.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam. Process registries persist it so independent facade handles and
/// crash redrives see the same first writer: a cancel binds its target here
/// before realization, and every submission retains its first outcome here.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    pub completed_at_ms: Option<u64>,
    /// The submission's trace scope: the cause and anchor its first
    /// submission offered and when it was made. The ledger's first writer
    /// retains it with the row; a later submission of the identity reads it
    /// back. It is beside the intent, never inside it, so the payload hash
    /// does not cover it. `None` for a runtime-minted intent, which runs
    /// under its tool call's scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<lash_trace::DurableTraceScope>,
}

impl ToolIntentSubmissionRecord {
    /// Builds the canonical first-submission row for protocol and
    /// process-engine implementors before any intent realization occurs.
    pub fn new(
        identity: ToolIntentIdentity,
        intent: ToolIntent,
    ) -> Result<Self, serde_json::Error> {
        let kind = intent.kind();
        // The hash is the intent's business identity: an occurrence an
        // emission carries is hashed without the trace offer beside it.
        let payload_hash = crate::stable_hash::blake3_hex(
            LASH_TOOL_INTENT_PAYLOAD_DOMAIN_VERSION,
            &match intent.without_trace_provenance() {
                Some(business) => serde_json::to_vec(&business)?,
                None => serde_json::to_vec(&intent)?,
            },
        );
        Ok(Self {
            protocol_version: TOOL_INTENT_PROTOCOL_V3,
            identity,
            kind,
            payload_hash,
            intent,
            outcome: None,
            completed_at_ms: None,
            trace: None,
        })
    }

    /// The scope id of this submission's trace scope: its owning runtime
    /// and replay key.
    pub fn trace_scope_id(&self) -> lash_trace::TraceScopeId {
        lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::ToolIntent {
            owner: self.identity.owner.clone(),
            replay_key: self.identity.replay_key.clone(),
        })
    }

    /// Offers `offer` as a host submission's trace scope, started at
    /// `submitted_at_ms`. The ledger keeps it only when this record is the
    /// identity's first writer.
    pub fn with_trace_offer(
        mut self,
        offer: lash_trace::TraceScopeOffer,
        submitted_at_ms: u64,
    ) -> Self {
        self.trace = Some(offer.into_scope(self.trace_scope_id(), submitted_at_ms));
        self
    }
}

impl ToolIntent {
    /// This intent without the trace offer it carries, when it carries one:
    /// what its payload hash covers.
    pub fn without_trace_provenance(&self) -> Option<Self> {
        match self {
            Self::EmitTrigger(emit) if !emit.request.trace.is_empty() => {
                let mut emit = emit.clone();
                emit.request.trace = lash_trace::TraceScopeOffer::default();
                Some(Self::EmitTrigger(emit))
            }
            _ => None,
        }
    }
}

/// Atomic result of claiming a tool-intent identity in the submission ledger.
///
/// This is an **integrator class 3: protocol and process-engine implementor**
/// seam returned by [`crate::ProcessRegistry`] implementations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolIntentSubmissionAdmission {
    /// This caller durably installed the first submission.
    Admitted,
    /// Another caller or an earlier crash installed the returned first submission.
    Existing(Box<ToolIntentSubmissionRecord>),
    /// The identity's owner session was durably deleted and the retained-evidence
    /// lever reclaimed its ledger (FIG-1509). The owner's fence outlives its
    /// rows, so nothing may claim or realize an identity of that owner again.
    Reclaimed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// The declaration carries no process id and no key. Realization derives the
/// key with [`crate::StartKeyDerivation::for_tool_intent`] from this declaration's own
/// intent identity, so every redrive presents the same key and starts the
/// same process; the registrar mints the id and the realized result carries
/// it (FIG-2994, ADR 0107).
pub struct StartProcessIntent {
    /// The runtime whose authority owns the child.
    pub owner: RuntimeOwner,
    /// Durable process-start declaration, minus the derived key.
    pub declaration: crate::ProcessStartDeclaration,
}

impl StartProcessIntent {
    /// Shared admission for completed intent batches and pending declared starts.
    pub(crate) fn admission_refusal(&self) -> Option<crate::ToolIntentRefusalReason> {
        (matches!(
            self.declaration.input,
            crate::ProcessStartTarget::Input(crate::ProcessInput::SessionTurn { .. })
        ) && self.declaration.env_ref.is_none())
        .then_some(crate::ToolIntentRefusalReason::ExecutionEnvMissing)
    }

    /// Both realization routes call this and nothing else, so every redrive
    /// of one declaration presents the same start key (ADR 0107).
    pub fn into_request(&self, identity: &ToolIntentIdentity) -> crate::ProcessStartRequest {
        self.declaration
            .clone()
            .into_request(crate::StartKeyDerivation::LASH_START_PATHS.for_tool_intent(identity))
    }
}

/// Publication of an immutable descriptor after the attempt commits.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishDefinitionIntent {
    pub owner: RuntimeOwner,
    pub draft: crate::ProcessDefinitionDraft,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<DeclaredModuleArtifact>,
}

/// Acquire a definition under the realizing execution before returning it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetDefinitionIntent {
    pub owner: RuntimeOwner,
    pub definition_id: crate::ProcessDefinitionId,
}

/// A module artifact a declaration carries for realization to publish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredModuleArtifact {
    /// The content-addressed reference the definition value names.
    pub module_ref: String,
    /// The module port's bytes for `module_ref`, exactly as its codec wrote
    /// them. The lashlang module codec is JSON, so they travel as text and
    /// publish byte-for-byte.
    pub bytes: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Distinct from [`EmitTriggerIntent`], which fires an occurrence: this one
/// installs the subscription. A leaf attempt cannot register synchronously for
/// the same reason it cannot emit synchronously — a subscription that outlived
/// a failed attempt would wake a target the attempt never committed.
#[serde(deny_unknown_fields)]
pub struct RegisterTriggerIntent {
    /// The runtime whose authority owns the subscription.
    pub owner: RuntimeOwner,
    /// The registrant scope the declaring attempt resolved, exactly as the
    /// retired host-operation path resolved it from the live context.
    pub owner_scope: crate::TriggerOwnerScope,
    /// The actor the declaring attempt resolved for the registration.
    pub actor: crate::ProcessOriginator,
    pub draft: crate::TriggerSubscriptionDraft,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Signal declaration consumed by protocol and process-engine implementors.
pub struct SignalProcessIntent {
    /// The runtime whose authority owns the signal.
    pub owner: RuntimeOwner,
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
    /// The runtime whose authority owns the cancellation.
    pub owner: RuntimeOwner,
    /// Target process id.
    pub process_id: ProcessId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// Event declaration consumed by protocol and process-engine implementors.
pub struct EmitProcessEventIntent {
    /// The runtime whose authority owns the append.
    pub owner: RuntimeOwner,
    /// Target process id.
    pub process_id: ProcessId,
    /// Registered event type.
    pub event_type: String,
    /// Event payload validated by the process registry.
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
/// - `owner` here is the authority the intent executor validates the
///   declaration against; `request.session_id` is the occurrence's own routing
///   scope, which the router carries onto the occurrence record and never
///   checks against it.
pub struct EmitTriggerIntent {
    /// The runtime whose authority owns the emission. Validated: a
    /// declaration naming another owner is refused before it reaches the
    /// router.
    pub owner: RuntimeOwner,
    /// Complete durable trigger-occurrence request. At realization the router
    /// replaces its caller-supplied `idempotency_key` with the declaration
    /// replay key. Its own `session_id` is the occurrence's routing scope, not
    /// an authority.
    pub request: crate::TriggerOccurrenceRequest,
}

/// version_surface = "coexist"
/// version_guard(items(TOOL_INTENT_IDENTITY_FAMILY_VERSION, derive_tool_intent_identity_inner))
const TOOL_INTENT_IDENTITY_FAMILY_VERSION: u8 = 2;

/// The public identity seam for host-submitted intents.
///
/// Runtime-minted declarations use the same v2 family through
/// [`derive_tool_intent_identity_under`], which additionally binds the identity
/// to the durable invocation that minted the declaration.
pub fn derive_tool_intent_identity(
    owner: &RuntimeOwner,
    execution_scope_id: &str,
    tool_call_id: &crate::ToolCallId,
    intent_index: u32,
) -> ToolIntentIdentity {
    derive_tool_intent_identity_inner(owner, execution_scope_id, tool_call_id, intent_index, None)
}

/// The one derivation every runtime-side declaration site uses.
///
/// An intent is named by the call that declared it and its index among the
/// call's intents (ADR 0117 §6). A declaration minted under a durable
/// invocation also binds that invocation's replay key, the final-emission
/// attribution that fences it; one minted outside any invocation binds
/// nothing. Both the attempt's own `AttemptContext::intent_identity` and the
/// intent executor's realization pass their parent invocation here, so the
/// identity an attempt reports and the identity the executor realizes under
/// cannot be derived by two rules (FIG-2994).
pub fn derive_tool_intent_identity_under(
    owner: &RuntimeOwner,
    execution_scope_id: &str,
    tool_call_id: &crate::ToolCallId,
    intent_index: u32,
    parent_invocation: Option<&crate::RuntimeInvocation>,
) -> ToolIntentIdentity {
    derive_tool_intent_identity_inner(
        owner,
        execution_scope_id,
        tool_call_id,
        intent_index,
        parent_invocation.and_then(crate::RuntimeInvocation::effect_replay_key),
    )
}

fn derive_tool_intent_identity_inner(
    owner: &RuntimeOwner,
    execution_scope_id: &str,
    tool_call_id: &crate::ToolCallId,
    intent_index: u32,
    minting_emission_replay_key: Option<&str>,
) -> ToolIntentIdentity {
    let mut encoder = crate::stable_identity::IdentityEncoder::new(
        "lash.tool-intent",
        TOOL_INTENT_IDENTITY_FAMILY_VERSION,
    );
    encode_owner(&mut encoder, owner);
    encoder.string(execution_scope_id);
    encoder.string(tool_call_id.as_str());
    encoder.u32(intent_index);
    encoder.optional(minting_emission_replay_key, |encoder, replay_key| {
        encoder.string(replay_key)
    });
    let replay_key = crate::stable_identity::rendered_hash(
        "tool-intent",
        TOOL_INTENT_IDENTITY_FAMILY_VERSION,
        &encoder.finish(),
    );
    ToolIntentIdentity {
        owner: owner.clone(),
        execution_scope_id: execution_scope_id.to_string(),
        tool_call_id: tool_call_id.clone(),
        intent_index,
        replay_key,
        minting_emission_replay_key: minting_emission_replay_key.map(str::to_string),
    }
}

/// Re-derive an identity from its own durable fields.
///
/// The v2 replay key hashes the minting-emission replay key, so the record
/// retains that input; a record whose `replay_key` does not equal the
/// re-derived one carries a forged or corrupted identity.
pub fn rederive_tool_intent_identity(identity: &ToolIntentIdentity) -> ToolIntentIdentity {
    derive_tool_intent_identity_inner(
        &identity.owner,
        &identity.execution_scope_id,
        &identity.tool_call_id,
        identity.intent_index,
        identity.minting_emission_replay_key.as_deref(),
    )
}

/// The owner's kind, then its id: a session named like a process id can
/// never share an identity with that process.
fn encode_owner(encoder: &mut crate::stable_identity::IdentityEncoder, owner: &RuntimeOwner) {
    match owner {
        RuntimeOwner::Session(session_id) => {
            encoder.string("session");
            encoder.string(session_id);
        }
        RuntimeOwner::Process(process_id) => {
            encoder.string("process");
            encoder.string(process_id.as_str());
        }
    }
}

/// A completed leaf-provider value. Unlike [`crate::ToolOutcome`], this type has
/// no deferred variant, so a completed result can be paired with intents
/// without making `Pending + intents` representable.
/// **Integrator class 3: protocol and process-engine implementors.**
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcomeDone(Box<crate::ToolCallOutput>);

impl ToolOutcomeDone {
    pub fn from_output(output: crate::ToolCallOutput) -> Self {
        Self(Box::new(output))
    }

    pub fn ok(result: serde_json::Value) -> Self {
        Self::from_output(crate::ToolCallOutput::success(result))
    }

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
    /// A host fault that aborts the attempt before it has a recorded result.
    /// Its journal disposition determines whether the engine may retry it.
    HostFailed(Box<crate::RuntimeEffectControllerError>),
    /// Deferred provider output with no representable declarations.
    Pending(crate::PendingCompletion),
}

impl ToolAttemptOutcome {
    /// Pair a completed result with its declarations for protocol and process-engine implementors.
    pub fn done(result: ToolOutcomeDone, intents: ToolIntents) -> Self {
        Self::Done { result, intents }
    }

    pub fn done_without_intents(result: ToolOutcomeDone) -> Self {
        Self::done(result, ToolIntents::default())
    }

    pub fn host_failed(error: crate::RuntimeEffectControllerError) -> Self {
        Self::HostFailed(Box::new(error))
    }

    pub fn pending(pending: crate::PendingCompletion) -> Self {
        Self::Pending(pending)
    }
}

/// Lift a plain tool outcome into an attempt outcome at the execution seam.
/// A completed outcome becomes [`ToolAttemptOutcome::Done`] with no declared
/// intents; a pending outcome stays [`ToolAttemptOutcome::Pending`] with its
/// completion payload intact. There is deliberately no conversion in the
/// other direction: projecting away `Done` intents would silently drop
/// durable declarations.
impl From<crate::ToolOutcome> for ToolAttemptOutcome {
    fn from(result: crate::ToolOutcome) -> Self {
        match result {
            crate::ToolOutcome::Done(output) => Self::done_without_intents(ToolOutcomeDone(output)),
            crate::ToolOutcome::Pending(pending) => Self::Pending(*pending),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionId;

    fn sample_intent(kind: ToolIntentKind) -> ToolIntent {
        let session_id = SessionId::from("session");
        // An exhaustive match, so a variant added to the generated kind set
        // without a matching `ToolIntent` payload fails to compile here.
        match kind {
            ToolIntentKind::StartProcess => {
                ToolIntent::StartProcess(Box::new(StartProcessIntent {
                    owner: crate::RuntimeOwner::Session(session_id),
                    declaration: crate::ProcessStartDeclaration::external(
                        crate::ProcessOriginator::host(),
                        serde_json::Value::Null,
                        crate::Lifetime::Detached,
                    ),
                }))
            }
            ToolIntentKind::SignalProcess => ToolIntent::SignalProcess(SignalProcessIntent {
                owner: crate::RuntimeOwner::Session(session_id),
                process_id: crate::process_id_for_test("process"),
                signal_name: "go".to_string(),
                payload: serde_json::Value::Null,
            }),
            ToolIntentKind::CancelProcess => ToolIntent::CancelProcess(CancelProcessIntent {
                owner: crate::RuntimeOwner::Session(session_id),
                process_id: crate::process_id_for_test("process"),
            }),
            ToolIntentKind::EmitProcessEvent => {
                ToolIntent::EmitProcessEvent(EmitProcessEventIntent {
                    owner: crate::RuntimeOwner::Session(session_id),
                    process_id: crate::process_id_for_test("process"),
                    event_type: "note".to_string(),
                    payload: serde_json::Value::Null,
                })
            }
            ToolIntentKind::EmitTrigger => ToolIntent::EmitTrigger(EmitTriggerIntent {
                owner: crate::RuntimeOwner::Session(session_id),
                request: crate::TriggerOccurrenceRequest::new(
                    "source",
                    "source-key",
                    serde_json::Value::Null,
                    "idempotency-key",
                ),
            }),
            ToolIntentKind::GetDefinition => ToolIntent::GetDefinition(GetDefinitionIntent {
                owner: RuntimeOwner::Session(session_id),
                definition_id: crate::ProcessDefinitionId::from_sha256_digest([0; 32]),
            }),
            ToolIntentKind::PublishDefinition => {
                ToolIntent::PublishDefinition(Box::new(PublishDefinitionIntent {
                    owner: RuntimeOwner::Session(session_id),
                    draft: crate::ProcessDefinitionDraft::new(
                        "engine",
                        serde_json::Value::Null,
                        [],
                    )
                    .expect("draft"),
                    module: None,
                }))
            }
            ToolIntentKind::RegisterTrigger => {
                ToolIntent::RegisterTrigger(Box::new(RegisterTriggerIntent {
                    owner_scope: crate::TriggerOwnerScope::session(session_id.clone()),
                    actor: crate::ProcessOriginator::session(crate::SessionScope::new(
                        session_id.clone(),
                    )),
                    owner: RuntimeOwner::Session(session_id),
                    draft: crate::TriggerSubscriptionDraft::for_process(
                        "subscription",
                        crate::ProcessExecutionEnvRef::new("env-ref"),
                        "source",
                        "source-key",
                        crate::ProcessInput::Engine {
                            kind: "engine".to_string(),
                            payload: serde_json::Value::Null,
                        },
                        crate::ProcessIdentity::new("engine"),
                    ),
                }))
            }
        }
    }

    #[test]
    fn intent_identity_has_a_literal_stable_oracle() {
        let identity = derive_tool_intent_identity(
            &crate::RuntimeOwner::Session(SessionId::from("session-fig1292")),
            "turn-7",
            &crate::ToolCallId::fixture("call-3"),
            2,
        );
        assert_eq!(
            identity,
            ToolIntentIdentity {
                owner: RuntimeOwner::Session(SessionId::from("session-fig1292")),
                execution_scope_id: "turn-7".to_string(),
                tool_call_id: crate::ToolCallId::fixture("call-3"),
                intent_index: 2,
                replay_key: "tool-intent:v2:blake3:cbd4d128fc11551063487aa7d35bf25457d31f6d9acc95ec90996e74cd102a9d".to_string(),
                minting_emission_replay_key: None,
            }
        );
    }

    /// The protocol discriminator is part of the row: a submission that
    /// names none is not a row of any protocol and does not decode.
    #[test]
    fn a_submission_row_without_its_protocol_version_fails_decode() {
        let intent = sample_intent(ToolIntentKind::CancelProcess);
        let identity = derive_tool_intent_identity(
            intent.owner(),
            "turn-7",
            &crate::ToolCallId::fixture("call"),
            0,
        );
        let record = ToolIntentSubmissionRecord::new(identity, intent).expect("build the row");
        let mut row = serde_json::to_value(&record).expect("encode the row");
        let decoded: ToolIntentSubmissionRecord =
            serde_json::from_value(row.clone()).expect("the stamped row decodes");
        assert_eq!(decoded.protocol_version, TOOL_INTENT_PROTOCOL_V3);

        row.as_object_mut()
            .expect("the row is an object")
            .remove("protocol_version")
            .expect("the row is stamped");
        let error = serde_json::from_value::<ToolIntentSubmissionRecord>(row)
            .expect_err("an unstamped row must not decode");
        assert!(
            error
                .to_string()
                .contains("missing field `protocol_version`"),
            "{error}"
        );
    }

    #[test]
    fn start_process_intent_without_a_lifetime_is_refused() {
        let declaration = crate::ProcessStartDeclaration::external(
            crate::ProcessOriginator::host(),
            serde_json::Value::Null,
            crate::Lifetime::Detached,
        );
        let mut payload = serde_json::to_value(declaration).expect("serialize start declaration");
        assert!(
            payload
                .as_object_mut()
                .expect("declaration object")
                .remove("lifetime")
                .is_some()
        );
        let error = serde_json::from_value::<StartProcessIntent>(serde_json::json!({
            "session_id": "session", "declaration": payload,
        }))
        .expect_err("lifetime absence must be refused");
        assert!(error.to_string().contains("lifetime"));
    }

    /// Every input the replay key is derived from, as a generated tuple.
    ///
    /// The fields are drawn from small alphabets on purpose: injectivity is
    /// only interesting where collisions are *possible*, and a generator over
    /// unconstrained strings proves that distinct 32-byte randoms hash apart
    /// rather than that adjacent scope ids do.
    fn identity_inputs()
    -> impl proptest::strategy::Strategy<Value = (String, String, String, u32, Option<String>)>
    {
        use proptest::prelude::*;
        let token = proptest::sample::select(vec!["a", "b", "ab", "a-b", "", "b-a"])
            .prop_map(str::to_string);
        // A session id is never blank, so its alphabet has no empty token.
        let session =
            proptest::sample::select(vec!["a", "b", "ab", "a-b", "b-a"]).prop_map(str::to_string);
        (
            session,
            token.clone(),
            token.clone(),
            0u32..4,
            proptest::option::of(token),
        )
    }

    fn derive_from(inputs: &(String, String, String, u32, Option<String>)) -> ToolIntentIdentity {
        let (session_id, execution_scope_id, tool_call_id, intent_index, minting) = inputs;
        derive_tool_intent_identity_inner(
            &RuntimeOwner::Session(SessionId::fixture(session_id.clone())),
            execution_scope_id,
            &crate::ToolCallId::fixture(tool_call_id),
            *intent_index,
            minting.as_deref(),
        )
    }

    proptest::proptest! {
        /// Distinct inputs derive distinct replay keys, and equal inputs derive
        /// equal ones.
        ///
        /// The replay key is the start key's preimage for a start declaration
        /// (`StartKeyDerivation::for_tool_intent`), so a collision here is two
        /// declarations realizing as one process, and a spurious difference is
        /// a re-submitted declaration starting a second one. The encoder
        /// length-prefixes each field precisely so that `("a", "b")` and
        /// `("ab", "")` cannot render to the same bytes; this executes that.
        #[test]
        fn tool_intent_identity_derivation_is_injective_in_its_inputs(
            left in identity_inputs(),
            right in identity_inputs(),
        ) {
            let derived_left = derive_from(&left);
            let derived_right = derive_from(&right);
            proptest::prop_assert_eq!(
                left == right,
                derived_left.replay_key == derived_right.replay_key,
                "inputs {:?} vs {:?} derived {} vs {}",
                left,
                right,
                derived_left.replay_key,
                derived_right.replay_key
            );
        }

    }

    #[test]
    fn a_forged_field_does_not_survive_re_derivation() {
        // The fence documented on `rederive_tool_intent_identity`: a record
        // whose `replay_key` does not equal the re-derived one carries a forged
        // identity. Nothing exercised it, so a field the derivation stopped
        // reading would have gone unnoticed -- every mutation below must move
        // the key, or that field is no longer part of the identity.
        let honest = derive_tool_intent_identity_inner(
            &crate::RuntimeOwner::Session(SessionId::from("session")),
            "scope",
            &crate::ToolCallId::fixture("call"),
            1,
            Some("minted"),
        );
        assert_eq!(
            rederive_tool_intent_identity(&honest).replay_key,
            honest.replay_key
        );

        let forgeries: Vec<(&str, ToolIntentIdentity)> = vec![
            (
                "owner",
                ToolIntentIdentity {
                    owner: RuntimeOwner::Session(SessionId::from("other")),
                    ..honest.clone()
                },
            ),
            (
                "execution_scope_id",
                ToolIntentIdentity {
                    execution_scope_id: "other".to_string(),
                    ..honest.clone()
                },
            ),
            (
                "tool_call_id",
                ToolIntentIdentity {
                    tool_call_id: crate::ToolCallId::fixture("other"),
                    ..honest.clone()
                },
            ),
            (
                "intent_index",
                ToolIntentIdentity {
                    intent_index: 2,
                    ..honest.clone()
                },
            ),
            (
                "minting_emission_replay_key",
                ToolIntentIdentity {
                    minting_emission_replay_key: Some("other".to_string()),
                    ..honest.clone()
                },
            ),
            (
                "minting_emission_replay_key absence",
                ToolIntentIdentity {
                    minting_emission_replay_key: None,
                    ..honest.clone()
                },
            ),
        ];
        for (field, forged) in forgeries {
            let rederived = rederive_tool_intent_identity(&forged);
            assert_ne!(
                rederived.replay_key, forged.replay_key,
                "a forged `{field}` must not re-derive to the key the record carries"
            );
        }
    }
}
