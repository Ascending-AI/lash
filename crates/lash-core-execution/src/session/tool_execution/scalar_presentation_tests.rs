//! Scalar presentation replay (FIG-3420): the ungrouped `complete_tool_call`
//! path journals `PresentToolResult` under `{call_id}:present`, so a re-driven
//! scalar call serves the recorded `ToolPresentation` and the registered step
//! never re-runs.
//!
//! A standard turn's tool calls are effect-group children since FIG-3397, so
//! this path is reached by scalar calls (a code cell's single awaited call, an
//! undispatched call) rather than by a model turn; the group-child twin is the
//! `tool_child_invocation_tests!` presentation catalogue.

use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A journal keyed by effect id: the first execution records each outcome,
/// and a re-driven call is served from the record without running anything.
#[derive(Default)]
struct JournalByEffectId {
    outcomes: std::sync::Mutex<HashMap<String, crate::RuntimeEffectOutcome>>,
}

impl crate::AwaitEventResolver for JournalByEffectId {}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for JournalByEffectId {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let effect_id = envelope.invocation.effect_id().to_string();
        if let Some(recorded) = self.outcomes.lock_recover().get(&effect_id) {
            return Ok(recorded.clone());
        }
        let outcome = local_executor.execute(envelope).await?;
        self.outcomes
            .lock_recover()
            .insert(effect_id, outcome.clone());
        Ok(outcome)
    }

    // A scalar call opens no group.
    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("JournalByEffectId"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("JournalByEffectId"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("JournalByEffectId"))
    }
}

struct EchoTool {
    definition: crate::ToolDefinition,
}

#[async_trait::async_trait]
impl crate::ToolProvider for EchoTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![self.definition.manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::ok(call.args.clone()).into()
    }
}

struct RecordingStepFactory {
    runs: Arc<AtomicUsize>,
}

impl crate::plugin::PluginFactory for RecordingStepFactory {
    fn id(&self) -> &'static str {
        "scalar-presentation-step"
    }

    fn build(
        &self,
        _ctx: &crate::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(RecordingStep {
            runs: Arc::clone(&self.runs),
        }))
    }
}

struct RecordingStep {
    runs: Arc<AtomicUsize>,
}

impl crate::plugin::SessionPlugin for RecordingStep {
    fn id(&self) -> &'static str {
        "scalar-presentation-step"
    }

    fn register(&self, reg: &mut crate::plugin::PluginRegistrar) -> Result<(), crate::PluginError> {
        let runs = Arc::clone(&self.runs);
        reg.tool_results().presentation_step(Arc::new(move |input| {
            runs.fetch_add(1, Ordering::SeqCst);
            let mut next = input.previous;
            next.parts
                .push(crate::ModelToolReturnPart::text("[recorded]"));
            Box::pin(async move { Ok(next) })
        }));
        Ok(())
    }
}

#[tokio::test]
async fn a_scalar_presentation_replays_from_the_journal_on_redrive() {
    let definition = crate::ToolDefinition::raw(
        "tool:scalar-echo",
        "scalar_echo",
        "scalar echo",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    );
    let journal = Arc::new(JournalByEffectId::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let execute = || {
        let context = crate::testing::TestExecutionContextBuilder::new()
            .plugin_factories(vec![Arc::new(RecordingStepFactory {
                runs: Arc::clone(&runs),
            })])
            .provider(Arc::new(EchoTool {
                definition: definition.clone(),
            }))
            .tool_catalog(crate::ToolCatalog::from_tool_definitions(vec![
                definition.clone(),
            ]))
            .borrowed_effect_controller(
                crate::ScopedEffectController::shared(
                    Arc::clone(&journal) as Arc<dyn crate::RuntimeEffectController>,
                    crate::AdmittedScope::turn("root", "scalar-presentation-redrive"),
                )
                .expect("valid turn scope"),
            )
            .build()
            .into_runtime();
        let tool_id = definition.manifest.id.clone();
        async move {
            Box::pin(context.execute_tool_call_by_id(
                "present-1".to_string(),
                tool_id,
                serde_json::json!({"value": "sample"}),
                0,
                None,
                None,
                None,
            ))
            .await
        }
    };
    let presented = |executed: &super::CompletedProtocolToolCall| {
        executed
            .completed
            .model_return
            .parts
            .iter()
            .any(|part| {
                matches!(part, crate::ModelToolReturnPart::Text { text } if text.contains("[recorded]"))
            })
    };

    let first = execute().await;
    assert!(presented(&first), "the first run presents through the step");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the step ran once");

    // The redrive: a fresh execution context over the same journal serves
    // every journaled effect, `PresentToolResult` included, from its record.
    let replayed = execute().await;
    assert!(
        presented(&replayed),
        "the recorded presentation is served on replay"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "replay served the recorded presentation; the step did not re-run"
    );
}
