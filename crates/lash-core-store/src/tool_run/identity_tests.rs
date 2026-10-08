//! Golden identity preimages the tool end state must keep byte for byte
//! (removal row M0201). Record layouts change in place before the 1.0 cut;
//! these identities do not move with them. A failure here means a change
//! moved a separately owned identity, not that the pin needs refreshing.

use lash_sansio::{ToolCallId, ToolCallPosition};

use crate::effect_opener::EffectOpener;
use crate::process_identity::{StartKey, StartKeyDerivation};
use crate::{ExecutionScope, ProcessId};

const DERIVE: StartKeyDerivation = StartKeyDerivation::LASH_START_PATHS;

#[test]
fn start_keys_keep_every_family_and_scope_tag() {
    let intent = lash_sansio::ToolIntentIdentity {
        owner: lash_sansio::RuntimeOwner::Session("session-1".into()),
        execution_scope_id: "turn-1".into(),
        tool_call_id: ToolCallId::fixture("start"),
        intent_index: 0,
        replay_key: "intent-replay-key".into(),
        minting_emission_replay_key: None,
    };
    let keys: Vec<(&str, StartKey)> = vec![
        ("tool_intent", DERIVE.for_tool_intent(&intent)),
        ("host", StartKey::for_host("host-key")),
        (
            "keyless/turn (scope tag 1)",
            DERIVE.for_keyless_host(&ExecutionScope::turn("session-1", "turn-1"), 0),
        ),
        (
            "keyless/process (scope tag 2)",
            DERIVE.for_keyless_host(&ExecutionScope::process(ProcessId::fixture("worker")), 0),
        ),
        (
            "keyless/session_operation (scope tag 3)",
            DERIVE.for_keyless_host(
                &ExecutionScope::session_operation("session-1", "batch-7"),
                1,
            ),
        ),
        (
            "keyless/session_delete (scope tag 4)",
            DERIVE.for_keyless_host(&ExecutionScope::session_delete("session-1"), 0),
        ),
        (
            "keyless/runtime_operation (scope tag 5)",
            DERIVE.for_keyless_host(&ExecutionScope::runtime_operation("op-1"), 0),
        ),
    ];
    let rendered: Vec<String> = keys
        .iter()
        .map(|(name, key)| format!("{name} {key}"))
        .collect();
    assert_eq!(
        rendered,
        vec![
            "tool_intent process-start-key:v1:intent:blake3:5dbfcc38194249b9dc1464436ae9f7ca238b89aa755da7ce5dacd00d4d31edac",
            "host process-start-key:v1:host:blake3:6aa69ddf2235e541820d19240250265d5da2965589cc52083fd45aadc75f2950",
            "keyless/turn (scope tag 1) process-start-key:v1:keyless:blake3:e8f889d5614d20828986e7f9abe395573f686a2e9d60db236db629bc20ebdc1b",
            "keyless/process (scope tag 2) process-start-key:v1:keyless:blake3:02ee475cfb0af992b3fc14eae8258670340738fceba594e0ee4ae9c666f9868f",
            "keyless/session_operation (scope tag 3) process-start-key:v1:keyless:blake3:9bff87268d7e8bd50f1e93690066b9fe9f3521ea0d146f4bafaa4b6a005b4afb",
            "keyless/session_delete (scope tag 4) process-start-key:v1:keyless:blake3:62c84432f1a05790e7fbc838e7981cef8711037c3b840017be57e1f98d5033c4",
            "keyless/runtime_operation (scope tag 5) process-start-key:v1:keyless:blake3:7ce476153d3458198730cded0977ae95ce6d89e69ade83975ab084736a55cd8f",
        ]
    );
}

#[test]
fn opener_encodings_and_their_call_ids_keep_their_bytes() {
    let openers = [
        EffectOpener::turn("session-1", "turn-1"),
        EffectOpener::session_operation("session-1", "batch-7"),
        EffectOpener::process(ProcessId::fixture("worker")),
    ];
    let rendered: Vec<String> = openers
        .iter()
        .map(|opener| {
            let call = opener
                .tool_call_admission()
                .call_id(&[ToolCallPosition::CodeCell("cell-0")]);
            format!("{} {call}", opener.identity_encoding())
        })
        .collect();
    assert_eq!(
        rendered,
        vec![
            "turn:9:session-1:6:turn-1 tc_2dbdafd637971b72e2a48188a103966bb0f747f68b2221fc2375b4cf6c30e557",
            "drain:9:session-1:7:batch-7 tc_eef31e5f9706678bb51aa77083156134b95ec312bd8372d0c250f5a312fc6ae9",
            "process:34:p_7d74fe3c5a3c74bfaf41de3113f46c4f tc_097871c49a7941d8ec8c6a8f153bd88e96a458120fe55d9bf6fcf92138f673a2",
        ]
    );
}
