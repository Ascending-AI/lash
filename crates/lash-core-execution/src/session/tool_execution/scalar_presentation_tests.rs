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

impl crate::AwaitEventResolver for JournalByEffectId {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

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
        _cancel: crate::runtime::TurnCancelWait,
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
        let context = crate::testing::TestExecutionContextBuilder::over_controller(
            crate::ScopedEffectController::shared(
                Arc::clone(&journal) as Arc<dyn crate::RuntimeEffectController>,
                crate::AdmittedScope::turn("root", "scalar-presentation-redrive"),
            )
            .expect("valid turn scope"),
        )
        .plugin_factories(vec![Arc::new(RecordingStepFactory {
            runs: Arc::clone(&runs),
        })])
        .provider(Arc::new(EchoTool {
            definition: definition.clone(),
        }))
        .tool_catalog(crate::ToolCatalog::from_tool_definitions(vec![
            definition.clone(),
        ]))
        .build()
        .into_runtime();
        let tool_id = definition.manifest.id.clone();
        async move {
            Box::pin(context.execute_command_tool(
                &crate::CommandReplayKey::new("present-1"),
                crate::session::ToolInvocation::new(
                    "present-1",
                    tool_id,
                    serde_json::json!({"value": "sample"}),
                ),
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

/// Records every envelope's stable hash and runs it locally.
///
/// The stable hash is the replay identity every durable substrate keys a
/// recorded effect by; on Restate it is also the `x-lash-replay-key` header
/// of the scope index's `begin_effect`/`end_effect` calls around the effect
/// (`ScopeRecordingController::execute_effect`).
#[derive(Default)]
struct StableHashRecorder {
    hashes: std::sync::Mutex<Vec<(String, String)>>,
}

impl crate::AwaitEventResolver for StableHashRecorder {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for StableHashRecorder {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        self.hashes.lock_recover().push((
            envelope.invocation.effect_id().to_string(),
            envelope.stable_hash()?,
        ));
        local_executor.execute(envelope).await
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("StableHashRecorder"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("StableHashRecorder"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("StableHashRecorder"))
    }
}

/// A fast and a slow live run of the same call present under one replay
/// identity: the presentation's stable hash — and so Restate's
/// `x-lash-replay-key` on the calls around it — never carries how long the
/// call took (FIG-3672 P3, FIG-3666).
#[tokio::test]
async fn a_fast_and_a_slow_run_present_under_one_replay_identity() {
    let recorder = Arc::new(StableHashRecorder::default());
    for duration_ms in [2, 46_000] {
        let context = crate::testing::TestExecutionContextBuilder::over_controller(
            crate::ScopedEffectController::shared(
                Arc::clone(&recorder) as Arc<dyn crate::RuntimeEffectController>,
                crate::AdmittedScope::turn("root", "presentation-identity"),
            )
            .expect("valid turn scope"),
        )
        .plugin_factories(Vec::new())
        .build()
        .into_runtime();
        context
            .complete_tool_call(
                "timed-call".to_string(),
                None,
                crate::tool_dispatch::ToolDispatchOutcome {
                    record: crate::ToolCallRecord {
                        call_id: Some("timed-call".to_string()),
                        tool: "timed".to_string(),
                        args: serde_json::json!({}),
                        output: crate::ToolCallOutput::success(serde_json::json!("timed")),
                    },
                    attempts: Vec::new(),
                    intents: crate::ToolIntents::default(),
                    intent_outcomes: Vec::new(),
                    captures: Vec::new(),
                    triggers: Vec::new(),
                },
                "timed-call",
                duration_ms,
            )
            .await
            .expect("the call presents");
    }
    let hashes = recorder.hashes.lock_recover().clone();
    assert_eq!(hashes.len(), 2, "one presentation per run: {hashes:?}");
    assert!(
        hashes
            .iter()
            .all(|(effect_id, _)| effect_id.ends_with("timed-call:present")),
        "both effects are the call's presentation: {hashes:?}"
    );
    assert_eq!(
        hashes[0].1, hashes[1].1,
        "the fast and the slow run present under one replay identity"
    );
}

/// Runs every effect locally except the presentation, which it refuses the
/// way a durable journal refuses a presentation whose redriven envelope no
/// longer hashes as recorded.
#[derive(Default)]
struct DivergedPresentation;

impl crate::AwaitEventResolver for DivergedPresentation {
    /// A test double that mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for DivergedPresentation {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        if matches!(
            envelope.command,
            crate::RuntimeEffectCommand::PresentToolResult { .. }
        ) {
            return Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::SqliteEffectReplayHashConflict,
                "presentation-conflict: recorded runtime effect hash did not match",
            ));
        }
        local_executor.execute(envelope).await
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DivergedPresentation"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::runtime::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DivergedPresentation"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DivergedPresentation"))
    }
}

/// A language runtime's call (a code cell's `call_tool`) whose presentation
/// diverged is refused: the divergence is the run's nested replay mismatch,
/// which stops the cell at this command and parks the turn, and nothing of
/// the conflict is presented as the call's result (FIG-3679).
#[tokio::test]
async fn a_cell_call_whose_presentation_diverged_stops_the_run() {
    let definition = crate::ToolDefinition::raw(
        "tool:scalar-echo",
        "scalar_echo",
        "scalar echo",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    );
    let context = crate::testing::TestExecutionContextBuilder::over_controller(
        crate::ScopedEffectController::shared(
            Arc::new(DivergedPresentation) as Arc<dyn crate::RuntimeEffectController>,
            crate::AdmittedScope::turn("root", "diverged-presentation"),
        )
        .expect("valid turn scope"),
    )
    .plugin_factories(Vec::new())
    .provider(Arc::new(EchoTool {
        definition: definition.clone(),
    }))
    .tool_catalog(crate::ToolCatalog::from_tool_definitions(vec![
        definition.clone(),
    ]))
    .build()
    .into_runtime();
    let executed = Box::pin(context.execute_command_tool(
        &crate::CommandReplayKey::new("diverged-1"),
        crate::session::ToolInvocation::new(
            "diverged-1",
            definition.manifest.id.clone(),
            serde_json::json!({"value": "sample"}),
        ),
    ))
    .await;
    let mismatch = context
        .nested_replay_mismatch()
        .expect("the diverged presentation is the run's nested replay mismatch");
    assert_eq!(
        mismatch.code,
        crate::RuntimeErrorCode::SqliteEffectReplayHashConflict
    );
    assert!(
        !executed.completed.output.is_success(),
        "the refused call settles no success"
    );
}
