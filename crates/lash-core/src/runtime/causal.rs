use crate::sansio::EffectId;
use crate::{
    CausalRef, RuntimeEffectKind, RuntimeInvocation, RuntimeReplay, RuntimeScope, RuntimeSubject,
};

pub(crate) fn turn_effect_invocation(
    session_id: &str,
    turn_id: &str,
    turn_index: usize,
    protocol_iteration: usize,
    effect_id: EffectId,
    effect_kind: RuntimeEffectKind,
) -> RuntimeInvocation {
    RuntimeInvocation::effect(
        RuntimeScope::for_turn(session_id, turn_id, turn_index, protocol_iteration),
        effect_id.0.to_string(),
        effect_kind,
        turn_effect_replay_key(
            session_id,
            turn_id,
            turn_index,
            protocol_iteration,
            effect_kind,
            effect_id,
        ),
    )
}

/// Invocation for the durable acceptance of a turn input (ADR 0069 §6).
///
/// Acceptance runs before the turn does, so it has no protocol iteration to
/// name and cannot use [`turn_effect_invocation`]. Its replay key is a function
/// of the session and the execution scope alone — both of which a replaying
/// engine reconstructs identically — so a redriven handler journals the same
/// entry and re-derives the admission instead of admitting a second turn.
pub(crate) fn turn_acceptance_effect_invocation(
    session_id: &str,
    turn_id: &str,
    turn_index: usize,
) -> RuntimeInvocation {
    RuntimeInvocation::effect(
        RuntimeScope::for_turn(session_id, turn_id, turn_index, 0),
        format!("{turn_id}.accept"),
        RuntimeEffectKind::AcceptTurnInput,
        format!(
            "{session_id}:{turn_id}:{}",
            RuntimeEffectKind::AcceptTurnInput.as_str()
        ),
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
pub(crate) fn turn_phase_effect_invocation(
    parent: &RuntimeInvocation,
    effect_id: EffectId,
    phase_kind: RuntimeEffectKind,
) -> RuntimeInvocation {
    child_effect_invocation(
        parent,
        format!("{}.{}", effect_id.0, phase_kind.as_str()),
        phase_kind,
        phase_kind.as_str(),
    )
}

fn turn_effect_replay_key(
    session_id: &str,
    turn_id: &str,
    turn_index: usize,
    protocol_iteration: usize,
    kind: RuntimeEffectKind,
    effect_id: EffectId,
) -> String {
    format!(
        "{session_id}:{turn_id}:{turn_index}:{protocol_iteration}:{}:{}",
        kind.as_str(),
        effect_id.0
    )
}

pub(crate) fn child_effect_invocation(
    parent: &RuntimeInvocation,
    effect_id: impl Into<String>,
    kind: RuntimeEffectKind,
    replay_suffix: impl AsRef<str>,
) -> RuntimeInvocation {
    let replay_base = parent
        .replay_key()
        .or_else(|| parent.effect_id())
        .unwrap_or("effect");
    RuntimeInvocation {
        scope: parent.scope.clone(),
        subject: RuntimeSubject::Effect {
            effect_id: effect_id.into(),
            kind,
        },
        caused_by: parent.causal_ref(),
        replay: Some(RuntimeReplay {
            key: format!("{replay_base}:{}", replay_suffix.as_ref()),
            attribution: parent
                .replay
                .as_ref()
                .and_then(|replay| replay.attribution.clone()),
        }),
    }
}

pub(crate) fn tool_retry_sleep_invocation(
    parent: &RuntimeInvocation,
    tool_name: &str,
    attempt: u32,
) -> RuntimeInvocation {
    let parent_effect_id = parent.effect_id().unwrap_or("effect");
    child_effect_invocation(
        parent,
        format!("{parent_effect_id}:{tool_name}:attempt:{attempt}:sleep"),
        RuntimeEffectKind::Sleep,
        format!("{tool_name}:attempt:{attempt}:sleep"),
    )
}

pub(crate) fn process_sleep_invocation(
    session_id: &str,
    parent: Option<&RuntimeInvocation>,
    scope: &str,
    sequence: u64,
) -> RuntimeInvocation {
    let suffix = format!("process:{scope}:sleep:{sequence}");
    if let Some(parent) = parent {
        let parent_effect_id = parent.effect_id().unwrap_or("effect");
        return child_effect_invocation(
            parent,
            format!("{parent_effect_id}:{suffix}"),
            RuntimeEffectKind::Sleep,
            suffix,
        );
    }
    RuntimeInvocation::effect(
        RuntimeScope::new(session_id),
        suffix.clone(),
        RuntimeEffectKind::Sleep,
        suffix,
    )
}

pub(crate) fn process_await_event_invocation(
    session_id: &str,
    parent: Option<&RuntimeInvocation>,
    process_id: &str,
    signal_name: &str,
    ordinal: u64,
) -> RuntimeInvocation {
    let suffix = format!("process:{process_id}:signal.{signal_name}:await:{ordinal}");
    if let Some(parent) = parent {
        let parent_effect_id = parent.effect_id().unwrap_or("effect");
        return child_effect_invocation(
            parent,
            format!("{parent_effect_id}:{suffix}"),
            RuntimeEffectKind::AwaitEvent,
            suffix,
        );
    }
    RuntimeInvocation::effect(
        RuntimeScope::new(session_id),
        suffix.clone(),
        RuntimeEffectKind::AwaitEvent,
        suffix,
    )
}

pub(crate) fn process_effect_invocation(
    session_id: &str,
    parent: Option<RuntimeInvocation>,
    effect_id: &str,
) -> RuntimeInvocation {
    if let Some(parent) = parent {
        let scope = if let Some(turn_id) = parent.scope.turn_id.clone() {
            RuntimeScope {
                session_id: session_id.to_string(),
                turn_id: Some(turn_id),
                turn_index: parent.scope.turn_index,
                protocol_iteration: parent.scope.protocol_iteration,
            }
        } else {
            RuntimeScope::new(session_id)
        };
        let replay_base = parent.replay_key().unwrap_or("process");
        return RuntimeInvocation {
            scope,
            subject: RuntimeSubject::Effect {
                effect_id: effect_id.to_string(),
                kind: RuntimeEffectKind::Process,
            },
            caused_by: parent.causal_ref(),
            replay: Some(RuntimeReplay {
                key: format!("{replay_base}:{effect_id}"),
                attribution: parent
                    .replay
                    .as_ref()
                    .and_then(|replay| replay.attribution.clone()),
            }),
        };
    }
    RuntimeInvocation::effect(
        RuntimeScope::new(session_id),
        effect_id.to_string(),
        RuntimeEffectKind::Process,
        format!("{session_id}:{effect_id}"),
    )
}

pub fn process_event_invocation(
    process_id: &str,
    sequence: u64,
    event_type: &str,
    replay: Option<RuntimeReplay>,
) -> RuntimeInvocation {
    RuntimeInvocation {
        scope: RuntimeScope::new("runtime"),
        subject: RuntimeSubject::ProcessEvent {
            process_id: process_id.to_string(),
            sequence,
            event_type: event_type.to_string(),
        },
        caused_by: Some(CausalRef::Process {
            process_id: process_id.to_string(),
        }),
        replay,
    }
}

pub(crate) fn trigger_occurrence_invocation(
    session_id: &str,
    occurrence_id: &str,
) -> RuntimeInvocation {
    RuntimeInvocation {
        scope: RuntimeScope::new(session_id),
        subject: RuntimeSubject::TriggerOccurrence {
            occurrence_id: occurrence_id.to_string(),
        },
        caused_by: None,
        replay: Some(RuntimeReplay {
            key: format!("trigger:{occurrence_id}"),
            attribution: None,
        }),
    }
}

pub(crate) fn direct_effect_invocation(
    session_id: &str,
    usage_source: &str,
    replay_discriminator: String,
    turn_id: Option<&str>,
    caused_by: Option<CausalRef>,
) -> RuntimeInvocation {
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
    RuntimeInvocation::effect(
        RuntimeScope {
            session_id: session_id.to_string(),
            turn_id: turn_id.map(str::to_string),
            turn_index: None,
            protocol_iteration: None,
        },
        replay_discriminator,
        RuntimeEffectKind::Direct,
        replay_key,
    )
    .with_caused_by(caused_by)
}

const DIRECT_EFFECT_FAMILY_VERSION: u8 = 2;

fn direct_effect_replay_preimage(
    session_id: &str,
    turn_id: Option<&str>,
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

pub(crate) fn direct_request_discriminator(
    explicit_replay: Option<&RuntimeReplay>,
    caused_by: Option<&CausalRef>,
    ordinal: u64,
) -> String {
    // Family v2 removes request content from replay identity entirely. Store
    // schemas are bumped as a reject-and-recreate cutover; durable workflow
    // adapters must likewise begin a fresh state namespace before deployment.
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
        CausalRef::Effect {
            session_id,
            turn_id,
            effect_id,
        } => {
            identity.tag(2);
            identity.string(session_id);
            identity.optional(turn_id.as_deref(), |identity, turn_id| {
                identity.string(turn_id)
            });
            identity.string(effect_id);
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

pub(super) fn causal_replay_discriminator(caused_by: &CausalRef) -> String {
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
        CausalRef::Effect {
            session_id,
            turn_id,
            effect_id,
        } => {
            format!(
                "cause:2:{}:{}:{}:",
                field(session_id),
                optional_field(turn_id.as_deref()),
                field(effect_id)
            )
        }
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
    fn direct_effect_identity_golden_corpus() {
        let causes = [
            (
                CausalRef::Turn {
                    session_id: "ab".to_string(),
                    turn_id: "c".to_string(),
                },
                "direct-discriminator:v2:blake3:990084fc9028c4cdec32cdc3182e323cdc03bf1fef212862fbd9442d60a69d42",
            ),
            (
                CausalRef::Effect {
                    session_id: "s".to_string(),
                    turn_id: None,
                    effect_id: "e".to_string(),
                },
                "direct-discriminator:v2:blake3:3f43a9b312aa3b7f98045904632bd4daf3aada4430019b4427616f600b6f0857",
            ),
            (
                CausalRef::ToolCall {
                    session_id: "s".to_string(),
                    call_id: "c".to_string(),
                },
                "direct-discriminator:v2:blake3:e54157dc19ee5d6d23ca76d9e7eb671f67422b83fd18519a279c1adf1f949202",
            ),
            (
                CausalRef::Process {
                    process_id: "p".to_string(),
                },
                "direct-discriminator:v2:blake3:ae12c1bd5c974ce1df6254fdff30ce90dfba1332bf8317caadc39d1cc32a9d36",
            ),
            (
                CausalRef::ProcessEvent {
                    process_id: "p".to_string(),
                    sequence: 0,
                },
                "direct-discriminator:v2:blake3:0644b6d881b463c4a0af5439e1215e344eedf6722cb58aadcf0cb85936086304",
            ),
            (
                CausalRef::TriggerOccurrence {
                    occurrence_id: "o".to_string(),
                    subscription_id: Some("s".to_string()),
                    subscription_incarnation: None,
                    subscription_revision: Some(0),
                },
                "direct-discriminator:v2:blake3:ca1bcd4e4d73231f9aed50b606f78c0d5b4f008c03127d27f7e8ee615a0986e1",
            ),
            (
                CausalRef::SessionNode {
                    session_id: "s".to_string(),
                    node_id: "n".to_string(),
                },
                "direct-discriminator:v2:blake3:16c84d9fc9b0b5190737be74c70df27637aa93a72558ff00fc639cfb17188403",
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
            "direct-discriminator:v2:blake3:eea6883a6f67874275731b7f5a3c9cf2e87ee743edd7c9e06004b39d5d47e2b5"
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
            "direct-discriminator:v2:blake3:464a8832ef048a3c853cc771a55b2b10ce49699f6c7265fa1992047b533021b6"
        );
        assert_eq!(
            direct_request_discriminator(
                None,
                Some(&CausalRef::Turn {
                    session_id: "ab".to_string(),
                    turn_id: "c".to_string(),
                }),
                1,
            ),
            "direct-discriminator:v2:blake3:990084fc9028c4cdec32cdc3182e323cdc03bf1fef212862fbd9442d60a69d42"
        );
        assert_eq!(
            direct_request_discriminator(
                None,
                Some(&CausalRef::Turn {
                    session_id: "a".to_string(),
                    turn_id: "bc".to_string(),
                }),
                1,
            ),
            "direct-discriminator:v2:blake3:4652b5ce2ed67bdb741213cc408c6da739e11ff4d9cced560bd083384a31dc98"
        );

        let discriminator = direct_request_discriminator(None, None, 1);
        let preimage = direct_effect_replay_preimage("s", Some("t"), "u", &discriminator);
        assert_eq!(
            hex(&preimage),
            "6c6173682d737461626c652d6964656e746974790202000000000000001d6c6173682e6469726563742d6566666563742d7265706c61792d6b657900000000000000017301000000000000000174000000000000000175000000000000005f6469726563742d6469736372696d696e61746f723a76323a626c616b65333a63646236306335326563653334356438396261353435633835626163323238343534653562353834336132646534306536376632386434343464323364323064"
        );
        assert_eq!(
            direct_effect_invocation("s", "u", discriminator, Some("t"), None).replay_key(),
            Some(
                "direct:v2:blake3:c92b5337c6f126eb1f8951b3c0c5eea412be5953c0e254bc4369e08d29d33451"
            )
        );

        let first_discriminator = direct_request_discriminator(
            Some(&RuntimeReplay {
                key: "x:direct:v2:ordinal:1".to_string(),
                attribution: None,
            }),
            None,
            0,
        );
        let first_preimage =
            direct_effect_replay_preimage("s", Some("t"), "u", &first_discriminator);
        assert_eq!(
            hex(&first_preimage),
            "6c6173682d737461626c652d6964656e746974790202000000000000001d6c6173682e6469726563742d6566666563742d7265706c61792d6b657900000000000000017301000000000000000174000000000000000175000000000000005f6469726563742d6469736372696d696e61746f723a76323a626c616b65333a38356537333765643465663038366634653336616436386263396330333632393264363665623430613831646130383031356436363163653530373435303263"
        );
        let first = direct_effect_invocation("s", "u", first_discriminator, Some("t"), None);
        assert_eq!(
            first.replay_key(),
            Some(
                "direct:v2:blake3:359eeeb5c5899114070602cf4659773cf646c6bd8aa919596785bfe55de41e34"
            )
        );
        let second_discriminator = direct_request_discriminator(None, None, 1);
        let second_preimage = direct_effect_replay_preimage(
            "s",
            Some("t"),
            "u:direct:v2:caller:21:x",
            &second_discriminator,
        );
        assert_eq!(
            hex(&second_preimage),
            "6c6173682d737461626c652d6964656e746974790202000000000000001d6c6173682e6469726563742d6566666563742d7265706c61792d6b6579000000000000000173010000000000000001740000000000000017753a6469726563743a76323a63616c6c65723a32313a78000000000000005f6469726563742d6469736372696d696e61746f723a76323a626c616b65333a63646236306335326563653334356438396261353435633835626163323238343534653562353834336132646534306536376632386434343464323364323064"
        );
        let second = direct_effect_invocation(
            "s",
            "u:direct:v2:caller:21:x",
            second_discriminator,
            Some("t"),
            None,
        );
        assert_eq!(
            second.replay_key(),
            Some(
                "direct:v2:blake3:91275bb8dccd63323941efc579d9177fbc134adde4f970c0c521623d7ef5edc7"
            )
        );
        assert_ne!(first.replay_key(), second.replay_key());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
