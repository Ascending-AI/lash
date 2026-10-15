//! The provider-request trace of a history-bearing turn is the full
//! assembled request. The instruction text is a host prompt section and the
//! Standard protocol's own sections are excluded by the session's plan
//! (prompt sections replaced the Standard prompt options, FIG-5257).

use std::sync::Arc;

use lash::plugins::PluginDeclaration;
use lash::prompt::{PromptPlacement, PromptPlan, PromptSectionId, PromptSectionKey};
use lash_core::facade_support::TraceDetailPolicy;
use serde_json::json;

use crate::provider::ScriptedLlmHttpTransport;
use crate::runtime_providers::{
    OPENAI_COMPATIBLE, runtime_provider_components, runtime_script_for_text,
};

const SNAPSHOT_PLUGIN: &str = "request-snapshot-instruction";

/// Registers the session's one instruction section.
#[derive(Clone)]
struct SnapshotInstruction;

impl SnapshotInstruction {
    pub fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(SNAPSHOT_PLUGIN)
    }
}
impl lash_core::plugin::PluginMetadata for SnapshotInstruction {
    fn plugin_declaration(&self) -> PluginDeclaration {
        Self::declaration()
    }
}

impl lash::plugins::PluginFactory for SnapshotInstruction {
    fn id(&self) -> &'static str {
        SNAPSHOT_PLUGIN
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash_core::plugin::PluginRecipe, lash_core::plugin::PluginError> {
        let mut reg = lash_core::plugin::PluginRegistrar::<SnapshotInstruction>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        SnapshotInstruction::declare(&mut reg)?;
        let id = self.id();
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
        ))
    }
}

impl lash::plugins::SessionPlugin for SnapshotInstruction {
    fn id(&self) -> &'static str {
        SNAPSHOT_PLUGIN
    }
}
impl SnapshotInstruction {
    fn declare(
        reg: &mut lash_core::plugin::PluginRegistrar<Self>,
    ) -> std::result::Result<(), lash_core::plugin::PluginError> {
        reg.prompt().section(
            lash_core::plugin::prompt::PromptSectionSpec::new(
                lash_core::prompt_sections::PromptSectionKey::new("intro")
                    .expect("valid section key"),
                lash_core::prompt_sections::PromptPlacement::InitialInstructions,
            ),
            move |_this, _state| {
                Ok(Arc::new(
                    |_: &lash_core::plugin::prompt::PromptInput<'_>| {
                        Ok::<_, lash_core::plugin::prompt::PromptRenderError>(
                            lash_core::plugin::prompt::SectionText::text(
                                "System snapshot instruction.",
                            ),
                        )
                    },
                ))
            },
        )?;
        Ok(())
    }
}

/// The plan that leaves the snapshot instruction as the only section.
fn snapshot_plan() -> PromptPlan {
    let standard = |key| {
        PromptSectionId::new(
            lash::standard::STANDARD_PROTOCOL_PLUGIN_ID,
            PromptSectionKey::new(key).expect("valid section key"),
        )
    };
    PromptPlan {
        placements: vec![lash::prompt::PromptSectionPlacement {
            section: standard(lash::standard::standard_section_keys::EXECUTION),
            placement: PromptPlacement::Excluded,
        }],
        ..PromptPlan::default()
    }
}

#[tokio::test]
async fn second_history_bearing_turn_snapshots_the_full_assembled_provider_request() {
    let scripts = ["first reply", "second reply"]
        .into_iter()
        .map(|text| runtime_script_for_text(OPENAI_COMPATIBLE, text))
        .collect::<Result<Vec<_>, _>>()
        .expect("runtime scripts");
    let transport = Arc::new(
        ScriptedLlmHttpTransport::from_scripts(scripts).expect("valid runtime provider scripts"),
    );
    let (provider, model, _) =
        runtime_provider_components(OPENAI_COMPATIBLE, &transport).expect("runtime provider");
    let trace = Arc::new(lash::testing::RecordingTraceSink::new());
    let engine = crate::backend::SimEngine::new(0x5eed_7005)
        .await
        .expect("sim engine");
    let backend = engine.backend();
    let core = lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(provider, model.clone())
        .trace_sink(trace.clone())
        .trace_detail(TraceDetailPolicy::extended_models())
        .telemetry_content(lash::tracing::TelemetryContent::Captured)
        .plugin(Arc::new(SnapshotInstruction))
        .build(crate::sim_process_owner())
        .expect("runtime core");
    let session = crate::open_created_session_from(
        lash::SessionSpec::new(
            model.wire_model.clone(),
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )
        .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        &core,
        "history-request-snapshot",
    )
    .await
    .expect("session");
    let config = session.admin().config();
    let applied = config
        .apply(
            lash::config::ConfigWrite::new(
                "snapshot-plan",
                config.revision().await.expect("config revision"),
            ),
            lash::config::ConfigTransaction::of(lash::config::SetPromptPlan {
                plan: snapshot_plan(),
            }),
        )
        .await
        .expect("accept the snapshot plan")
        .await_outcome(&config)
        .await
        .expect("apply the snapshot plan");
    assert!(
        matches!(
            applied,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{applied:?}"
    );

    let first = engine
        .run_text_turn(&session, "history-snapshot-turn-1", "first question")
        .await
        .expect("first turn handler")
        .expect("first turn");
    assert_eq!(first.assistant_message(), Some("first reply"));
    let second = engine
        .run_text_turn(&session, "history-snapshot-turn-2", "follow-up question")
        .await
        .expect("second turn handler")
        .expect("second turn");
    assert_eq!(second.assistant_message(), Some("second reply"));
    core.flush_trace_sink()
        .expect("flush provider request trace");

    let entries = trace.json_records().expect("trace entries serialize");
    let requests = entries
        .iter()
        .filter(|entry| {
            entry["type"] == "provider_event"
                && entry["event"]["direction"]["direction"] == "request"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requests.len(),
        2,
        "provider request trace entries: {entries:?}"
    );
    let assembled = requests[1]["event"]["raw_json"].clone();
    assert_eq!(
        assembled,
        json!({
            "messages": [
                {
                    "content": [{
                        "text": "System snapshot instruction.",
                        "type": "text"
                    }],
                    "role": "system"
                },
                {
                    "content": [{ "text": "first question", "type": "text" }],
                    "role": "user"
                },
                {
                    "content": [{ "text": "first reply", "type": "text" }],
                    "role": "assistant"
                },
                {
                    "content": [{ "text": "follow-up question", "type": "text" }],
                    "role": "user"
                }
            ],
            "model": "openai/gpt-5.4",
            "stream": true,
            "stream_options": { "include_usage": true },
            "tool_choice": "auto",
            "tools": [{
                "function": {
                    "description": "Run 1-64 independent tool calls concurrently. Every call starts before any finishes; results return in input order, each with a success flag and result or error. Do not nest batch calls.",
                    "name": "batch",
                    "parameters": {
                        "additionalProperties": false,
                        "properties": {
                            "tool_calls": {
                                "description": "1-64 objects { tool, parameters }; each tool must be available and parameters must match its schema.",
                                "items": {
                                    "additionalProperties": false,
                                    "properties": {
                                        "parameters": {
                                            "additionalProperties": true,
                                            "properties": {},
                                            "type": "object"
                                        },
                                        "tool": { "type": "string" }
                                    },
                                    "required": ["tool", "parameters"],
                                    "type": "object"
                                },
                                "maxItems": 64,
                                "minItems": 1,
                                "type": "array"
                            }
                        },
                        "required": ["tool_calls"],
                        "type": "object"
                    },
                    "strict": false
                },
                "type": "function"
            }]
        })
    );
    assert_eq!(requests[1]["event"]["raw_len"], 1162);
    assert_eq!(
        requests[1]["event"]["raw_sha256"],
        "563f5dbe2fe6ddd4cdceb55052daf6e259aa8d502098f05d4249a43b589fcc1c"
    );
}
