//! PURPOSE (FIG-5259, ADR 0133 §8): a compaction call and a direct call
//! compose the sections of their own purpose, under their owning execution.
//! A compaction call offers no tools and composes no turn section; a direct
//! call composes exactly the sections of its explicit purpose, and none of
//! the session's turn or compaction prompt.

#![expect(
    clippy::expect_used,
    reason = "test target: the setup helpers around the law are test code too"
)]

use std::sync::{Arc, Mutex};

use lash::plugins::{
    PluginDeclaration, PluginDefinition, PluginError, PluginRegistrar, PromptInput,
    PromptRenderError, PromptSectionSpec, SectionText,
};
use lash::prompt::{PromptPlacement, PromptPurpose, PromptSectionKey};
use lash::tools::{StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall};

const PLUGIN: &str = "purpose-probe";
const MODEL: &str = "purpose-probe-model";
const TOOL: &str = "purpose_probe";
const DIRECT: &str = "purpose-probe-direct";
const TURN_TEXT: &str = "turn section 41d2";
const COMPACTION_TEXT: &str = "compaction section 9b07";
const DIRECT_TEXT: &str = "direct section 6e3a";

#[derive(Clone)]
struct Probe;

impl PluginDefinition for Probe {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for Probe {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

fn section(
    reg: &mut PluginRegistrar,
    key: &str,
    purpose: PromptPurpose,
    text: &'static str,
) -> Result<(), PluginError> {
    reg.prompt().section(
        PromptSectionSpec::new(
            PromptSectionKey::new(key).expect("valid section key"),
            PromptPlacement::InitialInstructions,
        )
        .purposes([purpose]),
        Arc::new(move |_: &PromptInput<'_>| Ok::<_, PromptRenderError>(SectionText::text(text))),
    )
}

impl lash::plugins::SessionPlugin for Probe {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        section(reg, "turn", PromptPurpose::Turn, TURN_TEXT)?;
        section(
            reg,
            "compaction",
            PromptPurpose::Compaction,
            COMPACTION_TEXT,
        )?;
        section(
            reg,
            "direct",
            PromptPurpose::Direct {
                name: DIRECT.to_owned(),
            },
            DIRECT_TEXT,
        )?;
        let definition = lash::tools::ToolDefinition::raw(
            "purpose:probe",
            TOOL,
            "Asks the model a direct question",
            serde_json::json!({"type":"object","additionalProperties":false}),
            serde_json::json!({"type":"string"}),
        )
        .map_err(|error| PluginError::Session(error.to_string()))?
        .with_execution(std::time::Duration::from_secs(120));
        reg.tools()
            .provider(Arc::new(StaticToolProvider::new(vec![definition], Probe)))?;
        Ok(())
    }
}

#[lash::async_trait]
impl StaticToolExecute for Probe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        match call
            .context
            .direct_completions()
            .complete(
                lash::direct::DirectRequest::text("a direct question"),
                DIRECT,
            )
            .await
        {
            Ok(completion) => {
                lash::tools::ToolOutcome::ok(serde_json::Value::String(completion.text)).into()
            }
            Err(error) => lash::tools::ToolOutcome::err_fmt(error).into(),
        }
    }
}

/// One request the model received: its instructions and offered tools.
#[derive(Clone, Debug)]
struct Received {
    instructions: String,
    tools: Vec<String>,
}

fn answer(parts: Vec<lash::direct::LlmOutputPart>) -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        parts,
        ..Default::default()
    }
}

fn text(text: &str) -> lash::provider::LlmResponse {
    answer(vec![lash::direct::LlmOutputPart::Text {
        text: text.to_owned(),
        response_meta: None,
    }])
}

#[tokio::test]
async fn compaction_and_direct_calls_compose_only_their_own_purpose() {
    let seen = Arc::new(Mutex::new(Vec::<Received>::new()));
    let provider = {
        let seen = Arc::clone(&seen);
        lash_core::testing::TestProvider::builder()
            .kind("purpose-probe")
            .complete(move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    let received = Received {
                        instructions: request.instructions.as_deref().unwrap_or("").to_owned(),
                        tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
                    };
                    let turns = {
                        let mut seen = seen.lock().expect("request log");
                        seen.push(received.clone());
                        seen.iter()
                            .filter(|request| request.instructions.contains(TURN_TEXT))
                            .count()
                    };
                    Ok(if received.instructions.contains(DIRECT_TEXT) {
                        text("a direct answer")
                    } else if received.instructions.contains(COMPACTION_TEXT) {
                        text("a summary")
                    } else if turns == 1 {
                        // The turn's first call asks for the probe.
                        answer(vec![lash::direct::LlmOutputPart::ToolCall {
                            call_id: "probe-1".to_owned(),
                            tool_name: TOOL.to_owned(),
                            input_json: "{}".to_owned(),
                            replay: None,
                        }])
                    } else {
                        text("done")
                    })
                }
            })
            .build()
            .into_handle()
    };
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a memory store set");
    let core = lash::LashCore::standard_builder(lash_conformance::backend_over(Arc::new(stores)))
        .llm_profiles(Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    MODEL,
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder(MODEL)
                            .context_window_tokens(200_000)
                            .build()
                            .expect("valid model"),
                        provider,
                    ),
                )
                .expect("register the test model"),
        ))
        .plugin(Arc::new(Probe))
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "purpose-probe-worker",
            "purpose-probe-boot",
        ))
        .expect("core");
    let session = crate::created_session(&core, MODEL, "purpose-probe-session")
        .await
        .open()
        .await
        .expect("the session opens");

    let turn = session
        .send(lash::TurnInput::text("ask the probe"))
        .output()
        .await
        .expect("the turn answers");
    assert!(turn.is_success(), "{turn:?}");
    assert!(
        session
            .admin()
            .state()
            .compact_context(None)
            .await
            .expect("the compaction settles"),
        "the compaction opened a frame"
    );

    let seen = seen.lock().expect("request log").clone();
    let of = |marker: &str| {
        seen.iter()
            .filter(|request| request.instructions.contains(marker))
            .collect::<Vec<_>>()
    };
    let (turns, compactions, directs) = (of(TURN_TEXT), of(COMPACTION_TEXT), of(DIRECT_TEXT));
    assert_eq!(turns.len(), 2, "the turn called the model twice: {seen:?}");
    assert_eq!(compactions.len(), 1, "one summarizer call: {seen:?}");
    assert_eq!(directs.len(), 1, "one direct call: {seen:?}");
    assert_eq!(
        turns.len() + compactions.len() + directs.len(),
        seen.len(),
        "every call composed the sections of exactly one purpose: {seen:?}"
    );
    assert!(
        turns
            .iter()
            .all(|turn| turn.tools.iter().any(|tool| tool == TOOL)),
        "the turn offers the probe: {turns:?}"
    );
    assert!(
        compactions[0].tools.is_empty(),
        "a compaction call offers no tools: {compactions:?}"
    );
    assert_eq!(
        directs[0].instructions, DIRECT_TEXT,
        "a direct call composes only its explicit purpose's sections"
    );
    assert!(
        directs[0].tools.is_empty(),
        "a direct call offers no tools: {directs:?}"
    );
    core.shutdown().await.expect("shutdown");
}
