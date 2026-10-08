//! NO-LEAK (FIG-5258, ADR 0133): a plugin's model-facing text is a prompt
//! section, and section text never enters conversation history. The law
//! began as FIG-5254's before-turn leak law, where a default
//! `TurnContributions.messages` entry was committed to graph history. That
//! field is deleted; its replacement, a registered section, stays out of the
//! graph, out of the next turn's history and out of a compaction's seed.
//! Before-turn, checkpoint, after-tool and after-turn observers all publish
//! their model-facing text as namespace state rendered by the same section.
//! End to end (FIG-5260): every channel's text reaches the model request
//! outside its history, and the call's recorded snapshot holds exactly the
//! text the model received.

#![expect(
    clippy::expect_used,
    reason = "test target: the setup helpers around the law are test code too"
)]

use std::sync::{Arc, Mutex};

use lash::plugins::{
    PluginDeclaration, PluginDefinition, PromptInput, PromptRenderError, PromptSectionSpec,
    SectionText,
};
use lash::prompt::{PromptPlacement, PromptSectionKey};

const PLUGIN: &str = "section-leak";
const MODEL: &str = "section-leak-model";
const LEAK_MARKER: &str = "standing instruction 7c1f";
const CHANNELS: [&str; 4] = ["before-turn", "checkpoint", "after-tool", "after-turn"];

fn commands(channel: &str) -> lash::plugins::StateCommands {
    lash::plugins::StateCommands::new().set(
        channel,
        serde_json::json!(format!("{LEAK_MARKER} {channel}")),
    )
}

struct Ping;

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Ping {
    async fn execute(&self, _: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(serde_json::json!("pong")).into()
    }
}

#[derive(Clone)]
struct Instruction;

impl PluginDefinition for Instruction {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for Instruction {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for Instruction {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        reg.turn().before(
            lash::hook_key!("before"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash::plugins::TurnContributions {
                        state: commands("before-turn"),
                        ..Default::default()
                    })
                })
            }),
        )?;
        reg.turn().checkpoint(
            lash::hook_key!("checkpoint"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash::plugins::TurnContributions {
                        state: commands("checkpoint"),
                        ..Default::default()
                    })
                })
            }),
        )?;
        reg.tool_calls().check_result(
            lash::hook_key!("after-tool"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash::plugins::AfterToolContributions {
                        state: commands("after-tool"),
                        ..Default::default()
                    })
                })
            }),
        )?;
        reg.turn().after(
            lash::hook_key!("after"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash::plugins::AfterTurnContributions {
                        state: commands("after-turn"),
                        ..Default::default()
                    })
                })
            }),
        )?;
        reg.prompt().section(
            PromptSectionSpec::new(
                PromptSectionKey::new("instruct").expect("valid section key"),
                PromptPlacement::InitialInstructions,
            ),
            Arc::new(|input: &PromptInput<'_>| {
                let text = CHANNELS
                    .iter()
                    .map(|channel| input.state().get_as::<String>(channel))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok::<_, PromptRenderError>(SectionText::text(text))
            }),
        )
    }
}

fn carries_marker(message: &lash::messages::Message) -> bool {
    message
        .parts
        .iter()
        .any(|part| part.content().contains(LEAK_MARKER))
}

#[tokio::test]
async fn section_text_never_enters_history_or_a_compaction_seed() {
    let seen = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
    let provider = {
        let seen = Arc::clone(&seen);
        lash_core::testing::TestProvider::builder()
            .kind("section-leak")
            .requires_streaming(true)
            .complete(move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    let body = serde_json::to_string(&request.messages)
                        .expect("the request's messages encode");
                    let mut requests = seen.lock().expect("request log");
                    let first = requests.is_empty();
                    requests.push((
                        request
                            .instructions
                            .as_deref()
                            .unwrap_or_default()
                            .to_string(),
                        body,
                    ));
                    drop(requests);
                    if first {
                        return Ok(lash_core::llm::types::LlmResponse {
                            parts: vec![lash_core::llm::types::LlmOutputPart::ToolCall {
                                call_id: "section-leak-ping".to_string(),
                                tool_name: "ping".to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..Default::default()
                        });
                    }
                    Ok(lash_core::llm::types::LlmResponse {
                        parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                            text: "acknowledged".to_string(),
                            response_meta: None,
                        }],
                        ..Default::default()
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
        .tools(Arc::new(lash::tools::StaticToolProvider::new(
            vec![
                lash::tools::ToolDefinition::raw(
                    "ping",
                    "ping",
                    "Ping",
                    serde_json::json!({"type":"object"}),
                    serde_json::json!({"type":"string"}),
                )
                .expect("tool schema")
                .with_execution(std::time::Duration::from_secs(120)),
            ],
            Ping,
        )))
        .plugin(Arc::new(Instruction))
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "section-leak-worker",
            "section-leak-boot",
        ))
        .expect("core");
    let session = crate::created_session(&core, MODEL, "section-leak-session")
        .await
        .open()
        .await
        .expect("the session opens");

    let runs = ["hello", "again"].map(|text| {
        (
            text,
            lash::TurnId::parse(format!("section-leak-{text}")).expect("run id"),
        )
    });
    for (text, run) in &runs {
        let turn = session
            .send(lash::TurnInput::text(*text))
            .id(run.clone())
            .output()
            .await
            .expect("the turn answers");
        assert!(turn.is_success(), "{turn:?}");
    }
    assert!(
        session
            .admin()
            .prompt()
            .catalog()
            .await
            .expect("catalog")
            .sections()
            .iter()
            .any(|info| info.section.owner == PLUGIN),
        "the section is registered for the session"
    );
    let committed = session
        .durable()
        .read()
        .await
        .expect("committed read")
        .expect("committed session");
    assert_eq!(
        committed
            .messages()
            .iter()
            .filter(|message| carries_marker(message))
            .count(),
        0,
        "section text must not enter graph history"
    );

    assert!(
        session
            .admin()
            .state()
            .compact_context(
                None,
                "host:prompt_section_leak:compact_context:292".to_string()
            )
            .await
            .expect("the compaction settles")
            .settle_with(
                &session.admin().commands(),
                lash::testing::admin_fixture_outcome
            )
            .await
            .expect("fixture mutation settled"),
        "the compaction opened a frame"
    );
    let seeded = session
        .durable()
        .read()
        .await
        .expect("committed read")
        .expect("committed session");
    assert_eq!(
        seeded
            .messages()
            .iter()
            .filter(|message| carries_marker(message))
            .count(),
        0,
        "section text must not enter a compaction's seed"
    );
    let requests = seen.lock().expect("request log").clone();
    assert!(
        requests.len() >= 4,
        "two turns, an after-tool call and a summarizer each sent a request: {}",
        requests.len()
    );
    assert!(
        requests.iter().all(|(_, body)| !body.contains(LEAK_MARKER)),
        "no request carries section text as history, the next turn's and the summarizer's \
         included"
    );
    for channel in CHANNELS {
        let marker = format!("{LEAK_MARKER} {channel}");
        assert!(
            requests
                .iter()
                .any(|(instructions, _)| instructions.contains(&marker)),
            "{channel}'s published section text reached a model call, outside history"
        );
    }
    // The second turn's first call renders every channel's text: its
    // recorded snapshot holds it, and it is the text the model received.
    let loaded = session
        .admin()
        .prompt()
        .snapshot(&runs[1].1, 1)
        .await
        .expect("the recorded snapshot reads through the facade")
        .expect("the call retains a snapshot");
    let section = loaded
        .snapshot
        .sections
        .iter()
        .find(|section| section.section.owner == PLUGIN)
        .expect("the section was recorded");
    let recorded = loaded
        .text(&section.value)
        .expect("the section recorded text");
    for channel in CHANNELS {
        assert!(
            recorded.contains(&format!("{LEAK_MARKER} {channel}")),
            "the recorded snapshot holds {channel}'s text: {recorded:?}"
        );
    }
    assert!(
        requests
            .iter()
            .any(|(instructions, _)| instructions.contains(recorded)),
        "the recorded section text is the text a model call received"
    );
    core.shutdown().await.expect("shutdown");
}
