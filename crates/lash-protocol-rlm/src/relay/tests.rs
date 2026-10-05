use serde_json::json;

use super::*;
use lash_rlm_types::{RlmProtocolEvent, RlmTrajectoryEntry};

/// The baton carries only what the next step can be rebuilt from: a context
/// of strings within budget, plain vars that do not shadow a harness binding,
/// and a boolean `final`. The context travels in the seed under `context`.
#[test]
fn next_admits_only_a_baton_the_next_step_can_be_rebuilt_from() {
    let budget = 20;
    let next = RelayNext::from_args(
        &json!({ "context": ["a", "bc"], "vars": { "n": 1 }, "final": true }),
        budget,
    )
    .expect("a valid baton");
    assert_eq!(next.context, ["a", "bc"]);
    assert!(next.final_turn);
    let seed = next.seed_body();
    assert_eq!(seed.globals[CONTEXT_VAR], json!(["a", "bc"]));
    assert_eq!(seed.globals["n"], json!(1));
    assert_eq!(RelayBaton::from_seed(&seed).vars.len(), 1);

    for (args, refusal) in [
        (json!({}), "missing required parameter: context"),
        (json!({ "context": [1] }), "context[0] must be a string"),
        (
            json!({ "context": "a" }),
            "context must be an array of strings",
        ),
        (json!({ "context": [], "vars": 3 }), "vars must be a record"),
        (
            json!({ "context": [], "vars": { "context": [] } }),
            "vars cannot carry `context`",
        ),
        (
            json!({ "context": [], "vars": { "transcript": [] } }),
            "vars cannot carry `transcript`",
        ),
        (
            json!({ "context": [], "final": "yes" }),
            "final must be a boolean",
        ),
        (
            json!({ "context": ["x".repeat(21)] }),
            "over the 20-character budget",
        ),
    ] {
        let error = RelayNext::from_args(&args, budget).expect_err(refusal);
        assert!(error.contains(refusal), "{args}: {error}");
    }
}

fn user_message(
    id: &str,
    text: &str,
    origin: Option<lash_core::MessageOrigin>,
) -> lash_core::Message {
    lash_core::Message {
        id: id.to_string(),
        role: lash_core::MessageRole::User,
        parts: vec![lash_core::Part::text(
            format!("{id}.p0"),
            text.to_string(),
            None,
        )]
        .into(),
        origin,
        reply_marker: None,
    }
}

fn protocol_record(event: RlmProtocolEvent) -> lash_core::SessionHistoryRecord {
    lash_core::SessionHistoryRecord::Protocol(crate::projection::rlm_protocol_event(
        event,
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}

/// Found in the first live relay run: once a step committed, the turn's
/// user message vanished from every later harness message, because the
/// step's own seed ended the input run and a host note on the user channel
/// after it started a new one. Only an earlier turn's records end the run.
#[test]
fn the_turns_input_survives_its_own_steps_in_the_view() {
    let turn = lash_sansio::TurnId::from("turn-2");
    let events = vec![
        protocol_record(RlmProtocolEvent::RlmTrajectoryEntry(RlmTrajectoryEntry {
            id: "lashlang_step_turn-1_0".to_string(),
            ..Default::default()
        })),
        lash_core::SessionHistoryRecord::Conversation(
            lash_core::session_model::ConversationRecord::from_message(user_message(
                "input",
                "what is the vault code?",
                Some(lash_core::MessageOrigin::TurnInput {
                    turn_id: turn.clone(),
                    input_id: None,
                }),
            )),
        ),
        protocol_record(RlmProtocolEvent::RlmTrajectoryEntry(RlmTrajectoryEntry {
            id: format!("lashlang_step_{turn}_0"),
            ..Default::default()
        })),
        protocol_record(RlmProtocolEvent::RlmSeed(
            RelayNext::from_args(&json!({ "context": ["listed"] }), 100)
                .expect("a valid baton")
                .seed_body(),
        )),
    ];
    let messages = lash_core::facade_support::MessageSequence::from(vec![user_message(
        "host-note",
        "Context budget: prepared 1 message(s)",
        None,
    )]);
    let projection =
        lash_core::facade_support::ChronologicalProjection::from_turn_view(&events, &messages);

    let view = RelayView::read(&projection, &turn.to_string()).expect("a readable view");

    let input = view
        .turn_input
        .iter()
        .map(|&index| view.transcript[index].text.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        input,
        [
            "what is the vault code?",
            "Context budget: prepared 1 message(s)"
        ]
    );
    assert!(view.last_step_committed);
    assert_eq!(view.committed.expect("a commit").context, ["listed"]);
}
