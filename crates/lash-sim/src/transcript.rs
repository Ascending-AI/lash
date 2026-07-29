use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::scheduler::{BoundaryKind, DeliveredBoundary};
use crate::store::{CheckpointComponentWriteKind, CheckpointWriteEvent};
use crate::trace::SimulationTrace;

impl SimulationTrace {
    /// Render the completed run as deterministic, review-oriented text.
    ///
    /// The compact projection intentionally omits provider-wire `ProviderEvent`
    /// fragments; those remain in `SimulationTrace::events`. Checkpoint lines
    /// cover commits made through observed session-store factories. Lash-core's
    /// `DurableProcessWorker` task body uses a bare in-memory store, so its
    /// internal checkpoint commits are not represented here.
    pub fn render_transcript(&self) -> String {
        render(self, None)
    }

    /// Render one raw simulator session with the same stable alias used by the
    /// whole-run transcript. The provider-wire and process-worker exclusions
    /// documented on [`SimulationTrace::render_transcript`] also apply.
    pub fn render_session_transcript(&self, session_id: &str) -> String {
        render(self, Some(session_id))
    }
}

fn render(trace: &SimulationTrace, session_filter: Option<&str>) -> String {
    let show_session_column = session_filter.is_none();
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

    let mut aliases = TranscriptAliases::new(trace);
    for boundary in &boundaries {
        aliases.alias(&boundary.actor_alias);
    }
    for write in &writes {
        aliases.alias(write.attributed_session());
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
    let mut output = String::new();
    let mut sequence = 0usize;
    let mut current_turn = BTreeMap::<&str, usize>::new();

    for boundary in boundaries {
        let alias = aliases.alias(&boundary.actor_alias);
        let turn_index = boundary_turn_index(boundary);
        if boundary.kind == BoundaryKind::Ingress {
            current_turn.insert(&boundary.actor_alias, 1);
            write_turn_header(&mut output, 1, &alias, show_session_column);
        } else if boundary.kind == BoundaryKind::Provider
            && current_turn.get(boundary.actor_alias.as_str()) != Some(&turn_index)
        {
            current_turn.insert(&boundary.actor_alias, turn_index);
            write_turn_header(&mut output, turn_index, &alias, show_session_column);
        }

        let is_resume = boundary
            .payload
            .get("suspend_resume")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if is_resume {
            write_session_marker(&mut output, "resume", &alias, show_session_column);
        }

        sequence += 1;
        write_boundary_line(&mut output, sequence, boundary, &alias, show_session_column);

        if boundary.kind == BoundaryKind::Ingress
            && boundary.observed.get("runtime_suspend").is_some()
        {
            write_session_marker(&mut output, "park", &alias, show_session_column);
        }

        if (boundary.kind == BoundaryKind::Provider || is_resume)
            && let Some(turn_writes) =
                writes_by_turn.get(&(boundary.actor_alias.as_str(), turn_index))
        {
            for write in turn_writes {
                if rendered_writes.contains(&(write.attributed_session(), write.commit_index)) {
                    continue;
                }
                sequence += 1;
                write_checkpoint(&mut output, sequence, write, &alias, show_session_column);
                rendered_writes.insert((write.attributed_session(), write.commit_index));
            }
        }
        if let Some(boundary_writes) = writes_by_boundary.get(boundary.boundary_id.as_str()) {
            for write in boundary_writes {
                if rendered_writes.contains(&(write.attributed_session(), write.commit_index)) {
                    continue;
                }
                sequence += 1;
                write_checkpoint(&mut output, sequence, write, &alias, show_session_column);
                rendered_writes.insert((write.attributed_session(), write.commit_index));
            }
        }
    }

    for pending_writes in writes_by_turn.values().chain(writes_by_boundary.values()) {
        for write in pending_writes {
            if rendered_writes.contains(&(write.attributed_session(), write.commit_index)) {
                continue;
            }
            let alias = aliases.alias(write.attributed_session());
            write_turn_header(&mut output, write.turn_index, &alias, show_session_column);
            sequence += 1;
            write_checkpoint(&mut output, sequence, write, &alias, show_session_column);
        }
    }

    output.trim_end().to_string()
}

struct TranscriptAliases {
    aliases: BTreeMap<String, String>,
    next: usize,
}

impl TranscriptAliases {
    fn new(trace: &SimulationTrace) -> Self {
        let next = trace
            .aliases
            .values()
            .filter_map(|alias| alias.strip_prefix("session-"))
            .filter_map(|suffix| suffix.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        Self {
            aliases: trace.aliases.clone(),
            next,
        }
    }

    fn alias(&mut self, session_id: &str) -> String {
        if let Some(alias) = self.aliases.get(session_id) {
            return alias.clone();
        }
        if self.aliases.values().any(|alias| alias == session_id) {
            return session_id.to_string();
        }
        self.next += 1;
        let alias = format!("session-{:03}", self.next);
        self.aliases.insert(session_id.to_string(), alias.clone());
        alias
    }
}

fn write_turn_header(output: &mut String, turn_index: usize, alias: &str, show_session: bool) {
    if show_session {
        writeln!(output, "{alias:<11} turn {turn_index}").expect("write transcript");
    } else {
        writeln!(output, "turn {turn_index}  {alias}").expect("write transcript");
    }
}

fn write_session_marker(output: &mut String, marker: &str, alias: &str, show_session: bool) {
    if show_session {
        writeln!(output, "{alias:<11} {marker}").expect("write transcript");
    } else {
        writeln!(output, "{marker:<6} {alias}").expect("write transcript");
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

fn write_boundary_line(
    output: &mut String,
    sequence: usize,
    event: &DeliveredBoundary,
    alias: &str,
    show_session: bool,
) {
    let class = boundary_class(event.kind);
    let label = boundary_label(event);
    if show_session {
        write!(output, "{alias:<11} {sequence:04}  {class:<18} {label}").expect("write transcript");
    } else {
        write!(output, "  {sequence:04}  {class:<18} {label}").expect("write transcript");
    }
    match event.kind {
        BoundaryKind::Provider => {
            if let Some(provider) = event
                .observed
                .get("provider_kind")
                .and_then(serde_json::Value::as_str)
            {
                write!(output, "  model={provider}").expect("write transcript");
            }
        }
        BoundaryKind::Tool | BoundaryKind::ExecCode => {
            if let Some(name) = event
                .observed
                .get("tool_name")
                .or_else(|| event.payload.get("tool"))
                .and_then(serde_json::Value::as_str)
            {
                write!(output, "  name={name}").expect("write transcript");
            }
        }
        BoundaryKind::DurableEffect => {
            if let Some(key) = event
                .observed
                .get("durable_key")
                .and_then(serde_json::Value::as_str)
            {
                write!(output, "  key={key}").expect("write transcript");
            }
        }
        BoundaryKind::LeaseTime => {
            if let Some(tick) = event
                .observed
                .get("lease_time_tick")
                .and_then(serde_json::Value::as_u64)
            {
                write!(output, "  tick={tick}").expect("write transcript");
            }
        }
        BoundaryKind::ProcessWake => {
            if let Some(reason) = event
                .observed
                .get("discard_reason")
                .and_then(serde_json::Value::as_str)
            {
                write!(output, "  discard={reason}").expect("write transcript");
            }
        }
        BoundaryKind::ProcessLifecycle => {
            if let Some(outcome) = event
                .observed
                .get("outcome")
                .and_then(serde_json::Value::as_str)
            {
                write!(output, "  outcome={outcome}").expect("write transcript");
            }
        }
        BoundaryKind::Observer => {
            if let Some(visibility) = event
                .observed
                .get("visibility")
                .and_then(serde_json::Value::as_str)
            {
                write!(output, "  visibility={visibility}").expect("write transcript");
            }
        }
        _ => {}
    }
    output.push('\n');
}

fn boundary_class(kind: BoundaryKind) -> &'static str {
    match kind {
        BoundaryKind::Ingress | BoundaryKind::QueuedIngress => "Ingress",
        BoundaryKind::Provider | BoundaryKind::ProviderEvent => "Provider",
        BoundaryKind::Tool => "Tool",
        BoundaryKind::ExecCode => "ExecCode",
        BoundaryKind::DurableEffect => "DurableEffect",
        BoundaryKind::ProcessWake => "ProcessWake",
        BoundaryKind::ProcessLifecycle => "ProcessLifecycle",
        BoundaryKind::Worker => "Worker",
        BoundaryKind::Observer => "Observer",
        BoundaryKind::Cancellation => "Cancellation",
        BoundaryKind::Trigger => "Trigger",
        BoundaryKind::BackendFailure => "BackendFailure",
        BoundaryKind::ProviderMutation => "ProviderMutation",
        BoundaryKind::LeaseTime => "LeaseTime",
    }
}

fn boundary_label(event: &DeliveredBoundary) -> &str {
    if event.kind == BoundaryKind::Provider {
        "provider.chat.stream"
    } else {
        &event.label
    }
}

fn write_checkpoint(
    output: &mut String,
    sequence: usize,
    write: &CheckpointWriteEvent,
    alias: &str,
    show_session: bool,
) {
    if show_session {
        writeln!(
            output,
            "{alias:<11} {sequence:04}  {:<18} checkpoint.commit  rev={}->{}",
            "Checkpoint", write.revision_before, write.revision_after
        )
        .expect("write transcript");
    } else {
        writeln!(
            output,
            "  {sequence:04}  {:<18} checkpoint.commit  rev={}->{}",
            "Checkpoint", write.revision_before, write.revision_after
        )
        .expect("write transcript");
    }
    for component in &write.components {
        let detail = match component_write_detail(&component.kind) {
            Some(detail) => detail,
            None => "ref (unchanged)".to_string(),
        };
        let component = component.component.as_str();
        if show_session {
            writeln!(
                output,
                "{alias:<11}                       {component:<17} {detail}"
            )
            .expect("write transcript");
        } else {
            writeln!(output, "                         {component:<17} {detail}")
                .expect("write transcript");
        }
    }
}

fn component_write_detail(kind: &CheckpointComponentWriteKind) -> Option<String> {
    match kind {
        CheckpointComponentWriteKind::Stored { logical_bytes } => {
            if let Some(bytes) = logical_bytes {
                Some(format!("stored logical={}", format_bytes(*bytes)))
            } else {
                Some("stored logical=unknown".to_string())
            }
        }
        CheckpointComponentWriteKind::UnchangedRef => None,
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
        components: Vec::new(),
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
        BTreeMap::new(),
        events,
        writes,
        crate::trace::OracleVerdict::passed("test", "passed"),
        Vec::new(),
        crate::trace::AbstractWorldSummary::with_digest(0, 0, Vec::new(), Vec::new(), Vec::new()),
        std::path::Path::new("trace.json"),
    )
}

#[cfg(test)]
fn lines_for_sequence(transcript: &str, sequence: usize) -> Vec<&str> {
    let needle = format!("{sequence:04}");
    transcript
        .lines()
        .filter(|line| line.contains(&needle))
        .collect()
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
        for sequence in 1..=6 {
            let lines = lines_for_sequence(&transcript, sequence);
            assert_eq!(lines.len(), 1, "sequence {sequence}: {transcript}");
            let expected = if matches!(sequence, 1 | 3 | 4) {
                "session-001"
            } else {
                "session-002"
            };
            assert!(
                lines[0].starts_with(expected),
                "sequence {sequence} was not attributed to {expected}: {transcript}"
            );
        }
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
        let checkpoint = transcript.find("Checkpoint").expect("checkpoint line");
        assert!(
            checkpoint > trigger,
            "contract checkpoint rendered before its cause:\n{transcript}"
        );
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use lash_core::store::RuntimeCommit;
    use lash_core::{
        InMemorySessionStoreFactory, PluginSessionSnapshot, ProcessAwaitOutput, ProcessChangeHub,
        ProcessCompletionAuthority, ProcessEventAppendRequest, ProcessEventSemanticsSpec,
        ProcessEventType, ProcessRegistry as _, ProcessValueSelector, ProcessWakeSpec,
        ProjectionWatermark, RecoveryDisposition, RuntimeSessionState, SessionRelation,
        SessionStoreCreateRequest, SessionStoreFactory as _, ToolState,
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
            correct.contains("tool_state        stored"),
            "control transcript must show the changed body was stored: {correct}"
        );
        assert!(
            defect.contains("tool_state        ref (unchanged)"),
            "mutated transcript must expose the missing body: {defect}"
        );
    }

    #[tokio::test]
    async fn process_cutover_transcript_shows_retarget_and_retention_degradation() {
        let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
        let process_id = "transcript-process";
        registry
            .register_process(
                lash_core::ProcessRegistration::new(
                    process_id,
                    lash_core::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    RecoveryDisposition::ExternallyOwned,
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
                delivery.discard_reason == Some(lash_core::WakeDiscardReason::Retargeted)
            })
            .expect("retargeted delivery");

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
        let output = lash_core::ProcessAwaiter::new(
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

        let trace = trace_with_events(
            vec![
                DeliveredBoundary {
                    schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
                    sequence: 1,
                    scheduler: Default::default(),
                    boundary_id: "retarget".to_string(),
                    actor_alias: "source-session".to_string(),
                    kind: BoundaryKind::ProcessWake,
                    at: 1,
                    label: "process.subscription.retargeted".to_string(),
                    payload: serde_json::json!({"turn_index": 1}),
                    observed: serde_json::json!({
                        "turn_index": 1,
                        "discard_reason": retargeted
                            .discard_reason
                            .expect("discard reason")
                            .as_str(),
                    }),
                },
                DeliveredBoundary {
                    schema: crate::scheduler::BOUNDARY_EVENT_SCHEMA.to_string(),
                    sequence: 2,
                    scheduler: Default::default(),
                    boundary_id: "pruned-await".to_string(),
                    actor_alias: "branch-session".to_string(),
                    kind: BoundaryKind::ProcessLifecycle,
                    at: 2,
                    label: "process.await.information".to_string(),
                    payload: serde_json::json!({"turn_index": 1}),
                    observed: serde_json::json!({
                        "turn_index": 1,
                        "outcome": "no_longer_retained",
                    }),
                },
            ],
            Vec::new(),
        );

        insta::assert_snapshot!(trace.render_transcript(), @r"
session-001 0001  ProcessWake        process.subscription.retargeted  discard=retargeted
session-002 0002  ProcessLifecycle   process.await.information  outcome=no_longer_retained
");
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
                policy: Default::default(),
            })
            .await
            .expect("create observed store");
        let mut state = RuntimeSessionState {
            session_id: "mutation-session".to_string(),
            turn_index: 1,
            tool_state_snapshot: Some(tool_state(1)),
            plugin_snapshot: Some(PluginSessionSnapshot::default()),
            plugin_snapshot_revision: Some(1),
            execution_state_snapshot: Some(b"first execution state".to_vec()),
            ..RuntimeSessionState::default()
        };
        let first = store
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&state, &[]))
            .await
            .expect("seed checkpoint component refs");
        state.apply_persisted_commit_result(first);

        state.turn_index = 2;
        state.tool_state_snapshot = Some(tool_state(2));
        state.plugin_snapshot = Some(PluginSessionSnapshot::default());
        state.plugin_snapshot_revision = Some(2);
        state.execution_state_snapshot = Some(b"changed execution state".to_vec());
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
            BTreeMap::new(),
            Vec::new(),
            vec![write],
            OracleVerdict::passed("test", "passed"),
            Vec::new(),
            AbstractWorldSummary::with_digest(0, 0, Vec::new(), Vec::new(), Vec::new()),
            Path::new("trace.json"),
        )
    }
}
