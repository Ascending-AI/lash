use super::effect::{RecordingEffectController, host_with_effect_recorder};
use super::*;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_f508;

#[tokio::test(flavor = "multi_thread")]
async fn observer_failure_is_advisory_and_keeps_committed_state() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let gate = Arc::new((
        tokio::sync::Notify::new(),
        tokio::sync::Notify::new(),
        AtomicBool::new(true),
    ));
    let recorder = RecordingEffectController::default().with_direct_gate(Arc::clone(&gate));

    let plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(RuntimeTestPluginFactory {
            build: Arc::new(move |_| {
                Ok(Arc::new(RuntimeTestPlugin {
                    before_turn: None,
                    checkpoint: None,
                    presentation_steps: vec![],
                    runtime_event: Some(Arc::new(move |event| {
                        Box::pin(async move {
                            if let lash_core::facade_support::PluginLifecycleEvent::TurnPersisted(
                                ctx,
                            ) = event
                            {
                                assert_eq!(ctx.state.session_id(), &ctx.session_id);
                                let snapshot = ctx.sessions.snapshot_current().await?;
                                assert_eq!(snapshot.session_id, ctx.session_id);
                            }
                            Err(lash_core::PluginError::Session(
                                "observer sink unavailable".into(),
                            ))
                        })
                    })),
                    external_registrar: None,
                }))
            }),
        });
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(vec![MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "finished".to_string(),
                    response_meta: None,
                }],
                ..LlmResponse::default()
            }),
        }]),
        host_with_effect_recorder(&backend, recorder.clone()),
    )
    .await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("hook-error-surfacing"),
        ))
        .await
        .expect("open the turn's handler");
    let scoped = lash_core::testing::LayeredEffectHost::layer_scoped(
        handler.scoped(),
        Arc::new(recorder.clone()),
    )
    .expect("layer the lent controller with the recorder");
    let turn = runtime
        .execute_turn(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                trace_turn_id: None,
                turn_context: lash_core::TurnContext::default(),
            },
            lash_core::facade_support::TurnOptions::new(CancellationToken::new(), scoped),
        )
        .await
        .expect("turn remains committed despite an observer-hook failure");
    handler.close().await.expect("close the turn's handler");

    assert!(turn.errors.iter().any(|issue| {
        issue.kind == lash_core::TurnFailureKind::Plugin
            && issue.code == Some(lash_core::TurnFailureCode::LifecycleHookFailed.into())
            && issue.retryable == Some(false)
            && issue.message.contains("observer sink unavailable")
            && issue.severity == lash_core::facade_support::TurnIssueSeverity::Advisory
            && issue.plugin_failures.len() == 1
    }));
    assert!(
        matches!(
            runtime.resident_session.validity(),
            ResidentSessionState::Valid
        ),
        "an observer failure preserves resident plugin state"
    );
}
