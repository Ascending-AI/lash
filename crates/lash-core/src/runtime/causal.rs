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
    let replay_key = match turn_id.filter(|value| !value.is_empty()) {
        Some(turn_id) => {
            format!("{session_id}:{turn_id}:direct:{usage_source}:{replay_discriminator}")
        }
        None => format!("{session_id}:direct:{usage_source}:{replay_discriminator}"),
    };
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

pub(crate) fn direct_request_discriminator(
    explicit_replay: Option<&RuntimeReplay>,
    caused_by: Option<&CausalRef>,
    ordinal: u64,
) -> String {
    // Family v2 removes request content from replay identity entirely. Store
    // schemas are bumped as a reject-and-recreate cutover; durable workflow
    // adapters must likewise begin a fresh state namespace before deployment.
    let cause_discriminator = caused_by
        .map(causal_replay_discriminator)
        .unwrap_or_default();
    if let Some(replay) = explicit_replay.filter(|replay| !replay.key.is_empty()) {
        return format!(
            "direct:v2:{cause_discriminator}caller:{}:{}",
            replay.key.len(),
            replay.key
        );
    }
    format!("direct:v2:{cause_discriminator}ordinal:{ordinal}")
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
                "direct:v2:cause:1:2:ab:1:c:ordinal:1",
            ),
            (
                CausalRef::Effect {
                    session_id: "s".to_string(),
                    turn_id: None,
                    effect_id: "e".to_string(),
                },
                "direct:v2:cause:2:1:s:0:1:e:ordinal:1",
            ),
            (
                CausalRef::ToolCall {
                    session_id: "s".to_string(),
                    call_id: "c".to_string(),
                },
                "direct:v2:cause:3:1:s:1:c:ordinal:1",
            ),
            (
                CausalRef::Process {
                    process_id: "p".to_string(),
                },
                "direct:v2:cause:4:1:p:ordinal:1",
            ),
            (
                CausalRef::ProcessEvent {
                    process_id: "p".to_string(),
                    sequence: 0,
                },
                "direct:v2:cause:5:1:p:0:ordinal:1",
            ),
            (
                CausalRef::TriggerOccurrence {
                    occurrence_id: "o".to_string(),
                    subscription_id: Some("s".to_string()),
                    subscription_incarnation: None,
                    subscription_revision: Some(0),
                },
                "direct:v2:cause:6:1:o:1:1:s:0:1:0:ordinal:1",
            ),
            (
                CausalRef::SessionNode {
                    session_id: "s".to_string(),
                    node_id: "n".to_string(),
                },
                "direct:v2:cause:7:1:s:1:n:ordinal:1",
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
                }),
                None,
                99,
            ),
            "direct:v2:caller:3:a:b"
        );
        assert_eq!(
            direct_request_discriminator(Some(&RuntimeReplay { key: String::new() }), None, 0,),
            "direct:v2:ordinal:0"
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
            "direct:v2:cause:1:2:ab:1:c:ordinal:1"
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
            "direct:v2:cause:1:1:a:2:bc:ordinal:1"
        );
    }
}
