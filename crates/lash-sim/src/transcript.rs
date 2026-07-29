use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::scheduler::{BoundaryKind, DeliveredBoundary};
use crate::store::{DurableComponentWriteKind, DurableWriteEvent};
use crate::trace::{SimulationTrace, StableAliases};

impl SimulationTrace {
    /// Render the completed run as deterministic, review-oriented text.
    pub fn render_transcript(&self) -> String {
        render(self, None)
    }

    /// Render one raw simulator session using a stable `session-NNN` alias.
    pub fn render_session_transcript(&self, session_id: &str) -> String {
        render(self, Some(session_id))
    }
}

fn render(trace: &SimulationTrace, session_filter: Option<&str>) -> String {
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
        .filter(|write| session_filter.is_none_or(|session_id| write.session_id == session_id))
        .collect::<Vec<_>>();

    let mut aliases = StableAliases::default();
    for boundary in &boundaries {
        aliases.alias("session", boundary.actor_alias.clone());
    }
    for write in &writes {
        aliases.alias("session", write.session_id.clone());
    }

    let mut writes_by_turn = BTreeMap::<(&str, usize), Vec<&DurableWriteEvent>>::new();
    for write in writes {
        writes_by_turn
            .entry((&write.session_id, write.turn_index))
            .or_default()
            .push(write);
    }
    let mut rendered_writes = BTreeSet::<(&str, usize)>::new();
    let mut output = String::new();
    let mut sequence = 0usize;
    let mut current_turn = BTreeMap::<&str, usize>::new();

    for boundary in boundaries {
        let alias = aliases.alias("session", boundary.actor_alias.clone());
        let turn_index = boundary_turn_index(boundary);
        if boundary.kind == BoundaryKind::Ingress {
            current_turn.insert(&boundary.actor_alias, 1);
            writeln!(output, "turn 1  {alias}").expect("write transcript");
        } else if boundary.kind == BoundaryKind::Provider
            && current_turn.get(boundary.actor_alias.as_str()) != Some(&turn_index)
        {
            current_turn.insert(&boundary.actor_alias, turn_index);
            writeln!(output, "turn {turn_index}  {alias}").expect("write transcript");
        }

        let is_resume = boundary
            .payload
            .get("suspend_resume")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if is_resume {
            writeln!(output, "resume {alias}").expect("write transcript");
        }

        sequence += 1;
        write_boundary_line(&mut output, sequence, boundary);

        if boundary.kind == BoundaryKind::Ingress
            && boundary.observed.get("runtime_suspend").is_some()
        {
            writeln!(output, "park   {alias}").expect("write transcript");
        }

        if (boundary.kind == BoundaryKind::Provider || is_resume)
            && let Some(turn_writes) =
                writes_by_turn.get(&(boundary.actor_alias.as_str(), turn_index))
        {
            for write in turn_writes {
                sequence += 1;
                write_checkpoint(&mut output, sequence, write);
                rendered_writes.insert((write.session_id.as_str(), write.commit_index));
            }
        }
    }

    for turn_writes in writes_by_turn.values() {
        for write in turn_writes {
            if rendered_writes.contains(&(write.session_id.as_str(), write.commit_index)) {
                continue;
            }
            let alias = aliases.alias("session", write.session_id.clone());
            writeln!(output, "turn {}  {alias}", write.turn_index).expect("write transcript");
            sequence += 1;
            write_checkpoint(&mut output, sequence, write);
        }
    }

    output.trim_end().to_string()
}

fn boundary_turn_index(event: &DeliveredBoundary) -> usize {
    event
        .observed
        .get("turn_index")
        .or_else(|| event.payload.get("turn_index"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1) as usize
}

fn write_boundary_line(output: &mut String, sequence: usize, event: &DeliveredBoundary) {
    let class = boundary_class(event.kind);
    let label = boundary_label(event);
    write!(output, "  {sequence:04}  {class:<18} {label}").expect("write transcript");
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

fn write_checkpoint(output: &mut String, sequence: usize, write: &DurableWriteEvent) {
    writeln!(
        output,
        "  {sequence:04}  {:<18} checkpoint.commit  rev={}->{}",
        "Checkpoint", write.revision_before, write.revision_after
    )
    .expect("write transcript");
    for component in &write.components {
        match component.kind {
            DurableComponentWriteKind::Stored { bytes } => {
                writeln!(
                    output,
                    "                         {:<17} stored {}",
                    component.component,
                    format_bytes(bytes)
                )
                .expect("write transcript");
            }
            DurableComponentWriteKind::UnchangedRef => {
                writeln!(
                    output,
                    "                         {:<17} ref (unchanged)",
                    component.component
                )
                .expect("write transcript");
            }
        }
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
        InMemorySessionStoreFactory, PluginSessionSnapshot, RuntimeSessionState, SessionRelation,
        SessionStoreCreateRequest, SessionStoreFactory as _, ToolState,
    };

    use super::*;
    use crate::store::{DurableWriteCollector, ObservedSessionStoreFactory};
    use crate::trace::{AbstractWorldSummary, OracleVerdict};

    #[tokio::test]
    async fn transcript_discriminates_missing_checkpoint_component_bodies() {
        let correct = changed_component_commit(DurableWriteCollector::default()).await;
        let defect = changed_component_commit(DurableWriteCollector::with_ref_only_mutation(
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

    async fn changed_component_commit(collector: DurableWriteCollector) -> DurableWriteEvent {
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

    fn trace_with_write(write: DurableWriteEvent) -> SimulationTrace {
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
