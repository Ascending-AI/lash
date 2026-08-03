use super::*;
use std::sync::{Arc, Mutex as StdMutex};

#[derive(Default)]
struct RecordingPhaseProbe {
    events: StdMutex<Vec<String>>,
}

impl RecordingPhaseProbe {
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("recording phase probe").clone()
    }

    fn record(&self, event: impl Into<String>) {
        self.events
            .lock()
            .expect("recording phase probe")
            .push(event.into());
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

#[test]
fn default_lease_timings_are_contractual_windows() {
    let timings = crate::LeaseTimings::default();
    assert_eq!(timings.ttl_ms(), 30_000);
    assert_eq!(timings.renew_interval_ms(), 10_000);
    assert_eq!(timings.ttl_ms(), timings.renew_interval_ms() * 3);
    assert!(
        crate::LeaseTimings::new(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(1)
        )
        .is_err(),
        "a ttl below three renew intervals must be rejected"
    );
    let host = crate::RuntimeHostConfig::in_memory();
    assert_eq!(host.control.lease_timings, timings);
}

#[test]
fn runtime_phase_probe_slot_routes_session_fallback_and_scope_override() {
    let slot = RuntimeTurnPhaseProbeSlot::default();
    let session_probe = Arc::new(RecordingPhaseProbe::default());
    let session_probe_dyn: Arc<dyn RuntimeTurnPhaseProbe> = session_probe.clone();
    slot.set_for_session("turn-session", session_probe_dyn);

    let frame_scope = crate::SessionScope::for_agent_frame("turn-session", "frame-a");
    let fallback_probe = slot
        .get_for_scope(&frame_scope)
        .expect("frame scope should inherit the session probe");
    fallback_probe.begin(RuntimeTurnPhase::PromptBuild);

    assert_eq!(session_probe.events(), vec!["begin:PromptBuild"]);
    assert!(
        slot.get_for_scope(&crate::SessionScope::new("unregistered"))
            .is_none()
    );

    let frame_probe = Arc::new(RecordingPhaseProbe::default());
    let frame_probe_dyn: Arc<dyn RuntimeTurnPhaseProbe> = frame_probe.clone();
    slot.set_for_scope(&frame_scope, frame_probe_dyn);

    let scoped_probe = slot
        .get_for_scope(&frame_scope)
        .expect("specific frame scope should override the session probe");
    scoped_probe.end(RuntimeTurnPhase::FinalizeTurn);

    assert_eq!(frame_probe.events(), vec!["end:FinalizeTurn"]);
    assert_eq!(session_probe.events(), vec!["begin:PromptBuild"]);
}

#[test]
fn runtime_named_phase_closes_the_named_probe_scope_on_drop() {
    let probe = Arc::new(RecordingPhaseProbe::default());
    let probe_dyn: Arc<dyn RuntimeTurnPhaseProbe> = probe.clone();

    {
        let _phase = RuntimeNamedPhase::begin(Some(probe_dyn), "queued_work.claim");
        assert_eq!(probe.events(), vec!["begin_named:queued_work.claim"]);
    }

    assert_eq!(
        probe.events(),
        vec![
            "begin_named:queued_work.claim",
            "end_named:queued_work.claim"
        ]
    );

    let _no_probe_phase = RuntimeNamedPhase::begin(None, "noop");
}

#[test]
fn turn_context_live_plugin_inputs_are_typed_listed_and_durable_rejected() {
    let mut context = TurnContext::new();

    assert!(!context.has_live_plugin_inputs());
    assert!(!context.has_plugin_input("plugin_alpha"));
    assert!(
        context
            .live_plugin_inputs()
            .durable_effect_rejection()
            .is_ok()
    );

    context.insert_plugin_input("plugin_alpha", String::from("queued payload"));
    context.insert_plugin_input("plugin_beta", 7usize);

    assert!(context.has_live_plugin_inputs());
    assert!(context.has_plugin_input("plugin_alpha"));
    assert!(context.has_plugin_input("plugin_beta"));
    assert!(!context.has_plugin_input("missing"));
    assert_eq!(
        context
            .plugin_input::<String>("plugin_alpha")
            .map(String::as_str),
        Some("queued payload")
    );
    assert_eq!(
        context.plugin_input::<usize>("plugin_beta").copied(),
        Some(7)
    );
    assert!(context.plugin_input::<usize>("plugin_alpha").is_none());

    let mut plugin_ids = context.live_plugin_input_ids();
    plugin_ids.sort_unstable();
    assert_eq!(plugin_ids, vec!["plugin_alpha", "plugin_beta"]);

    let err = context
        .live_plugin_inputs()
        .durable_effect_rejection()
        .expect_err("durable effect hosts must reject process-local live inputs");
    assert_eq!(err.code, RuntimeErrorCode::DurableEffectLivePluginInput);
    assert!(err.message.contains("live TurnContext plugin inputs"));
}
