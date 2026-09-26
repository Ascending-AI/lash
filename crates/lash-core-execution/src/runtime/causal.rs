use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use crate::sansio::EffectId;
use crate::{
    CausalRef, EffectAddress, ExecutionScope, RuntimeAttribution, RuntimeEffectInvocation,
    RuntimeEffectKind, RuntimeInvocation, RuntimeReplay, RuntimeSubject,
};

#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn turn_effect_invocation(
    execution_scope: &ExecutionScope,
    session_id: &SessionId,
    turn_id: &TurnId,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: EffectId,
    effect_kind: RuntimeEffectKind,
) -> RuntimeEffectInvocation {
    // Session and turn describe the work for traces and replay-key stability;
    // the scoped controller is the authority that owns the effect address.
    let replay_key = turn_effect_replay_key(
        session_id,
        turn_id,
        turn_index,
        protocol_iteration,
        effect_kind.as_str(),
        effect_id,
    );
    RuntimeEffectInvocation::new(
        EffectAddress::new(execution_scope.clone(), replay_key)
            .expect("turn effect uses the already admitted controller scope"),
        RuntimeAttribution::for_turn(session_id, turn_id, turn_index, protocol_iteration),
        effect_id.0.to_string(),
    )
}

/// Invocation for the durable acceptance of a turn input (ADR 0069 §6).
///
/// Acceptance runs before the turn does, so it has no protocol iteration to
/// name and cannot use [`turn_effect_invocation`]. Its replay key is a function
/// of the session and the execution scope alone — both of which a replaying
/// engine reconstructs identically — so a redriven handler journals the same
/// entry and re-derives the admission instead of admitting a second turn.
/// Nor does it name a turn index: the drive that admits the turn fixes that,
/// and anything read before it would come from the live head (FIG-3682).
#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn turn_acceptance_effect_invocation(
    execution_scope: &ExecutionScope,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> RuntimeEffectInvocation {
    // A process-backed turn still has truthful session/turn attribution, but
    // its acceptance journal entry belongs to the admitted process scope.
    let replay_key = format!(
        "{session_id}:{turn_id}:{}",
        RuntimeEffectKind::AcceptTurnInput.as_str()
    );
    RuntimeEffectInvocation::new(
        EffectAddress::new(execution_scope.clone(), replay_key)
            .expect("turn acceptance uses the already admitted controller scope"),
        RuntimeAttribution::for_turn_admission(session_id, turn_id),
        format!("{turn_id}.accept"),
    )
}

/// Invocation for the journaled initial drive set of an accepted turn input
/// (ADR 0069 §6).
///
/// The drive is a child of the acceptance it claims: same execution scope and
/// attribution, a replay key derived from the acceptance's, and a causal edge
/// back to it. Nothing about the lease that performs the claim enters the
/// identity, so every lease generation replays the same entry.
pub fn turn_input_drive_effect_invocation(
    acceptance: &RuntimeEffectInvocation,
) -> RuntimeEffectInvocation {
    let kind = RuntimeEffectKind::ClaimAcceptedTurnInput.as_str();
    child_effect_invocation_from_effect(
        acceptance.execution_scope(),
        acceptance,
        format!("{}.{kind}", acceptance.effect_id()),
        kind,
    )
}

/// Invocation for a later phase of a staged turn effect (FIG-1276).
///
/// The phase carries its own effect id — `<id>.<kind>` — so its journal entry,
/// replay key, and causal reference stay distinct from the phase that minted
/// its input while remaining a deterministic function of that phase's identity.
///
/// It is built as a *child* of the minting phase rather than as a sibling
/// effect: the two halves are one boundary, and reading the journal has to be
/// able to say which completion a derivation derives from. A bare turn-scoped
/// invocation would leave that edge inferable only from the shared effect-id
/// prefix, which is a naming coincidence, not a recorded fact.
pub fn turn_phase_effect_invocation(
    execution_scope: &ExecutionScope,
    parent: &RuntimeEffectInvocation,
    effect_id: EffectId,
    phase_kind: RuntimeEffectKind,
) -> RuntimeEffectInvocation {
    child_effect_invocation_from_effect(
        execution_scope,
        parent,
        format!("{}.{}", effect_id.0, phase_kind.as_str()),
        phase_kind.as_str(),
    )
}

/// The replay-key segment of a turn's tool-child group invocation.
///
/// A group invocation is never an envelope, so it has no
/// [`RuntimeEffectKind`]; it is key material only. The segment keeps the bytes
/// the turn's tool calls have been keyed under since before they formed groups:
/// the group key, every child's `ToolChildRequest` attempt identity and every
/// attempt envelope hash derive from this key, so renaming the segment would
/// move each of those hashes for no change in meaning (ADR 0099 §3).
const TURN_TOOL_GROUP_KEY_SEGMENT: &str = "tool_batch";

/// Invocation a standard-protocol turn opens its tool-child effect group under
/// (ADR 0099 §3).
///
/// The replay key carries the physical turn and protocol iteration, so a
/// follow-on agent frame whose sansio effect ids restart does not name the
/// root frame's group.
#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn turn_tool_group_invocation(
    execution_scope: &ExecutionScope,
    session_id: &SessionId,
    turn_id: &TurnId,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: EffectId,
) -> RuntimeEffectInvocation {
    let replay_key = turn_effect_replay_key(
        session_id,
        turn_id,
        turn_index,
        protocol_iteration,
        TURN_TOOL_GROUP_KEY_SEGMENT,
        effect_id,
    );
    RuntimeEffectInvocation::new(
        EffectAddress::new(execution_scope.clone(), replay_key)
            .expect("turn tool group uses the already admitted controller scope"),
        RuntimeAttribution::for_turn(session_id, turn_id, turn_index, protocol_iteration),
        effect_id.0.to_string(),
    )
}

fn turn_effect_replay_key(
    session_id: &SessionId,
    turn_id: &TurnId,
    turn_index: usize,
    protocol_iteration: usize,
    segment: &str,
    effect_id: EffectId,
) -> String {
    format!(
        "{session_id}:{turn_id}:{turn_index}:{protocol_iteration}:{segment}:{}",
        effect_id.0
    )
}

#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn child_effect_invocation(
    execution_scope: &ExecutionScope,
    parent: &RuntimeInvocation,
    effect_id: impl Into<String>,
    replay_suffix: impl AsRef<str>,
) -> RuntimeEffectInvocation {
    let replay_base = parent
        .replay_key()
        .or_else(|| parent.effect_id())
        .unwrap_or("effect");
    RuntimeEffectInvocation {
        address: EffectAddress::new(
            execution_scope.clone(),
            format!("{replay_base}:{}", replay_suffix.as_ref()),
        )
        .expect("child effect uses the already admitted controller scope"),
        attribution: parent.attribution.clone(),
        effect_id: effect_id.into(),
        caused_by: parent.causal_ref(),
        replay_attribution: parent.replay_attribution().cloned(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn child_effect_invocation_from_effect(
    execution_scope: &ExecutionScope,
    parent: &RuntimeEffectInvocation,
    effect_id: impl Into<String>,
    replay_suffix: impl AsRef<str>,
) -> RuntimeEffectInvocation {
    RuntimeEffectInvocation {
        address: EffectAddress::new(
            execution_scope.clone(),
            format!("{}:{}", parent.replay_key(), replay_suffix.as_ref()),
        )
        .expect("child effect uses the already admitted controller scope"),
        attribution: parent.attribution.clone(),
        effect_id: effect_id.into(),
        caused_by: Some(parent.causal_ref()),
        replay_attribution: parent.replay_attribution.clone(),
    }
}

/// The replay address of one command a replayed language program issued
/// (FIG-3586).
///
/// A language runtime that re-executes its program on replay (a lashlang cell,
/// a lashlang process body) mints one of these per command, from the dense
/// issue ordinal of that command within the run, and every journal row the
/// command writes lives under it. Core derives the command's sub-keys from it
/// — tool attempts, retry sleeps, a deferred completion's await, an
/// aggregate's group and children, a sleep — and never reads its structure:
/// the grammar that spells the prefix is the language runtime's, versioned
/// there (`LASHLANG_REPLAY_KEY_GRAMMAR_VERSION`).
///
/// Nothing a compiler produces may appear in it. That is the property the
/// type exists for: a redrive on a build whose lowering differs reaches the
/// same command at the same address as long as it issues the same commands in
/// the same order.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandReplayKey(String);

impl CommandReplayKey {
    /// Wraps a key a language runtime minted.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The command's own replay key: a runtime value's or a trigger
    /// operation's row, and an aggregate's group key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The row a foreground or process sleep journals its intent under.
    pub fn sleep(&self) -> String {
        format!("{}:sleep", self.0)
    }

    /// The row an aggregate's timers record their shared admission instant
    /// under (ADR 0099 §11 clause 4).
    pub fn timers_admitted(&self) -> String {
        format!("{}:timers-admitted", self.0)
    }

    /// The replay suffix of an aggregate's tool leaf at `leaf` — its index in
    /// first-appearance order — under the group's invocation.
    pub fn child_suffix(leaf: usize) -> String {
        format!("child:{leaf}")
    }

    /// The row a process-signal wait journals its await under.
    pub fn signal(&self) -> String {
        format!("{}:signal", self.0)
    }

    /// Unwraps the key.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for CommandReplayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The invocation a command's nested effects descend from: an effect subject
/// at the command's replay key, caused by the execution that issued it.
///
/// It is never itself journaled — like the code cell that issues it (ADR
/// 0103) — so it is lineage and key material only: every sub-effect a
/// command writes is a [`child_effect_invocation`] of it, which puts the
/// sub-effect's row under the command's key.
#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn command_invocation(
    execution_scope: &ExecutionScope,
    attribution: RuntimeAttribution,
    issuer: Option<&RuntimeInvocation>,
    command: &CommandReplayKey,
) -> RuntimeEffectInvocation {
    RuntimeEffectInvocation::new(
        EffectAddress::new(execution_scope.clone(), command.as_str())
            .expect("a command uses the already admitted controller scope"),
        attribution,
        command.as_str(),
    )
    .with_caused_by(issuer.and_then(RuntimeInvocation::causal_ref))
}

pub fn tool_retry_sleep_invocation(
    execution_scope: &ExecutionScope,
    parent: &RuntimeInvocation,
    tool_name: &str,
    attempt: u32,
) -> RuntimeEffectInvocation {
    let parent_effect_id = parent.effect_id().unwrap_or("effect");
    child_effect_invocation(
        execution_scope,
        parent,
        format!("{parent_effect_id}:{tool_name}:attempt:{attempt}:sleep"),
        format!("{tool_name}:attempt:{attempt}:sleep"),
    )
}

/// The operation a process sleep's durable effect-summary record carries.
pub const PROCESS_SLEEP_OPERATION: &str = "sleep";

#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn process_effect_invocation(
    execution_scope: &ExecutionScope,
    attribution: RuntimeAttribution,
    parent: Option<RuntimeInvocation>,
    effect_id: &str,
) -> RuntimeEffectInvocation {
    if let Some(parent) = parent {
        let replay_base = parent.replay_key().unwrap_or("process");
        return RuntimeEffectInvocation {
            address: EffectAddress::new(
                execution_scope.clone(),
                format!("{replay_base}:{effect_id}"),
            )
            .expect("process effect uses the already admitted controller scope"),
            attribution,
            effect_id: effect_id.to_string(),
            caused_by: parent.causal_ref(),
            replay_attribution: parent.replay_attribution().cloned(),
        };
    }
    RuntimeEffectInvocation::new(
        EffectAddress::new(execution_scope.clone(), effect_id.to_string())
            .expect("process effect uses the already admitted controller scope"),
        attribution,
        effect_id.to_string(),
    )
}

pub fn process_event_invocation(
    process_id: &ProcessId,
    sequence: u64,
    event_type: &str,
    replay: Option<RuntimeReplay>,
) -> RuntimeInvocation {
    RuntimeInvocation {
        attribution: RuntimeAttribution::none(),
        subject: RuntimeSubject::ProcessEvent {
            process_id: process_id.clone(),
            sequence,
            event_type: event_type.to_string(),
        },
        caused_by: Some(CausalRef::Process {
            process_id: process_id.clone(),
        }),
        replay,
    }
}

pub(crate) fn trigger_occurrence_invocation(
    attribution: RuntimeAttribution,
    cause: &CausalRef,
) -> RuntimeInvocation {
    let CausalRef::TriggerOccurrence {
        occurrence_id,
        subscription_id,
        subscription_incarnation,
        subscription_revision,
    } = cause
    else {
        unreachable!("trigger occurrence invocation requires a trigger cause")
    };
    RuntimeInvocation {
        attribution,
        subject: RuntimeSubject::TriggerOccurrence {
            occurrence_id: occurrence_id.to_string(),
            subscription_id: subscription_id.clone(),
            subscription_incarnation: subscription_incarnation.clone(),
            subscription_revision: *subscription_revision,
        },
        caused_by: None,
        replay: Some(RuntimeReplay {
            key: format!("trigger:{occurrence_id}"),
            attribution: None,
        }),
    }
}

#[expect(
    clippy::expect_used,
    reason = "the caller's live effect controller admitted this scope"
)]
pub fn direct_effect_invocation(
    execution_scope: &ExecutionScope,
    session_id: &SessionId,
    usage_source: &str,
    replay_discriminator: String,
    turn_id: Option<&TurnId>,
    caused_by: Option<CausalRef>,
) -> RuntimeEffectInvocation {
    let replay_preimage = direct_effect_replay_preimage(
        session_id,
        turn_id.filter(|value| !value.is_empty()),
        usage_source,
        &replay_discriminator,
    );
    let replay_key = crate::stable_identity::rendered_hash(
        "direct",
        DIRECT_EFFECT_FAMILY_VERSION,
        &replay_preimage,
    );
    RuntimeEffectInvocation::new(
        EffectAddress::new(execution_scope.clone(), replay_key)
            .expect("direct effect uses the already admitted controller scope"),
        RuntimeAttribution {
            session_id: Some(SessionId::from(session_id.to_string())),
            turn_id: turn_id.cloned(),
            turn_index: None,
            protocol_iteration: None,
        },
        replay_discriminator,
    )
    .with_caused_by(caused_by)
}

// Version 3 includes the admitted execution scope in the direct effect's
// address. Version 2 identified the replay only by descriptive session fields.
const DIRECT_EFFECT_FAMILY_VERSION: u8 = 3;

fn direct_effect_replay_preimage(
    session_id: &SessionId,
    turn_id: Option<&TurnId>,
    usage_source: &str,
    replay_discriminator: &str,
) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.direct-effect-replay-key",
        DIRECT_EFFECT_FAMILY_VERSION,
    );
    identity.string(session_id);
    identity.optional(turn_id, |identity, turn_id| identity.string(turn_id));
    identity.string(usage_source);
    identity.string(replay_discriminator);
    identity.finish()
}

pub fn direct_request_discriminator(
    explicit_replay: Option<&RuntimeReplay>,
    caused_by: Option<&CausalRef>,
    ordinal: u64,
) -> String {
    // Family v3 removes request content from replay identity and admits the
    // execution scope. Store schemas are bumped as a reject-and-recreate
    // cutover; durable workflow adapters must likewise begin a fresh state
    // namespace before deployment.
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.direct-effect-discriminator",
        DIRECT_EFFECT_FAMILY_VERSION,
    );
    identity.optional(caused_by, project_direct_causal_ref);
    if let Some(replay) = explicit_replay.filter(|replay| !replay.key.is_empty()) {
        identity.tag(1);
        identity.string(&replay.key);
    } else {
        identity.tag(2);
        identity.u64(ordinal);
    }
    crate::stable_identity::rendered_hash(
        "direct-discriminator",
        DIRECT_EFFECT_FAMILY_VERSION,
        &identity.finish(),
    )
}

fn project_direct_causal_ref(
    identity: &mut crate::stable_identity::IdentityEncoder,
    caused_by: &CausalRef,
) {
    match caused_by {
        CausalRef::Turn {
            session_id,
            turn_id,
        } => {
            identity.tag(1);
            identity.string(session_id);
            identity.string(turn_id);
        }
        CausalRef::Effect { address } => {
            identity.tag(2);
            project_effect_address(identity, address);
        }
        CausalRef::ToolCall {
            session_id,
            call_id,
        } => {
            identity.tag(3);
            identity.string(session_id);
            identity.string(call_id);
        }
        CausalRef::Process { process_id } => {
            identity.tag(4);
            identity.string(process_id);
        }
        CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => {
            identity.tag(5);
            identity.string(process_id);
            identity.u64(*sequence);
        }
        CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id,
            subscription_incarnation,
            subscription_revision,
        } => {
            identity.tag(6);
            identity.string(occurrence_id);
            identity.optional(subscription_id.as_deref(), |identity, value| {
                identity.string(value)
            });
            identity.optional(subscription_incarnation.as_deref(), |identity, value| {
                identity.string(value)
            });
            identity.optional(
                *subscription_revision,
                crate::stable_identity::IdentityEncoder::u64,
            );
        }
        CausalRef::SessionNode {
            session_id,
            node_id,
        } => {
            identity.tag(7);
            identity.string(session_id);
            identity.string(node_id);
        }
    }
}

pub(crate) fn project_effect_address(
    identity: &mut crate::stable_identity::IdentityEncoder,
    address: &EffectAddress,
) {
    match &address.execution_scope {
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } => {
            identity.tag(1);
            identity.string(session_id);
            identity.string(turn_id);
        }
        ExecutionScope::Process { process_id } => {
            identity.tag(2);
            identity.string(process_id);
        }
        ExecutionScope::QueueDrain {
            session_id,
            drain_id,
        } => {
            identity.tag(3);
            identity.string(session_id);
            identity.string(drain_id);
        }
        ExecutionScope::SessionDelete { session_id } => {
            identity.tag(4);
            identity.string(session_id);
        }
        ExecutionScope::RuntimeOperation { operation_id } => {
            identity.tag(5);
            identity.string(operation_id);
        }
    }
    identity.string(&address.replay_key);
}

#[expect(
    clippy::expect_used,
    reason = "the address carries an admitted execution scope"
)]
pub fn causal_replay_discriminator(caused_by: &CausalRef) -> String {
    fn field(value: &str) -> String {
        format!("{}:{value}", value.len())
    }
    fn optional_field(value: Option<&str>) -> String {
        value.map_or_else(|| "0".to_string(), |value| format!("1:{}", field(value)))
    }

    match caused_by {
        CausalRef::Turn {
            session_id,
            turn_id,
        } => format!("cause:1:{}:{}:", field(session_id), field(turn_id)),
        CausalRef::Effect { address } => format!(
            "cause:2:{}:{}:",
            field(
                address
                    .execution_scope
                    .journal_identity()
                    .expect("causal effect address contains a valid scope")
                    .key()
            ),
            field(&address.replay_key)
        ),
        CausalRef::ToolCall {
            session_id,
            call_id,
        } => format!("cause:3:{}:{}:", field(session_id), field(call_id)),
        CausalRef::Process { process_id } => format!("cause:4:{}:", field(process_id)),
        CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => format!("cause:5:{}:{sequence}:", field(process_id)),
        CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id,
            subscription_incarnation,
            subscription_revision,
        } => {
            let revision =
                subscription_revision.map_or_else(|| "0".to_string(), |value| format!("1:{value}"));
            format!(
                "cause:6:{}:{}:{}:{revision}:",
                field(occurrence_id),
                optional_field(subscription_id.as_deref()),
                optional_field(subscription_incarnation.as_deref()),
            )
        }
        CausalRef::SessionNode {
            session_id,
            node_id,
        } => format!("cause:7:{}:{}:", field(session_id), field(node_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_invocations_use_admitted_scope_without_losing_turn_attribution() {
        let session_id = SessionId::from("session:subagent:call");
        let turn_id = TurnId::from("process:subagent:call");
        let process_scope =
            ExecutionScope::process(crate::process_id_for_test("process:subagent:call"));

        let effect = turn_effect_invocation(
            &process_scope,
            &session_id,
            &turn_id,
            3,
            5,
            EffectId(7),
            RuntimeEffectKind::LlmCall,
        );
        assert_eq!(effect.execution_scope(), &process_scope);
        assert_eq!(effect.attribution.session_id.as_ref(), Some(&session_id));
        assert_eq!(effect.attribution.turn_id.as_ref(), Some(&turn_id));
        assert_eq!(effect.attribution.turn_index, Some(3));
        assert_eq!(effect.attribution.protocol_iteration, Some(5));
        assert_eq!(
            effect.replay_key(),
            "session:subagent:call:process:subagent:call:3:5:llm_call:7"
        );

        let acceptance = turn_acceptance_effect_invocation(&process_scope, &session_id, &turn_id);
        assert_eq!(acceptance.execution_scope(), &process_scope);
        assert_eq!(
            acceptance.attribution.session_id.as_ref(),
            Some(&session_id)
        );
        assert_eq!(acceptance.attribution.turn_id.as_ref(), Some(&turn_id));
        // The drive fixes the turn index; nothing before it names one
        // (FIG-3682).
        assert_eq!(acceptance.attribution.turn_index, None);
        assert_eq!(acceptance.attribution.protocol_iteration, None);
        assert_eq!(
            acceptance.replay_key(),
            "session:subagent:call:process:subagent:call:accept_turn_input"
        );

        let drive = turn_input_drive_effect_invocation(&acceptance);
        assert_eq!(drive.execution_scope(), &process_scope);
        assert_eq!(drive.attribution, acceptance.attribution);
        assert_eq!(
            drive.replay_key(),
            "session:subagent:call:process:subagent:call:accept_turn_input:claim_accepted_turn_input"
        );
        assert_eq!(
            drive.effect_id(),
            "process:subagent:call.accept.claim_accepted_turn_input"
        );
        assert_eq!(drive.caused_by, Some(acceptance.causal_ref()));
    }

    #[test]
    fn turn_invocations_retain_genuine_turn_scope() {
        let session_id = SessionId::from("session");
        let turn_id = TurnId::from("turn");
        let turn_scope = ExecutionScope::turn(&session_id, &turn_id);

        let effect = turn_effect_invocation(
            &turn_scope,
            &session_id,
            &turn_id,
            1,
            2,
            EffectId(4),
            RuntimeEffectKind::Checkpoint,
        );
        let acceptance = turn_acceptance_effect_invocation(&turn_scope, &session_id, &turn_id);

        assert_eq!(effect.execution_scope(), &turn_scope);
        assert_eq!(acceptance.execution_scope(), &turn_scope);
    }

    #[test]
    fn direct_effect_identity_golden_corpus() {
        let causes = [
            (
                CausalRef::Turn {
                    session_id: SessionId::from("ab"),
                    turn_id: TurnId::from("c"),
                },
                "direct-discriminator:v3:blake3:78e0554f9dc6f57ec9fb07dacb6a6bc9dc221c5d696da56562d52480951c025a",
            ),
            (
                CausalRef::Effect {
                    address: EffectAddress::new(ExecutionScope::runtime_operation("s"), "e")
                        .expect("valid effect cause"),
                },
                "direct-discriminator:v3:blake3:6a86c188f94aabd14d86ec775e19ad91498610bf7c3f8865cc1231d4cb467efc",
            ),
            (
                CausalRef::ToolCall {
                    session_id: SessionId::from("s"),
                    call_id: "c".to_string(),
                },
                "direct-discriminator:v3:blake3:f731dff92e6119ed6f49dad112351e389440d6f591fa2ace2fd3972241110949",
            ),
            (
                CausalRef::Process {
                    process_id: crate::process_id_for_test("p"),
                },
                "direct-discriminator:v3:blake3:34ad73dc0d8b6305ccdc1207afa8e4ef37c30035a2c2bf9455256563d6861c74",
            ),
            (
                CausalRef::ProcessEvent {
                    process_id: crate::process_id_for_test("p"),
                    sequence: 0,
                },
                "direct-discriminator:v3:blake3:8e1e40c288970d6da166b74e66c8fd393ace16bf4ca2c36edd30d23a65976b1c",
            ),
            (
                CausalRef::TriggerOccurrence {
                    occurrence_id: "o".to_string(),
                    subscription_id: Some("s".to_string()),
                    subscription_incarnation: None,
                    subscription_revision: Some(0),
                },
                "direct-discriminator:v3:blake3:d56a11d6ab13e7e6d320486668f8c684d0d72725161f4f2d655729bd751a9c04",
            ),
            (
                CausalRef::SessionNode {
                    session_id: SessionId::from("s"),
                    node_id: "n".to_string(),
                },
                "direct-discriminator:v3:blake3:6910556f4a2679c13d3329a613bc5467cecc143d30b2a4013eead1fe77b1b067",
            ),
        ];
        for (cause, expected) in causes {
            assert_eq!(
                direct_request_discriminator(None, Some(&cause), 1),
                expected
            );
        }
        assert_eq!(
            direct_request_discriminator(
                Some(&RuntimeReplay {
                    key: "a:b".to_string(),
                    attribution: None,
                }),
                None,
                99,
            ),
            "direct-discriminator:v3:blake3:62222a794daf6905fa399028c7384ed983a60058bca0c630a91a2e7466b46d88"
        );
        assert_eq!(
            direct_request_discriminator(
                Some(&RuntimeReplay {
                    key: String::new(),
                    attribution: None,
                }),
                None,
                0,
            ),
            "direct-discriminator:v3:blake3:5ba4e3f6185036771681615925aa5cd3287065402ec0ec69630d8288629bd046"
        );
        assert_eq!(
            direct_request_discriminator(
                None,
                Some(&CausalRef::Turn {
                    session_id: SessionId::from("ab"),
                    turn_id: TurnId::from("c"),
                }),
                1,
            ),
            "direct-discriminator:v3:blake3:78e0554f9dc6f57ec9fb07dacb6a6bc9dc221c5d696da56562d52480951c025a"
        );
        assert_eq!(
            direct_request_discriminator(
                None,
                Some(&CausalRef::Turn {
                    session_id: SessionId::from("a"),
                    turn_id: TurnId::from("bc"),
                }),
                1,
            ),
            "direct-discriminator:v3:blake3:c71a5192c9a6d515e0c954b8e295d8c72db1d9215b7a960b5c320b01ca73def6"
        );

        let discriminator = direct_request_discriminator(None, None, 1);
        let preimage = direct_effect_replay_preimage(
            &SessionId::from("s"),
            Some(&TurnId::from("t")),
            "u",
            &discriminator,
        );
        assert_eq!(
            hex(&preimage),
            "6c6173682d737461626c652d6964656e746974790203000000000000001d6c6173682e6469726563742d6566666563742d7265706c61792d6b657900000000000000017301000000000000000174000000000000000175000000000000005f6469726563742d6469736372696d696e61746f723a76333a626c616b65333a31666631386539313861383032313933623937636263346537316433313039626639306232656230323966396137343233396264333865386331343839663665"
        );
        assert_eq!(
            direct_effect_invocation(
                &ExecutionScope::turn("s", "t"),
                &SessionId::from("s"),
                "u",
                discriminator,
                Some(&TurnId::from("t")),
                None,
            )
            .replay_key(),
            "direct:v3:blake3:43bfa7f80a468e47f435784b0cf43f95ffef5a368e3fb26ee78929ffaf618c35"
        );

        let first_discriminator = direct_request_discriminator(
            Some(&RuntimeReplay {
                key: "x:direct:v2:ordinal:1".to_string(),
                attribution: None,
            }),
            None,
            0,
        );
        let first_preimage = direct_effect_replay_preimage(
            &SessionId::from("s"),
            Some(&TurnId::from("t")),
            "u",
            &first_discriminator,
        );
        assert_eq!(
            hex(&first_preimage),
            "6c6173682d737461626c652d6964656e746974790203000000000000001d6c6173682e6469726563742d6566666563742d7265706c61792d6b657900000000000000017301000000000000000174000000000000000175000000000000005f6469726563742d6469736372696d696e61746f723a76333a626c616b65333a34336339616331396231653233316136616162653966386265623333623337376365343731313666343165336661653963373133363535306233346464616634"
        );
        let first = direct_effect_invocation(
            &ExecutionScope::turn("s", "t"),
            &SessionId::from("s"),
            "u",
            first_discriminator,
            Some(&TurnId::from("t")),
            None,
        );
        assert_eq!(
            first.replay_key(),
            "direct:v3:blake3:f8af7289056d371cc0b80d6d1f4ad3f8cccfd86bc863e9744a080354544dfa9e"
        );
        let second_discriminator = direct_request_discriminator(None, None, 1);
        let second_preimage = direct_effect_replay_preimage(
            &SessionId::from("s"),
            Some(&TurnId::from("t")),
            "u:direct:v2:caller:21:x",
            &second_discriminator,
        );
        assert_eq!(
            hex(&second_preimage),
            "6c6173682d737461626c652d6964656e746974790203000000000000001d6c6173682e6469726563742d6566666563742d7265706c61792d6b6579000000000000000173010000000000000001740000000000000017753a6469726563743a76323a63616c6c65723a32313a78000000000000005f6469726563742d6469736372696d696e61746f723a76333a626c616b65333a31666631386539313861383032313933623937636263346537316433313039626639306232656230323966396137343233396264333865386331343839663665"
        );
        let second = direct_effect_invocation(
            &ExecutionScope::turn("s", "t"),
            &SessionId::from("s"),
            "u:direct:v2:caller:21:x",
            second_discriminator,
            Some(&TurnId::from("t")),
            None,
        );
        assert_eq!(
            second.replay_key(),
            "direct:v3:blake3:89749b2923cda770d21363d4c9723d995d61023fe30a4d9d086f8ef240f60c0c"
        );
        assert_ne!(first.replay_key(), second.replay_key());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
