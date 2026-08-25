use std::collections::{BTreeMap, BTreeSet};

use lash_core::testing::behavior_transcript::{
    Actor, Attr, Component, Entry, Kind, Transcript, Usage,
};

use crate::scheduler::{BoundaryKind, DeliveredBoundary};
use crate::store::{CheckpointComponentWriteKind, CheckpointWriteEvent};
use crate::trace::SimulationTrace;

/// Simulator runs interleave many sessions and are the longest transcripts the
/// repo renders; they are read as artifacts rather than as inline snapshots, so
/// they get a wider budget than an expect test.
const SIMULATION_REVIEW_BUDGET_LINES: usize = 4096;

impl SimulationTrace {
    /// Render the completed run as a behavior transcript.
    ///
    /// The projection intentionally omits provider-wire `ProviderEvent`
    /// fragments; those remain in `SimulationTrace::events`. Durable-write lines
    /// cover commits made through observed session-store factories. Lash-core's
    /// `DurableProcessWorker` task body uses a bare in-memory store, so its
    /// internal checkpoint commits are not represented here.
    pub fn render_transcript(&self) -> String {
        build(self, None).render()
    }

    /// Render one raw simulator session with the same stable aliases used by the
    /// whole-run transcript. The provider-wire and process-worker exclusions
    /// documented on [`SimulationTrace::render_transcript`] also apply.
    pub fn render_session_transcript(&self, session_id: &str) -> String {
        build(self, Some(session_id)).render()
    }
}

fn build(trace: &SimulationTrace, session_filter: Option<&str>) -> Transcript {
    let boundaries = trace
        .events
        .iter()
        .filter(|event| {
            session_filter.is_none_or(|session_id| event.actor_alias == session_id)
                && event.kind != BoundaryKind::ProviderEvent
        })
        .collect::<Vec<_>>();
    let writes = trace
        .durable_writes
        .iter()
        .filter(|write| {
            session_filter.is_none_or(|session_id| write.attributed_session() == session_id)
        })
        .collect::<Vec<_>>();

    let mut transcript = Transcript::new().with_review_budget(SIMULATION_REVIEW_BUDGET_LINES);
    // The simulator already owns run-stable actor aliases: `actor_alias` on a
    // delivered boundary is the normalized name, and `trace.aliases` maps the
    // raw session ids of separately executed contract proofs onto the same
    // space. Pin both so the vocabulary keeps the simulator's identities instead
    // of re-normalizing an already-normalized name.
    for (session_id, alias) in &trace.aliases {
        transcript.pin(session_id.clone(), alias.clone());
    }
    for actor in boundaries
        .iter()
        .map(|boundary| boundary.actor_alias.as_str())
        .chain(writes.iter().map(|write| write.attributed_session()))
    {
        if !trace.aliases.contains_key(actor) {
            transcript.pin(actor.to_string(), actor.to_string());
        }
    }

    let mut writes_by_turn = BTreeMap::<(&str, usize), Vec<&CheckpointWriteEvent>>::new();
    let mut writes_by_boundary = BTreeMap::<&str, Vec<&CheckpointWriteEvent>>::new();
    for write in writes {
        if let Some(boundary_id) = write.cause_boundary_id.as_deref() {
            writes_by_boundary
                .entry(boundary_id)
                .or_default()
                .push(write);
        } else {
            writes_by_turn
                .entry((write.attributed_session(), write.turn_index))
                .or_default()
                .push(write);
        }
    }
    let mut rendered_writes = BTreeSet::<(&str, usize)>::new();
    let mut current_turn = BTreeMap::<&str, usize>::new();

    for boundary in boundaries {
        let turn_index = boundary_turn_index(boundary);
        let turn_changed = boundary.kind == BoundaryKind::Ingress
            || (boundary.kind == BoundaryKind::Provider
                && current_turn.get(boundary.actor_alias.as_str()) != Some(&turn_index));
        if turn_changed {
            current_turn.insert(
                &boundary.actor_alias,
                if boundary.kind == BoundaryKind::Ingress {
                    1
                } else {
                    turn_index
                },
            );
        }

        let is_resume = boundary
            .payload
            .get("suspend_resume")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if is_resume {
            transcript.record(Entry::new(
                Kind::Resume,
                actor_for(boundary),
                "session.resume",
            ));
        }

        transcript.record(boundary_entry(boundary, turn_changed));

        if boundary.kind == BoundaryKind::Ingress
            && boundary.observed.get("runtime_suspend").is_some()
        {
            transcript.record(Entry::new(Kind::Park, actor_for(boundary), "session.park"));
        }

        if (boundary.kind == BoundaryKind::Provider || is_resume)
            && let Some(turn_writes) =
                writes_by_turn.get(&(boundary.actor_alias.as_str(), turn_index))
        {
            for write in turn_writes {
                if rendered_writes.insert((write.attributed_session(), write.commit_index)) {
                    transcript.record(commit_entry(write));
                }
            }
        }
        if let Some(boundary_writes) = writes_by_boundary.get(boundary.boundary_id.as_str()) {
            for write in boundary_writes {
                if rendered_writes.insert((write.attributed_session(), write.commit_index)) {
                    transcript.record(commit_entry(write));
                }
            }
        }
    }

    for pending_writes in writes_by_turn.values().chain(writes_by_boundary.values()) {
        for write in pending_writes {
            if rendered_writes.insert((write.attributed_session(), write.commit_index)) {
                transcript
                    .record(commit_entry(write).attr(Attr::int("turn", write.turn_index as u64)));
            }
        }
    }

    transcript
}

fn actor_for(boundary: &DeliveredBoundary) -> Actor {
    Actor::session(boundary.actor_alias.clone())
}

fn boundary_entry(boundary: &DeliveredBoundary, turn_changed: bool) -> Entry {
    let mut entry = Entry::new(
        boundary_kind(boundary.kind),
        actor_for(boundary),
        boundary_label(boundary),
    );
    if turn_changed {
        entry = entry.attr(Attr::int("turn", boundary_turn_index(boundary) as u64));
    }
    match boundary.kind {
        BoundaryKind::Provider => {
            if let Some(provider) = observed_str(boundary, "provider_kind") {
                entry = entry.attr(Attr::text("model", provider));
            }
        }
        BoundaryKind::Tool | BoundaryKind::ExecCode => {
            if let Some(name) = observed_str(boundary, "tool_name").or_else(|| {
                boundary
                    .payload
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
            }) {
                entry = entry.attr(Attr::text("name", name));
            }
        }
        BoundaryKind::DurableEffect => {
            if let Some(key) = observed_str(boundary, "durable_key") {
                entry = entry.attr(Attr::text("key", key));
            }
        }
        BoundaryKind::LeaseTime => {
            if let Some(tick) = boundary
                .observed
                .get("lease_time_tick")
                .and_then(serde_json::Value::as_u64)
            {
                entry = entry.attr(Attr::int("tick", tick));
            }
        }
        BoundaryKind::ProcessWake => {
            if let Some(reason) = observed_str(boundary, "discard_reason") {
                entry = entry.attr(Attr::token("discard", reason));
            }
        }
        BoundaryKind::ProcessLifecycle => {
            if let Some(outcome) = observed_str(boundary, "outcome") {
                entry = entry.attr(Attr::token("outcome", outcome));
            }
        }
        BoundaryKind::Observer => {
            if let Some(visibility) = observed_str(boundary, "visibility") {
                entry = entry.attr(Attr::token("visibility", visibility));
            }
        }
        _ => {}
    }
    entry
}

fn observed_str<'event>(boundary: &'event DeliveredBoundary, field: &str) -> Option<&'event str> {
    boundary
        .observed
        .get(field)
        .and_then(serde_json::Value::as_str)
}

fn commit_entry(write: &CheckpointWriteEvent) -> Entry {
    let mut entry = Entry::commit(
        Actor::session(write.attributed_session().to_string()),
        write.revision_before,
        write.revision_after,
        Usage::new(
            write.usage.entries,
            write.usage.input_tokens,
            write.usage.output_tokens,
            write.usage.cache_read_input_tokens,
            write.usage.cache_write_input_tokens,
            write.usage.reasoning_output_tokens,
        ),
    );
    for component in &write.components {
        entry = entry.component(match &component.kind {
            CheckpointComponentWriteKind::Stored { logical_bytes } => {
                Component::stored(component.component.as_str(), *logical_bytes)
            }
            CheckpointComponentWriteKind::UnchangedRef => {
                Component::unchanged_ref(component.component.as_str())
            }
        });
    }
    entry
}

/// Boundary classes the simulator schedules, projected onto the shared
/// vocabulary. `ProviderEvent` is filtered out before this point.
fn boundary_kind(kind: BoundaryKind) -> Kind {
    match kind {
        BoundaryKind::Ingress | BoundaryKind::QueuedIngress | BoundaryKind::Trigger => {
            Kind::Ingress
        }
        BoundaryKind::Provider | BoundaryKind::ProviderEvent => Kind::Provider,
        BoundaryKind::Tool => Kind::Tool,
        BoundaryKind::ExecCode => Kind::Exec,
        BoundaryKind::DurableEffect => Kind::Effect,
        BoundaryKind::ProcessWake => Kind::Wake,
        BoundaryKind::ProcessLifecycle => Kind::Outcome,
        BoundaryKind::Worker => Kind::Worker,
        BoundaryKind::Observer => Kind::Observe,
        BoundaryKind::Cancellation => Kind::Cancel,
        BoundaryKind::BackendFailure | BoundaryKind::ProviderMutation => Kind::Fault,
        BoundaryKind::LeaseTime => Kind::Lease,
    }
}

fn boundary_turn_index(event: &DeliveredBoundary) -> usize {
    event
        .observed
        .get("turn_index")
        .or_else(|| event.payload.get("turn_index"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as usize
}

fn boundary_label(event: &DeliveredBoundary) -> &str {
    if event.kind == BoundaryKind::Provider {
        "provider.chat.stream"
    } else {
        &event.label
    }
}

#[cfg(test)]
fn test_boundary(
    sequence: usize,
    actor_alias: &str,
    kind: BoundaryKind,
    turn_index: usize,
) -> DeliveredBoundary {
    DeliveredBoundary {
        schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
        sequence,
        scheduler: Default::default(),
        boundary_id: format!("{actor_alias}:{sequence}"),
        actor_alias: actor_alias.to_string(),
        kind,
        at: sequence as u64,
        label: format!("{kind:?}"),
        payload: serde_json::json!({"turn_index": turn_index}),
        observed: serde_json::json!({"turn_index": turn_index}),
    }
}

#[cfg(test)]
fn test_write(session_id: &str, turn_index: usize) -> CheckpointWriteEvent {
    CheckpointWriteEvent {
        schema: crate::store::CHECKPOINT_WRITE_EVENT_SCHEMA.to_string(),
        session_id: session_id.to_string(),
        attributed_session_id: None,
        cause_boundary_id: None,
        commit_index: turn_index,
        turn_index,
        revision_before: (turn_index - 1) as u64,
        revision_after: turn_index as u64,
        usage: Default::default(),
        components: Vec::new(),
        state: None,
    }
}

#[cfg(test)]
fn trace_with_events(
    events: Vec<DeliveredBoundary>,
    writes: Vec<CheckpointWriteEvent>,
) -> SimulationTrace {
    SimulationTrace::new(
        1,
        "test-generator",
        "test",
        "1/1",
        "transcript-attribution",
        "test-workload",
        "test-script-bundle",
        crate::trace::WorkloadExpectations::default(),
        BTreeMap::new(),
        events,
        writes,
        crate::trace::OracleVerdict::passed("sim.oracle.generated-workload.v1", "passed"),
        Vec::new(),
        crate::trace::AbstractWorldSummary::with_digest(0, 0, Vec::new(), Vec::new(), Vec::new()),
    )
}

#[cfg(test)]
mod attribution_tests {
    use super::*;

    #[test]
    fn interleaved_whole_run_labels_every_boundary_and_checkpoint() {
        let trace = trace_with_events(
            vec![
                test_boundary(1, "alpha", BoundaryKind::Ingress, 1),
                test_boundary(2, "beta", BoundaryKind::Ingress, 1),
                test_boundary(3, "alpha", BoundaryKind::Provider, 1),
                test_boundary(4, "beta", BoundaryKind::Provider, 1),
            ],
            vec![test_write("alpha", 1), test_write("beta", 1)],
        );

        let transcript = trace.render_transcript();
        let lines = transcript.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 8, "{transcript}");
        // Two sessions interleave; every line, boundary and commit alike, must
        // name the actor it belongs to.
        let expected_actors = [
            "alpha", "beta", "alpha", "alpha", "alpha", "beta", "beta", "beta",
        ];
        for (line, actor) in lines.iter().zip(expected_actors) {
            assert!(
                line.starts_with(actor),
                "line `{line}` was not attributed to {actor}: {transcript}"
            );
        }
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("checkpoint.commit"))
                .count(),
            2,
            "both sessions must render their own commit: {transcript}"
        );
    }

    #[test]
    fn contract_checkpoint_renders_after_its_causal_trigger() {
        let mut write = test_write("contract-store-session", 1);
        write.attributed_session_id = Some("alpha".to_string());
        write.cause_boundary_id = Some("alpha:2".to_string());
        let trace = trace_with_events(
            vec![
                test_boundary(1, "alpha", BoundaryKind::Ingress, 1),
                test_boundary(2, "alpha", BoundaryKind::Trigger, 1),
            ],
            vec![write],
        );

        let transcript = trace.render_transcript();
        let trigger = transcript.find("Trigger").expect("trigger line");
        let checkpoint = transcript
            .find("checkpoint.commit")
            .expect("checkpoint line");
        assert!(
            checkpoint > trigger,
            "contract checkpoint rendered before its cause:\n{transcript}"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash_core::store::RuntimeCommit;
    use lash_core::{
        PluginSessionSnapshot, ProcessAwaitOutput, ProcessCompletionAuthority,
        ProcessEventAppendRequest, ProcessEventSemanticsSpec, ProcessEventType,
        ProcessRegistry as _, ProcessValueSelector, ProcessWakeSpec, ProjectionWatermark,
        RecoveryContract, RuntimeSessionState, SessionRelation, SessionStoreCreateRequest,
        SessionStoreFactory as _, ToolState, facade_support::InMemorySessionStoreFactory,
        facade_support::ProcessChangeHub,
    };

    use super::*;
    use crate::store::{CheckpointWriteCollector, ObservedSessionStoreFactory};
    use crate::trace::{AbstractWorldSummary, OracleVerdict};

    #[tokio::test]
    async fn transcript_discriminates_missing_checkpoint_component_bodies() {
        let correct = changed_component_commit(CheckpointWriteCollector::default()).await;
        let defect = changed_component_commit(CheckpointWriteCollector::with_ref_only_mutation(
            "mutation-session",
            1,
        ))
        .await;

        let correct = trace_with_write(correct).render_transcript();
        let defect = trace_with_write(defect).render_transcript();

        assert_ne!(
            correct, defect,
            "the real defect must change the transcript"
        );
        assert!(
            correct
                .lines()
                .any(|line| line.contains("tool_state") && line.contains("stored logical=")),
            "control transcript must show the changed body was stored: {correct}"
        );
        assert!(
            defect
                .lines()
                .any(|line| line.contains("tool_state") && line.contains("ref (unchanged)")),
            "mutated transcript must expose the missing body: {defect}"
        );
    }

    /// The retarget/prune cutover is checked against the registry's own reported
    /// facts. It deliberately does **not** snapshot a transcript: this test runs
    /// no scheduler, so the only boundary events available to render would be
    /// ones the test constructed itself, which ADR 0044 rules out. Transcript
    /// coverage of real boundary shapes lives in the scenario harnesses.
    #[tokio::test]
    async fn process_cutover_reports_retarget_discard_and_pruned_await_as_information() {
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        let process_id = "transcript-process";
        registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    process_id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    RecoveryContract::ExternallyOwned,
                    lash_core::ProcessProvenance::host(),
                )
                .with_extra_event_types([ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: ProcessEventSemanticsSpec {
                        wake: Some(ProcessWakeSpec {
                            when: Some(ProcessValueSelector::Present("/wake_input".to_string())),
                            input: ProcessValueSelector::Pointer("/wake_input".to_string()),
                        }),
                        ..ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some("source-session".to_string())),
            )
            .await
            .expect("register transcript process");
        registry
            .append_event(
                process_id,
                ProcessEventAppendRequest::new(
                    "producer.wake",
                    serde_json::json!({"wake_input": "resume"}),
                ),
            )
            .await
            .expect("append wake event");
        registry
            .retarget_subscription(process_id, Some("branch-session"))
            .await
            .expect("retarget subscription");
        let retargeted = registry
            .list_wake_deliveries(None)
            .await
            .expect("read wake delivery")
            .into_iter()
            .find(|delivery| {
                delivery.disposition.discard_reason()
                    == Some(lash_core::WakeDiscardReason::Retargeted)
            })
            .expect("retargeted delivery");
        assert_eq!(
            retargeted
                .disposition
                .discard_reason()
                .expect("retargeted delivery reason")
                .as_str(),
            "retargeted",
            "a retarget must settle its stale wake delivery as retargeted"
        );
        let retarget_event = registry
            .events_after(process_id, 0)
            .await
            .expect("read process audit events")
            .into_iter()
            .find(|event| event.event_type == "process.subscription_retargeted")
            .expect("retarget audit event");
        assert_eq!(retarget_event.event_type, "process.subscription_retargeted");

        let terminal = registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::json!({"done": true}),
                    control: None,
                },
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete transcript process");
        registry
            .prune_terminal_processes(
                terminal.updated_at_ms.saturating_add(1),
                None,
                ProjectionWatermark::NoProjector,
            )
            .await
            .expect("prune transcript process");
        let output = lash_core::facade_support::ProcessAwaiter::new(
            registry as Arc<dyn lash_core::ProcessRegistry>,
            ProcessChangeHub::new(),
        )
        .await_terminal(process_id)
        .await
        .expect("await pruned process");
        assert!(matches!(
            output,
            ProcessAwaitOutput::NoLongerRetained { .. }
        ));
        let rendered_output = output.into_tool_output();
        let rendered_value = match rendered_output.outcome {
            lash_core::ToolCallOutcome::Success(value) => value.to_json_value(),
            other => panic!("pruned await must render as information success, got {other:?}"),
        };
        assert_eq!(
            rendered_value
                .get("type")
                .and_then(serde_json::Value::as_str),
            Some("information"),
            "a pruned await must not be reported as a failure: {rendered_value}"
        );
        assert_eq!(
            rendered_value
                .get("code")
                .and_then(serde_json::Value::as_str),
            Some("process_no_longer_retained"),
            "a pruned await must report retention loss by code: {rendered_value}"
        );
    }

    async fn changed_component_commit(collector: CheckpointWriteCollector) -> CheckpointWriteEvent {
        let factory = ObservedSessionStoreFactory::new(
            Arc::new(InMemorySessionStoreFactory::new()),
            collector.clone(),
        );
        let store = factory
            .create_store(&SessionStoreCreateRequest {
                session_id: "mutation-session".to_string(),
                relation: SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .expect("create observed store");
        let mut state = RuntimeSessionState {
            session_id: "mutation-session".to_string(),
            turn_index: 1,
            plugin_snapshot_revision: Some(1),
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
            ))
        };
        state.set_tool_state_snapshot(Some(tool_state(1)));
        state.set_plugin_snapshot(Some(PluginSessionSnapshot::default()));
        state.set_execution_state_snapshot(Some(b"first execution state".to_vec()));
        let first = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("seed checkpoint component refs");
        state.apply_persisted_commit_result(first);

        state.turn_index = 2;
        state.set_tool_state_snapshot(Some(tool_state(2)));
        state.set_plugin_snapshot(Some(PluginSessionSnapshot::default()));
        state.plugin_snapshot_revision = Some(2);
        state.set_execution_state_snapshot(Some(b"changed execution state".to_vec()));
        let second = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("commit changed checkpoint components");
        assert_eq!(second.head_revision, 2);

        collector
            .events()
            .into_iter()
            .find(|write| write.revision_before == 1)
            .expect("observed second commit")
    }

    fn tool_state(generation: u64) -> ToolState {
        serde_json::from_value(serde_json::json!({
            "generation": generation,
            "tools": {}
        }))
        .expect("construct tool state")
    }

    fn trace_with_write(write: CheckpointWriteEvent) -> SimulationTrace {
        SimulationTrace::new(
            1,
            "test-generator",
            "test",
            "1/1",
            "transcript-mutation",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "test-script-bundle",
            crate::trace::WorkloadExpectations::default(),
            BTreeMap::new(),
            Vec::new(),
            vec![write],
            OracleVerdict::passed("sim.oracle.generated-workload.v1", "passed"),
            Vec::new(),
            AbstractWorldSummary::with_digest(0, 0, Vec::new(), Vec::new(), Vec::new()),
        )
    }
}
