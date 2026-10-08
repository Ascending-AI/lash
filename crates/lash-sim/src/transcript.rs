use lash_sansio::SessionId;
use std::collections::{BTreeMap, BTreeSet};

use lash_core::testing::behavior_transcript::{Actor, Attr, Component, Entry, Kind, Transcript};

use crate::scheduler::{BoundaryKind, DeliveredBoundary};
use crate::store::{CheckpointComponentWriteKind, CheckpointWriteEvent};
use crate::trace::SimulationTrace;

/// Simulator runs interleave many sessions and are the longest transcripts the
/// repo renders; they are read as artifacts rather than as inline snapshots, so
/// they get a wider budget than an expect test.
const SIMULATION_REVIEW_BUDGET_LINES: usize = 4096;

impl SimulationTrace {
    /// The projection intentionally omits provider-wire `ProviderEvent`
    /// fragments; those remain in `SimulationTrace::events`. Durable-write lines
    /// cover commits made through observed session-store factories. Lash-core's
    /// `DurableProcessWorker` uses the engine's SQLite memory store set directly,
    /// so commits that bypass the observed factory are not represented here.
    pub fn render_transcript(&self) -> String {
        build(self, None).render()
    }

    /// The provider-wire and process-worker exclusions documented on
    /// [`SimulationTrace::render_transcript`] also apply.
    pub fn render_session_transcript(&self, session_id: &SessionId) -> String {
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
        if let Some(attribution) = write.attribution.as_ref() {
            let boundary_id = attribution.cause_boundary_id.as_str();
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

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn commit_entry(write: &CheckpointWriteEvent) -> Entry {
    let mut entry = Entry::commit(
        Actor::session(write.attributed_session().to_string()),
        write.revision_before,
        write.revision_after,
    );
    for component in &write.components {
        entry = entry.component(match &component.kind {
            CheckpointComponentWriteKind::Stored { logical_bytes } => {
                Component::stored(component.component.as_str(), *logical_bytes)
            }
            CheckpointComponentWriteKind::PluginState { state } => Component::stored_json(
                component.component.as_str(),
                serde_json::to_value(state).expect("decoded plugin state"),
            ),
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
        BoundaryKind::Ingress | BoundaryKind::QueuedIngress | BoundaryKind::ContractExecution => {
            Kind::Ingress
        }
        BoundaryKind::Provider | BoundaryKind::ProviderEvent => Kind::Provider,
        BoundaryKind::Tool => Kind::Tool,
        BoundaryKind::ExecCode => Kind::Exec,
        BoundaryKind::DurableEffect => Kind::Effect,
        BoundaryKind::Observer => Kind::Observe,
        BoundaryKind::Cancellation => Kind::Cancel,
        BoundaryKind::BackendFailure | BoundaryKind::ProviderMutation => Kind::Fault,
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
fn test_write(session_id: &SessionId, turn_index: usize) -> CheckpointWriteEvent {
    CheckpointWriteEvent {
        schema: crate::store::CHECKPOINT_WRITE_EVENT_SCHEMA.to_string(),
        session_id: session_id.clone(),
        attribution: None,
        commit_index: turn_index,
        turn_index,
        revision_before: (turn_index - 1) as u64,
        revision_after: turn_index as u64,
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
        crate::trace::OracleVerdict::passed(
            crate::oracles::GENERATED_WORKLOAD_BATTERY_ORACLE,
            "passed",
        ),
        Vec::new(),
        crate::trace::AbstractWorldView::with_digest(0, 0, Vec::new(), Vec::new()),
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
            vec![
                test_write(&SessionId::from("alpha"), 1),
                test_write(&SessionId::from("beta"), 1),
            ],
        );

        let transcript = trace.render_transcript();
        let lines = transcript.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 6, "{transcript}");
        // Two sessions interleave; every line, boundary and commit alike, must
        // name the actor it belongs to.
        let expected_actors = ["alpha", "beta", "alpha", "alpha", "beta", "beta"];
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
    fn contract_checkpoint_renders_after_its_causal_boundary() {
        let mut write = test_write(&SessionId::from("contract-store-session"), 1);
        write.attribution = Some(crate::store::CheckpointAttribution {
            session_id: SessionId::from("alpha"),
            cause_boundary_id: "alpha:2".to_string(),
        });
        let trace = trace_with_events(
            vec![
                test_boundary(1, "alpha", BoundaryKind::Ingress, 1),
                test_boundary(2, "alpha", BoundaryKind::ContractExecution, 1),
            ],
            vec![write],
        );

        let transcript = trace.render_transcript();
        let cause = transcript
            .find("ContractExecution")
            .expect("contract boundary line");
        let checkpoint = transcript
            .find("checkpoint.commit")
            .expect("checkpoint line");
        assert!(
            checkpoint > cause,
            "contract checkpoint rendered before its cause:\n{transcript}"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use lash_core::store::RuntimeCommit;
    use lash_core::{
        PluginState, RuntimeSessionState, SessionCatalogStore as _, SessionCreationHead,
        SessionRelation, SessionStoreCreateRequest, ToolState,
    };

    use super::*;
    use crate::store::{CheckpointWriteCollector, ObservedDeploymentStore};
    use crate::trace::{AbstractWorldView, OracleVerdict};

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

    async fn changed_component_commit(collector: CheckpointWriteCollector) -> CheckpointWriteEvent {
        let backend = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory store set");
        let factory =
            ObservedDeploymentStore::new(backend.session_store_factory(), collector.clone());
        factory
            .admit_session(&SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from("mutation-session"),
                relation: SessionRelation::Root,
                config: lash_core::PersistedSessionConfig::new(
                    lash_core::TurnBudget::Unbounded,
                    lash_core::MaxToolCalls::new(1024),
                    lash_core::NoProgressBudget::bounded(12),
                    lash_core::SessionToolAccess::ambient(),
                ),
                head: SessionCreationHead::Config,
                retention: lash_core::Retention::UntilGc,
            })
            .await
            .expect("create observed store");
        let store =
            lash_core::SessionStore::new(Arc::new(factory), SessionId::from("mutation-session"))
                .expect("valid session id");
        let mut state = RuntimeSessionState {
            session_id: SessionId::from("mutation-session"),
            turn_index: 1,
            ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
                lash_core::NoProgressBudget::bounded(12),
            ))
        };
        state.set_tool_state_snapshot(Some(tool_state(1)));
        state.set_plugin_state(Some(PluginState::default()));
        state.set_execution_state_snapshot(Some(b"first execution state".to_vec().into()));
        let first = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
            .await
            .expect("seed checkpoint component refs");
        state.apply_persisted_commit_result(first);

        state.turn_index = 2;
        state.set_tool_state_snapshot(Some(tool_state(2)));
        state.set_plugin_state(Some(PluginState::default()));
        state.set_execution_state_snapshot(Some(b"changed execution state".to_vec().into()));
        let second = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state))
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
            OracleVerdict::passed(crate::oracles::GENERATED_WORKLOAD_BATTERY_ORACLE, "passed"),
            Vec::new(),
            AbstractWorldView::with_digest(0, 0, Vec::new(), Vec::new()),
        )
    }
}
