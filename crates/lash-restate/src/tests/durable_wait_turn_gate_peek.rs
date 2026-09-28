use super::process_await_redrive::fig1943_invocation_with_state;
use super::*;
use crate::durable_wait::{RestateDurableWaitIndexRequest, RestateTurnGatePeek};
use crate::object_state::StampedValue;

fn stamped(body: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&StampedValue {
        format: crate::durable_wait::DURABLE_WAIT_REGISTRY_FORMAT_VERSION,
        body,
    })
    .expect("encode a stamped registry row")
}

async fn peek_turn_gate(
    key: &AwaitEventKey,
    state: &BTreeMap<String, Vec<u8>>,
) -> Result<RestateTurnGatePeek, String> {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitRegistryImpl::default().serve())
        .build();
    let output = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "peek_turn_gate",
        fig1943_invocation_with_state(
            "fig3978-session",
            &RestateDurableWaitIndexRequest { key: key.clone() },
            state,
        ),
    )
    .await
    .expect("invoke peek_turn_gate");
    if let Some(failure) =
        restate_output_failure_message(&output).or_else(|| restate_error_message(&output))
    {
        return Err(failure);
    }
    Ok(restate_output_json(&output).expect("peek_turn_gate answers"))
}

/// A turn gate's peek is answered by the session index alone (FIG-3978): the
/// revocation fence, then the gate's terminal the index mirrored, or an open
/// gate. Only a turn's cancellation gate and its escalation are read this way.
#[tokio::test]
pub(super) async fn the_session_index_answers_a_turn_gate_peek_in_one_shared_read() {
    let session_id = SessionId::from("fig3978-session");
    let scope = ExecutionScope::turn(session_id.clone(), TurnId::from("turn-1"));
    let gate = restate_await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .expect("derive the turn's cancellation gate");
    let escalation = restate_await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelEscalation)
        .expect("derive the turn's escalation gate");
    let metadata = |revoked: bool| {
        let mut metadata = serde_json::to_value(RestateDurableWaitIndexMetadata::default())
            .expect("encode registry metadata");
        metadata["revoked"] = serde_json::json!(revoked);
        stamped(metadata)
    };
    let sealed = Resolution::Ok(serde_json::json!({ "state": "completion_sealed" }));

    assert_eq!(
        peek_turn_gate(&gate, &BTreeMap::new()).await,
        Ok(RestateTurnGatePeek::Open(None)),
        "a pristine index holds an open gate"
    );

    let mut state = BTreeMap::from([(
        crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY.to_string(),
        metadata(false),
    )]);
    assert_eq!(
        peek_turn_gate(&gate, &state).await,
        Ok(RestateTurnGatePeek::Open(None)),
        "a gate the index holds no terminal for is open"
    );
    state.insert(
        format!(
            "wait-index/v2/resolution/{}",
            RestateDurableWaitAddress::for_key(&gate).workflow_key
        ),
        stamped(serde_json::to_value(&sealed).expect("encode the gate's terminal")),
    );
    assert_eq!(
        peek_turn_gate(&gate, &state).await,
        Ok(RestateTurnGatePeek::Open(Some(sealed.clone()))),
        "the gate's mirrored terminal is its answer"
    );
    assert_eq!(
        peek_turn_gate(&escalation, &state).await,
        Ok(RestateTurnGatePeek::Open(None)),
        "the escalation gate reads its own terminal, not the base gate's"
    );

    state.insert(
        crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY.to_string(),
        metadata(true),
    );
    assert_eq!(
        peek_turn_gate(&gate, &state).await,
        Ok(RestateTurnGatePeek::Revoked),
        "a revoked session's gate has nothing to read"
    );

    let terminal = restate_await_event_key(&scope, AwaitEventWaitIdentity::TurnTerminal)
        .expect("derive the turn's terminal key");
    let refusal = peek_turn_gate(&terminal, &BTreeMap::new())
        .await
        .expect_err("a turn's terminal is not a cancellation gate");
    assert!(
        refusal.contains("peek_turn_gate reads a turn cancellation gate"),
        "{refusal}"
    );
}
