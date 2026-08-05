use std::collections::BTreeMap;

use super::*;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ProcessCountConservation {
    spawned: usize,
    pruned: usize,
}

impl ProcessCountConservation {
    pub(super) fn from_modeled_totals(spawned: usize, pruned: usize) -> Self {
        Self { spawned, pruned }
    }

    pub(super) fn record_spawn(&mut self) {
        self.spawned += 1;
    }

    pub(super) fn record_pruned(&mut self, count: usize) {
        self.pruned += count;
    }
}

pub(super) async fn assert_process_count_conservation(
    registry: &Arc<dyn ProcessRegistry>,
    conservation: ProcessCountConservation,
) -> Result<(), String> {
    let summaries = registry
        .live_reference_summary()
        .await
        .map_err(|error| error.to_string())?;
    let retained = registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            ..ProcessListFilter::default()
        })
        .await
        .map_err(|error| error.to_string())?;
    let canonical = ProcessLiveReferenceSummary::from_records(&retained);
    if summaries != canonical {
        return Err(format!(
            "process live-reference summary diverged from ProcessLiveReferenceSummary::from_records: actual={summaries:?} canonical={canonical:?}"
        ));
    }

    let spawned_count = conservation.spawned;
    let live_count = summaries
        .iter()
        .map(|summary| summary.process_count)
        .sum::<usize>();
    let terminal_count = retained
        .iter()
        .filter(|record| record.is_terminal())
        .count();
    let pruned_count = conservation.pruned;
    if spawned_count != live_count + terminal_count + pruned_count {
        return Err(format!(
            "process-count conservation violated: spawned={spawned_count} live={live_count} terminal={terminal_count} pruned={pruned_count}"
        ));
    }
    Ok(())
}

pub(super) async fn live_reference_summary_tracks_non_terminal_reference_counts(
    registry: Arc<dyn ProcessRegistry>,
) {
    let mut conservation = ProcessCountConservation::default();
    assert!(
        registry
            .live_reference_summary()
            .await
            .expect("empty summary")
            .is_empty(),
        "a fresh registry has no live references"
    );

    let definition_a = serde_json::json!({ "module": "alpha", "process": "main" });
    let definition_b = serde_json::json!({ "module": "beta", "process": "main" });
    let env_a = ProcessExecutionEnvRef::new("process-env:alpha");
    let env_b = ProcessExecutionEnvRef::new("process-env:beta");
    for (process_id, definition, env_ref) in [
        ("proc-ref-a1", definition_a.clone(), env_a.clone()),
        ("proc-ref-a2", definition_a.clone(), env_a.clone()),
        ("proc-ref-b", definition_b.clone(), env_b.clone()),
    ] {
        registry
            .register_process(
                ProcessRegistration::new(
                    process_id,
                    ProcessInput::Engine {
                        kind: "reference-test".to_string(),
                        payload: serde_json::Value::Null,
                    },
                    RecoveryDisposition::Rerunnable,
                    ProcessProvenance::host(),
                )
                .with_identity(
                    ProcessIdentity::new("reference-test").with_definition(Some(definition)),
                )
                .with_execution_env_ref(Some(env_ref)),
            )
            .await
            .expect("register reference process");
        conservation.record_spawn();
        assert_process_count_conservation(&registry, conservation)
            .await
            .expect("process counts conserve after registration");
    }

    let counts = reference_counts(registry.live_reference_summary().await.expect("summary"));
    assert_eq!(
        counts.get(&(key(&definition_a), env_a.to_string())),
        Some(&2)
    );
    assert_eq!(
        counts.get(&(key(&definition_b), env_b.to_string())),
        Some(&1)
    );

    registry
        .complete_process(
            "proc-ref-a1",
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            crate::ProcessCompletionAuthority::workflow_key("proc-ref-a1"),
        )
        .await
        .expect("complete one alpha process");
    assert_process_count_conservation(&registry, conservation)
        .await
        .expect("process counts conserve after one terminal transition");
    let counts = reference_counts(registry.live_reference_summary().await.expect("summary"));
    assert_eq!(
        counts.get(&(key(&definition_a), env_a.to_string())),
        Some(&1)
    );
    assert_eq!(
        counts.get(&(key(&definition_b), env_b.to_string())),
        Some(&1)
    );

    for process_id in ["proc-ref-a2", "proc-ref-b"] {
        registry
            .complete_process(
                process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                crate::ProcessCompletionAuthority::workflow_key(process_id),
            )
            .await
            .expect("complete remaining process");
        assert_process_count_conservation(&registry, conservation)
            .await
            .expect("process counts conserve after terminal transition");
    }
    assert!(
        registry
            .live_reference_summary()
            .await
            .expect("drained summary")
            .is_empty(),
        "summary is empty when all processes using the references are terminal"
    );
}

fn reference_counts(
    summaries: Vec<ProcessLiveReferenceSummary>,
) -> BTreeMap<(String, String), usize> {
    summaries
        .into_iter()
        .map(|summary| {
            (
                (
                    summary.definition.as_ref().map(key).unwrap_or_default(),
                    summary
                        .env_ref
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                ),
                summary.process_count,
            )
        })
        .collect()
}

fn key(definition: &serde_json::Value) -> String {
    serde_json::to_string(definition).expect("definition serializes")
}
