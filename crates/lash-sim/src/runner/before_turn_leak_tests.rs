//! FIG-5254: a before-turn instructional message is prompt material for the
//! turn it prepares, never conversation history. A default
//! `TurnContributions.messages` entry carries no transient origin, so this law
//! pins that the turn's commit still keeps it out of the session graph.

use super::*;
use lash_sansio::sync::MutexExt;

const LEAK_MARKER: &str = "before-turn instruction 7c1f";
const LEAK_SEED: u64 = 0x5eed_5254;

#[derive(Clone)]
struct BeforeTurnInstruction;

impl lash_core::plugin::PluginDefinition for BeforeTurnInstruction {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("before-turn-instruction")
    }
}

impl lash_core::plugin::PluginFactory for BeforeTurnInstruction {
    fn id(&self) -> &'static str {
        "before-turn-instruction"
    }

    fn build(
        &self,
        _: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::SessionPlugin for BeforeTurnInstruction {
    fn id(&self) -> &'static str {
        "before-turn-instruction"
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), lash_core::PluginError> {
        reg.turn().before(
            lash_core::hook_key!("instruct"),
            Arc::new(|_| {
                Box::pin(async {
                    Ok(lash_core::plugin::TurnContributions {
                        messages: vec![lash_core::PluginMessage::text(
                            lash_core::MessageRole::User,
                            LEAK_MARKER,
                        )],
                        ..Default::default()
                    })
                })
            }),
        )?;
        Ok(())
    }
}

#[tokio::test]
async fn before_turn_instruction_never_enters_graph_history() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = {
        let seen = Arc::clone(&seen);
        lash_core::testing::TestProvider::builder()
            .kind("lash-sim-before-turn-leak")
            .complete(move |request| {
                let seen = Arc::clone(&seen);
                async move {
                    let body = serde_json::to_string(&request.messages)
                        .map_err(|err| LlmTransportError::new(err.to_string()))?;
                    seen.lock_recover().push(body);
                    Ok(text_llm_response("acknowledged"))
                }
            })
            .build()
            .into_handle()
    };
    let engine = crate::backend::SimEngine::new(LEAK_SEED)
        .await
        .expect("sim engine");
    let core = lash::LashCore::standard_builder(engine.backend())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .plugin(Arc::new(BeforeTurnInstruction))
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder("mock-before-turn-leak")
                .context_window_tokens(200_000)
                .build()
                .expect("profile metadata"),
        )
        .build(crate::sim_process_owner())
        .expect("core");
    let session = crate::open_created_session("mock-before-turn-leak", &core, "before-turn-leak")
        .await
        .expect("session");
    let output = engine
        .run_turn(
            &session,
            "before-turn-leak-turn",
            Arc::new(super::runtime_proofs::RuntimeProofRecordingEvents::default()),
            Arc::new(
                |session: &lash::LashSession| Ok(session.send(lash::TurnInput::text("hello"))),
            ),
        )
        .await
        .expect("sim turn")
        .expect("turn output");
    assert!(
        output.is_success(),
        "the turn finishes: {:?}",
        output.result.outcome
    );
    assert!(
        seen.lock_recover()
            .first()
            .is_some_and(|body| body.contains(LEAK_MARKER)),
        "the before-turn instruction reaches the turn's model request"
    );

    let committed = session
        .durable()
        .read()
        .await
        .expect("committed read")
        .expect("committed session");
    let leaked = committed
        .messages()
        .iter()
        .filter(|message| {
            message
                .parts
                .iter()
                .any(|part| part.content().contains(LEAK_MARKER))
        })
        .count();
    assert_eq!(
        leaked, 0,
        "a before-turn instruction must not enter graph history"
    );
}
