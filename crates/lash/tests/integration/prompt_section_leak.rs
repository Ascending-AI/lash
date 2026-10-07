//! NO-LEAK (FIG-5258, ADR 0133): a plugin's model-facing text is a prompt
//! section, and section text never enters conversation history. The law
//! began as FIG-5254's before-turn leak law, where a default
//! `TurnContributions.messages` entry was committed to graph history. That
//! field is deleted; its replacement, a registered section, stays out of the
//! graph, out of the next turn's history and out of a compaction's seed.

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
        reg.prompt().section(
            PromptSectionSpec::new(
                PromptSectionKey::new("instruct").expect("valid section key"),
                PromptPlacement::InitialInstructions,
            ),
            Arc::new(|_: &PromptInput<'_>| {
                Ok::<_, PromptRenderError>(SectionText::text(LEAK_MARKER))
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
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = {
        let seen = Arc::clone(&seen);
        lash_core::testing::TestProvider::builder()
            .kind("section-leak")
            .complete(move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    let body = serde_json::to_string(&request.messages)
                        .expect("the request's messages encode");
                    seen.lock().expect("request log").push(body);
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
        .plugin(Arc::new(Instruction))
        .plugin(Arc::new(
            lash_plugin_standard_compaction::StandardCompactionPluginFactory::default(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
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

    for text in ["hello", "again"] {
        let turn = session
            .send(lash::TurnInput::text(text))
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
            .compact_context(None)
            .await
            .expect("the compaction settles"),
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
        requests.len() >= 3,
        "two turns and a summarizer each sent a request: {}",
        requests.len()
    );
    assert!(
        requests.iter().all(|body| !body.contains(LEAK_MARKER)),
        "no request carries section text as history, the next turn's and the summarizer's \
         included"
    );
    core.shutdown().await.expect("shutdown");
}
