// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_f45a;

/// An active-turn input admitted at a non-terminal checkpoint is settled by
/// the in-flight turn's commit, and hosts see that in exactly one
/// `ingress.settled` record under that turn and its root (ADR 0101, FIG-3927
/// amendment: settlement is keyed by the root and the turn). The workbench's
/// live turn-ingress law reads this record; FIG-3946 renamed it from
/// `turn_input.completed` without that reader, which only the hourly e2e saw.
#[tokio::test]
pub(super) async fn active_input_settles_once_under_the_in_flight_turn_in_the_trace() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let trace_path = std::env::temp_dir().join(format!(
        "lash-active-input-settled-trace-{}.jsonl",
        uuid::Uuid::new_v4()
    ));
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::ToolCall {
                    call_id: "settle-call-1".to_string(),
                    tool_name: "echo_tool".to_string(),
                    input_json: json!({ "value": "work" }).to_string(),
                    replay: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "done".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let store = double_unbound_recording_store(&double).await;
    let runtime_store: Arc<dyn lash_core::RuntimeStore> = store.clone();
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EchoTool),
        transport,
        test_host_config_with_trace_path(&backend, trace_path.clone()),
        runtime_store,
    )
    .await;
    let turn_id = TurnId::from("active-input-settled-turn");
    let admitted = enqueue_turn_input_for_checkpoint(
        store.as_ref(),
        &SessionId::from("root"),
        &turn_id,
        Some("host:active-input-settled".to_string()),
        TurnInput::text("mid-turn input"),
    )
    .await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            turn_id.clone(),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .drive_turn(
            TurnInput::text("use the tool, then answer"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    // The tool step is not terminal, so the in-flight turn itself absorbs
    // the input rather than a follow-on turn.
    assert!(
        active_conversation_messages(&turn.state)
            .iter()
            .any(|message| matches!(
                message.origin.as_ref(),
                Some(lash_core::MessageOrigin::TurnInput { turn_id: absorbed, input_id })
                    if absorbed == turn_id.as_str()
                        && input_id.as_deref() == Some(admitted.input_id.as_str())
            )),
        "the in-flight turn must absorb the active input"
    );
    let settlements = lash_trace::parse_jsonl_records::<serde_json::Value>(
        &std::fs::read_to_string(&trace_path).expect("read the turn's trace"),
    )
    .expect("trace records")
    .into_iter()
    .filter(|record| {
        record.get("name").and_then(serde_json::Value::as_str) == Some("ingress.settled")
            && record
                .pointer("/payload/input_ids")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|ids| {
                    ids.iter()
                        .any(|id| id.as_str() == Some(admitted.input_id.as_str()))
                })
    })
    .map(|record| {
        (
            record
                .pointer("/context/turn_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            record
                .pointer("/payload/root")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        )
    })
    .collect::<Vec<_>>();
    let _ = std::fs::remove_file(&trace_path);
    assert_eq!(
        settlements,
        vec![(Some(turn_id.to_string()), Some(turn_id.to_string()))],
        "the active input settles exactly once, under the in-flight turn and its root"
    );
}
