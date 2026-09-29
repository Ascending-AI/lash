//! FIG-4044: work withheld at a terminal checkpoint across a frame switch.

use super::*;
use lash_core::testing::TestTurnDrive as _;

const SEED: u64 = 0x5_f470;

/// FIG-4044: work withheld at a `BeforeCompletion` checkpoint waits out a
/// frame switch that follows it.
///
/// A terminal checkpoint that also delivers something (here a plugin's
/// message) re-enters the protocol loop, and the next model call may switch
/// frames. The switch's own follow-on turn then runs before the FIG-3157
/// follow-on that drives the withheld wake. That frame turn still owes the
/// FIG-3157 follow-on, so its commit writes no root terminal and releases
/// nothing: the wake stays bound to the root, and the follow-on completes it
/// in the same run.
#[tokio::test]
pub(super) async fn work_withheld_before_a_frame_switch_waits_for_its_follow_on() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    const SESSION_ID: &str = "withheld-across-frame-switch";
    let root = TurnId::from("withheld-across-frame-switch-turn");

    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    type WakeSource = (
        Arc<dyn lash_core::ProcessRegistry>,
        Arc<RecordingStore>,
        ProcessId,
    );
    let wake_source: Arc<Mutex<Option<WakeSource>>> = Arc::new(Mutex::new(None));
    let captured_wake_source = Arc::clone(&wake_source);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            let captured_calls = Arc::clone(&captured_calls);
            let captured_wake_source = Arc::clone(&captured_wake_source);
            async move {
                captured_requests.lock_recover().push(req);
                let source = captured_wake_source.lock_recover().take();
                if let Some((registry, store, process)) = source {
                    append_process_wake_to_queue(
                        registry.as_ref(),
                        store.as_ref(),
                        &process,
                        lash_core::ProcessEventAppendRequest::new(
                            "process.wake",
                            json!({
                                "text": "wake withheld across the switch",
                                "value": { "status": "wake withheld across the switch" }
                            }),
                        ),
                    )
                    .await;
                }
                let text = match captured_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => "committed answer",
                    1 => "frame answer",
                    _ => "wake answer",
                };
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    // The protocol switches frames at the model call the terminal
    // checkpoint's delivery leads to, and only there.
    let protocol = Arc::new(super::SwitchBeforeLlmProtocol {
        executor: None,
        frame_key_material: "withheld-across-frame-switch-frame".to_string(),
        switch_next: AtomicBool::new(false),
    });
    let protocol_factory = lash_core::testing::test_standard_protocol_factory_with_runtime_state(
        Arc::clone(&protocol) as Arc<dyn lash_core::plugin::ProtocolSessionPlugin>,
        None,
    );
    let injected = Arc::new(AtomicBool::new(false));
    let checkpoint_plugin = Arc::new(RuntimeTestPluginFactory {
        build: Arc::new(move |_| {
            let protocol = Arc::clone(&protocol);
            let injected = Arc::clone(&injected);
            Ok(Arc::new(RuntimeTestPlugin {
                before_turn: None,
                checkpoint: Some(Arc::new(move |ctx| {
                    let protocol = Arc::clone(&protocol);
                    let injected = Arc::clone(&injected);
                    Box::pin(async move {
                        if ctx.checkpoint != lash_core::CheckpointKind::BeforeCompletion
                            || injected.swap(true, Ordering::SeqCst)
                        {
                            return Ok(Vec::new());
                        }
                        protocol.switch_next.store(true, Ordering::SeqCst);
                        Ok(vec![
                            lash_core::facade_support::TurnPluginDirective::EnqueueMessages(
                                lash_core::facade_support::EnqueueMessagesDirective {
                                    messages: vec![lash_core::PluginMessage::text(
                                        lash_core::MessageRole::System,
                                        "one more step before finishing",
                                    )],
                                },
                            ),
                        ])
                    })
                })),
                presentation_steps: vec![],
                runtime_event: None,
                external_registrar: None,
            }))
        }),
    });
    let backend = double.lash_backend();
    let store = double_unbound_recording_store(&double).await;
    let mut runtime = TestRuntime::new(&backend, transport)
        .tools(Arc::new(EmptyTools))
        .plugins(vec![protocol_factory, checkpoint_plugin])
        .host(test_host_config(&backend))
        .store(store.clone())
        .with_session_id(SESSION_ID)
        .build()
        .await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new(SESSION_ID);
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::session(target_scope.clone()),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    *wake_source.lock_recover() = Some((
        Arc::clone(&registry),
        Arc::clone(&store),
        registered.id.clone(),
    ));

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from(SESSION_ID),
            root.clone(),
        ))
        .await
        .expect("open the turn's handler");
    let run = runtime
        .drive_turn_frames(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        )
        .await
        .expect("the run drives the switch and the withheld wake");
    handler.close().await.expect("close the turn's handler");

    let outcomes = run
        .turns
        .iter()
        .map(|turn| format!("{:?}", turn.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        run.turns.len(),
        3,
        "the switched turn, its frame's follow-on and the wake's follow-on: {outcomes:?}"
    );
    assert!(
        matches!(run.turns[0].outcome, TurnOutcome::AgentFrameSwitch { .. }),
        "the terminal checkpoint's delivery led to a frame switch: {:?}",
        run.turns[0].outcome
    );
    for (index, turn) in run.turns.iter().enumerate() {
        assert!(
            turn.errors
                .iter()
                .all(|issue| issue.severity == lash_core::runtime::TurnIssueSeverity::Advisory),
            "turn {index} reports no failure: {:?}",
            turn.errors
        );
    }
    assert_eq!(run.turns[1].assistant_output.safe_text, "frame answer");
    assert_eq!(run.turns[2].assistant_output.safe_text, "wake answer");
    let requests = requests.lock_recover().clone();
    assert_eq!(requests.len(), 3);
    assert!(
        request_contains_text(&requests[2], "wake withheld across the switch"),
        "the wake is the last follow-on's input"
    );

    let session = SessionId::from(SESSION_ID);
    assert!(
        lash_core::store::IngressStore::list_queued_work(store.as_ref(), &session)
            .await
            .expect("queued work after the run")
            .is_empty(),
        "the wake's follow-on completed it"
    );
    assert!(
        lash_core::store::RootStore::root_terminal(store.as_ref(), &session, &root)
            .await
            .expect("read the root's terminal")
            .is_some(),
        "the root ends with the run"
    );
    assert!(
        lash_core::store::RootStore::unfinished_root(store.as_ref(), &session)
            .await
            .expect("read the unfinished root")
            .is_none(),
        "no unfinished root holds the session"
    );
}
