use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex as StdMutex};

#[derive(Default)]
struct RecordingPhaseProbe {
    events: StdMutex<Vec<String>>,
}

impl RecordingPhaseProbe {
    fn events(&self) -> Vec<String> {
        self.events.lock_recover().clone()
    }

    fn record(&self, event: impl Into<String>) {
        self.events.lock_recover().push(event.into());
    }
}

impl RuntimeTurnPhaseProbe for RecordingPhaseProbe {
    fn begin(&self, phase: RuntimeTurnPhase) {
        self.record(format!("begin:{phase:?}"));
    }

    fn end(&self, phase: RuntimeTurnPhase) {
        self.record(format!("end:{phase:?}"));
    }

    fn begin_named(&self, phase: &str) {
        self.record(format!("begin_named:{phase}"));
    }

    fn end_named(&self, phase: &str) {
        self.record(format!("end_named:{phase}"));
    }
}

#[tokio::test]
async fn default_lease_timings_are_contractual_windows() {
    let backend = sqlite_memory_store_backend().await;
    let timings = lash_core::facade_support::LeaseTimings::default();
    assert_eq!(timings.ttl_ms(), 30_000);
    assert_eq!(timings.renew_interval_ms(), 10_000);
    assert_eq!(timings.ttl_ms(), timings.renew_interval_ms() * 3);
    assert!(
        lash_core::facade_support::LeaseTimings::new(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(1)
        )
        .is_err(),
        "a ttl below three renew intervals must be rejected"
    );
    let host = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    assert_eq!(host.control.lease_timings, timings);
}

#[test]
fn runtime_phase_probe_slot_routes_session_fallback_and_scope_override() {
    let slot = RuntimeTurnPhaseProbeSlot::default();
    let session_probe = Arc::new(RecordingPhaseProbe::default());
    let session_probe_dyn: Arc<dyn RuntimeTurnPhaseProbe> = session_probe.clone();
    slot.set_for_session("turn-session", session_probe_dyn);

    let frame_scope = lash_core::SessionScope::for_agent_frame(
        "turn-session",
        lash_core::facade_support::frame_node_id(&SessionId::from("turn-session"), "frame-a"),
    );
    let fallback_probe = slot
        .get_for_scope(&frame_scope)
        .expect("frame scope should inherit the session probe");
    fallback_probe.begin(RuntimeTurnPhase::PromptBuild);

    assert_eq!(session_probe.events(), vec!["begin:PromptBuild"]);
    assert!(
        slot.get_for_scope(&lash_core::SessionScope::new("unregistered"))
            .is_none()
    );

    let frame_probe = Arc::new(RecordingPhaseProbe::default());
    let frame_probe_dyn: Arc<dyn RuntimeTurnPhaseProbe> = frame_probe.clone();
    slot.set_for_scope(&frame_scope, frame_probe_dyn);

    let scoped_probe = slot
        .get_for_scope(&frame_scope)
        .expect("specific frame scope should override the session probe");
    scoped_probe.end(RuntimeTurnPhase::PreparedTurn);

    assert_eq!(frame_probe.events(), vec!["end:PreparedTurn"]);
    assert_eq!(session_probe.events(), vec!["begin:PromptBuild"]);
}

#[test]
fn runtime_named_phase_closes_the_named_probe_scope_on_drop() {
    let probe = Arc::new(RecordingPhaseProbe::default());
    let probe_dyn: Arc<dyn RuntimeTurnPhaseProbe> = probe.clone();

    {
        let _phase = RuntimeNamedPhase::begin(Some(probe_dyn), "queued_work.admission");
        assert_eq!(probe.events(), vec!["begin_named:queued_work.admission"]);
    }

    assert_eq!(
        probe.events(),
        vec![
            "begin_named:queued_work.admission",
            "end_named:queued_work.admission"
        ]
    );

    let _no_probe_phase = RuntimeNamedPhase::begin(None, "noop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_dispatch_preserves_probe_names_pairing_and_uninstrumented_calls() {
    use lash_core::plugin::{PluginDeclaration, PluginSpec, StaticPluginFactory};
    use lash_core::testing::TestTurnDrive as _;

    let double = kernel_double(0x1252, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let plugin = Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial("probe.fixture"),
        PluginSpec::new()
            .with_before_turn(Arc::new(|_| Box::pin(async { Ok(Vec::new()) })))
            .with_after_turn(Arc::new(|_| Box::pin(async { Ok(Vec::new()) })))
            .with_runtime_event(Arc::new(|event| {
                Box::pin(async move {
                    if matches!(event, PluginLifecycleEvent::TurnPersisted(_)) {
                        // Observer errors still close their named phase and leave
                        // the committed turn observable.
                        Err(PluginError::Session("observer failure".into()))
                    } else {
                        Ok(())
                    }
                })
            })),
    ));
    let mut runtime = runtime_with_plugins(
        &backend,
        vec![plugin],
        mock_provider(vec![
            MockCall {
                stream_events: Vec::new(),
                response: Ok(LlmResponse::default()),
            },
            MockCall {
                stream_events: Vec::new(),
                response: Ok(LlmResponse::default()),
            },
        ]),
    )
    .await;
    let probe = Arc::new(RecordingPhaseProbe::default());
    runtime.set_turn_phase_probe(probe.clone());
    for (turn_id, instrumented) in [("probed", true), ("unprobed", false)] {
        if !instrumented {
            runtime.turn_phase_probe = None;
        }
        let handler = double
            .open_handler(AdmittedScope::turn(
                runtime.state().session_id.clone(),
                TurnId::from(turn_id),
            ))
            .await
            .expect("open a turn handler");
        let turn = Box::pin(runtime.drive_turn(
            TurnInput::text("exercise plugin dispatch"),
            TurnOptions::new(CancellationToken::new(), handler.scoped()),
        ))
        .await
        .expect("drive the turn");
        assert!(
            turn.errors
                .iter()
                .any(|issue| issue.message.contains("observer failure"))
        );
    }
    let named = probe
        .events()
        .into_iter()
        .filter(|event| event.contains("_named:"))
        .collect::<Vec<_>>();
    let plugin_named = named
        .into_iter()
        .filter(|event| event.contains("plugin_hook."))
        .collect::<Vec<_>>();
    assert_eq!(
        plugin_named,
        [
            "begin_named:plugin_hook.before_turn.probe.fixture",
            "end_named:plugin_hook.before_turn.probe.fixture",
            "begin_named:plugin_hook.after_turn.probe.fixture",
            "end_named:plugin_hook.after_turn.probe.fixture",
            "begin_named:plugin_hook.turn_finalized.probe.fixture",
            "end_named:plugin_hook.turn_finalized.probe.fixture",
            "begin_named:plugin_hook.turn_persisted.probe.fixture",
            "end_named:plugin_hook.turn_persisted.probe.fixture",
        ]
    );
}
