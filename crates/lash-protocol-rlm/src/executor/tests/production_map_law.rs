//! Law L1 on the production RLM path (FIG-3571).
//!
//! A foreground cell runs through the ordinary executor with a trace sink.
//! Every `(node_id, kind)` the VM emits must be a site of the
//! `ExecutionStarted` map the same execution published, whatever loop,
//! assignment or branch shape emitted it, and an arm the run never took folds
//! to `Skipped` rather than to an unmapped node.

use super::*;

#[derive(Default)]
struct RecordingSink(Mutex<Vec<lash_core::facade_support::TraceRecord>>);

impl TraceSink for RecordingSink {
    fn append(
        &self,
        record: &lash_core::facade_support::TraceRecord,
    ) -> Result<(), lash_core::facade_support::TraceSinkError> {
        if matches!(
            &record.event,
            lash_core::TraceEvent::LanguageExecution { .. }
        ) {
            self.0.lock().expect("trace sink lock").push(record.clone());
        }
        Ok(())
    }
}

/// Every loop kind TypeScript lowers, each with a multi-statement body, plus
/// member assignment in braced and unbraced arms, a switch, a try, an array
/// callback, a labelled statement and a process literal.
const CORPUS: &str = r#"
const items = [1, 2];
for (const item of items) {
  await web.fetch({ url: "array-first" });
  if (item > 0) {
    await web.fetch({ url: "array-then" });
  } else {
    await web.fetch({ url: "array-never" });
  }
}
for (const [key, value] of new Map([["a", 1]])) {
  await web.fetch({ url: "map-first" });
  await web.fetch({ url: "map-second" });
}
for (const member of new Set([1])) {
  await web.fetch({ url: "set-first" });
  await web.fetch({ url: "set-second" });
}
for (const [name, text] of new URLSearchParams("a=1")) {
  await web.fetch({ url: "params-first" });
  await web.fetch({ url: "params-second" });
}
for (const field in { a: 1 }) {
  await web.fetch({ url: "keys-first" });
  await web.fetch({ url: "keys-second" });
}
for (const { url } of [{ url: "destructured" }]) {
  await web.fetch({ url: url });
  await web.fetch({ url: "destructured-second" });
}
const box = { value: 0 };
if (items.length > 0) box.value = (await web.fetch({ url: "unbraced-member" })).length;
if (items.length > 0) {
  box.value = (await web.fetch({ url: "braced-member" })).length;
}
switch (items.length) {
  case 2:
    await web.fetch({ url: "switch" });
    break;
  default:
    await web.fetch({ url: "switch-default" });
}
try {
  await web.fetch({ url: "try" });
} catch (error) {
  await web.fetch({ url: "catch" });
}
const doubled = items.map((item) => item * 2);
/** @label Labelled fetch */
await web.fetch({ url: "labelled" });
const worker = async () => {
  for (const step of [1, 2]) {
    await web.fetch({ url: "worker-first" });
    await web.fetch({ url: "worker-second" });
  }
};
finish(doubled);
"#;

async fn run_cell(source: &str) -> Vec<lash_core::facade_support::TraceRecord> {
    let sink = Arc::new(RecordingSink::default());
    let context =
        lash_core::testing::code_execution_context_with_tool_provider_catalog_and_invocation(
            Arc::new(BindingRecordingDeferredProvider {
                executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                observed_bindings: Arc::new(std::sync::Mutex::new(Vec::new())),
                enumerations: Default::default(),
            }),
            lash_core::ToolCatalog::default(),
            lash_core::testing::exec_code_invocation(
                "fig3571-l1",
                "turn-1",
                1,
                1,
                "exec-l1",
                "exec:l1",
            ),
        );
    let mut state = RlmExecutionState::for_engine("typescript");
    let response = execute_code_with_channel_and_bounds(
        &mut state,
        context,
        ExecRequest {
            language: "typescript".to_string(),
            code: source.to_string(),
        },
        lashlang::global_in_memory_lashlang_artifact_store(),
        LashlangSurface {
            language_features: lashlang::LashlangLanguageFeatures::default()
                .with_label_annotations(),
            ..LashlangSurface::default()
        },
        Some(Arc::new(BindingDeferredResolver {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })),
        RlmProjectedBindings::default(),
        Arc::new(ProjectionRegistry::new()),
        RlmLashlangExecutionTraceConfig {
            sink: Some(sink.clone()),
            trace_context: TraceContext::default(),
        },
        lashlang::ExecutionBounds::unbounded(),
        crate::plugin::RlmChannel::Cell,
    )
    .await;
    assert_eq!(response.error, None, "the L1 corpus cell executes");
    sink.0.lock().expect("trace sink lock").clone()
}

fn language_events(
    records: &[lash_core::facade_support::TraceRecord],
) -> Vec<&TraceLanguageExecution> {
    records
        .iter()
        .filter_map(|record| match &record.event {
            lash_core::TraceEvent::LanguageExecution { event, .. } => Some(event),
            _ => None,
        })
        .collect()
}

#[test]
fn production_rlm_map_contains_every_emitted_site_for_every_loop_kind() {
    block_on(async {
        let records = run_cell(CORPUS).await;
        let events = language_events(&records);
        let maps = events
            .iter()
            .filter_map(|event| match &event.payload {
                TraceLanguageExecutionPayload::ExecutionStarted { execution_map } => {
                    Some(execution_map)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [map] = maps.as_slice() else {
            panic!("one execution_started event, got {}", maps.len());
        };
        let declared = map
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.kind))
            .collect::<BTreeSet<_>>();

        let mut emitted = BTreeSet::new();
        for event in &events {
            let site = match &event.payload {
                TraceLanguageExecutionPayload::NodeStarted {
                    node_id, node_kind, ..
                }
                | TraceLanguageExecutionPayload::NodeCompleted {
                    node_id, node_kind, ..
                }
                | TraceLanguageExecutionPayload::NodeFailed {
                    node_id, node_kind, ..
                }
                | TraceLanguageExecutionPayload::NodeWaiting {
                    node_id, node_kind, ..
                }
                | TraceLanguageExecutionPayload::NodeResumed {
                    node_id, node_kind, ..
                } => Some((node_id.clone(), *node_kind)),
                _ => None,
            };
            if let Some(site) = site {
                emitted.insert(site);
            }
        }
        let unmapped = emitted.difference(&declared).collect::<Vec<_>>();
        assert!(
            unmapped.is_empty(),
            "every emitted (node_id, kind) must be a site of the ExecutionStarted map; unmapped: {unmapped:?}"
        );

        let resource_operations = emitted
            .iter()
            .filter(|(_, kind)| *kind == lash_sansio::ExecutionNodeKind::ResourceOperation)
            .count();
        assert!(
            resource_operations >= 17,
            "the corpus must emit one resource-operation node per authored fetch statement, got {resource_operations}: {emitted:?}"
        );
        let graph = lash_trace::TraceLashlangGraphStore::fold(None, &records)
            .expect("the cell's records fold");
        let skipped = graph
            .nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.observation,
                    lash_trace::TraceLashlangNodeObservation::Skipped { .. }
                )
            })
            .count();
        assert!(
            skipped >= 1,
            "the arm the loop never took folds to Skipped: {:?}",
            graph.nodes
        );
    });
}
