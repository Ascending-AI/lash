//! FIG-5254: a host reaches the whole prompt-section contract through the
//! facade alone. It registers a section and a wrapper over it, sets the
//! prompt plan through session config, and decodes a snapshot record.

#![expect(
    clippy::expect_used,
    reason = "test target: the setup helpers around the law are test code too"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash::plugins::{
    PluginDeclaration, PluginDefinition, PromptInput, PromptRenderError, PromptSectionSpec,
    PromptWrapSpec, PromptWrapTarget, SectionText,
};
use lash::prompt::{
    PromptPlacement, PromptPlan, PromptPlanError, PromptSectionId, PromptSectionKey,
    PromptSectionPlacement, PromptSnapshot, PromptWrapKey, RecordedSectionText,
};

const PLUGIN: &str = "prompt-witness";
const MODEL: &str = "prompt-witness-model";

fn notes() -> PromptSectionId {
    PromptSectionId::new(
        PLUGIN,
        PromptSectionKey::new("notes").expect("valid section key"),
    )
}

#[derive(Clone)]
struct Witness {
    registered: Arc<AtomicBool>,
}

impl PluginDefinition for Witness {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for Witness {
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

impl lash::plugins::SessionPlugin for Witness {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        reg.prompt().section(
            PromptSectionSpec::new(
                PromptSectionKey::new("notes").expect("valid section key"),
                PromptPlacement::CurrentContext,
            ),
            Arc::new(|input: &PromptInput<'_>| {
                let count = input.state().get_as::<u64>("count")?.unwrap_or(0);
                Ok::<_, PromptRenderError>(SectionText::Text(format!("{count} notes")))
            }),
        )?;
        reg.prompt().wrap(
            PromptWrapSpec::new(
                PromptWrapKey::new("frame").expect("valid wrap key"),
                notes(),
            ),
            Arc::new(
                |_: &PromptInput<'_>, _: PromptWrapTarget<'_>, previous: SectionText| {
                    Ok(match previous {
                        SectionText::Text(text) => {
                            SectionText::Text(format!("<notes>{text}</notes>"))
                        }
                        SectionText::Omit => SectionText::Omit,
                    })
                },
            ),
        )?;
        self.registered.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn a_host_registers_sections_sets_the_plan_and_decodes_a_snapshot_through_the_facade() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a memory store set");
    let registered = Arc::new(AtomicBool::new(false));
    let core = lash::LashCore::standard_builder(lash_conformance::backend_over(Arc::new(stores)))
        .llm_profiles(Arc::new(
            lash::LlmProfileRegistry::new()
                .register(
                    MODEL,
                    lash::RegisteredLlmProfile::new(
                        lash::LlmProfileMetadata::builder(MODEL)
                            .context_window_tokens(4_096)
                            .build()
                            .expect("valid model"),
                        lash_core::testing::TestProvider::builder()
                            .kind("prompt-witness")
                            .complete(|_| async {
                                Ok(lash_core::llm::types::LlmResponse {
                                    parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                                        text: "done".to_string(),
                                        response_meta: None,
                                    }],
                                    ..Default::default()
                                })
                            })
                            .build()
                            .into_handle(),
                    ),
                )
                .expect("register the test model"),
        ))
        .plugin(Arc::new(Witness {
            registered: Arc::clone(&registered),
        }))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "prompt-witness-worker",
            "prompt-witness-boot",
        ))
        .expect("core");
    let session = crate::created_session(&core, MODEL, "prompt-witness-session")
        .await
        .open()
        .await
        .expect("the session opens");

    let config = session.admin().config();
    let placed = |placement| PromptSectionPlacement {
        section: notes(),
        placement,
    };
    let plan = PromptPlan {
        order: vec![notes()],
        placements: vec![placed(PromptPlacement::InitialInstructions)],
        ..PromptPlan::default()
    };
    let revision = config.revision().await.expect("revision");
    let applied = config
        .apply(
            lash::config::ConfigWrite::new("prompt-plan", revision),
            lash::config::ConfigTransaction::of(lash::config::SetPromptPlan { plan }),
        )
        .await
        .expect("the plan settles");
    assert!(
        matches!(
            applied,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{applied:?}"
    );
    let revision = config.revision().await.expect("revision");
    let refused = config
        .apply(
            lash::config::ConfigWrite::new("prompt-plan-twice", revision),
            lash::config::ConfigTransaction::of(lash::config::SetPromptPlan {
                plan: PromptPlan {
                    placements: vec![
                        placed(PromptPlacement::InitialInstructions),
                        placed(PromptPlacement::CurrentContext),
                    ],
                    ..PromptPlan::default()
                },
            }),
        )
        .await
        .expect("the refusal settles");
    let lash::config::ConfigTransactionOutcome::Refused { refusal } = refused else {
        panic!("a plan placing a section twice is refused: {refused:?}");
    };
    assert_eq!(
        refusal.owner_refusal::<lash::config::CoreConfigRefusal>(),
        Some(lash::config::CoreConfigRefusal::PromptPlanRefused {
            error: PromptPlanError::DuplicatePlacement { section: notes() },
        })
    );

    let turn = session
        .send(lash::TurnInput::text("build the plugin session"))
        .output()
        .await
        .expect("the turn answers");
    assert!(turn.is_success(), "{turn:?}");
    assert!(
        registered.load(Ordering::SeqCst),
        "the plugin's section and wrapper registered"
    );

    let snapshot: PromptSnapshot = serde_json::from_value(serde_json::json!({
        "version": 1,
        "plan": {
            "purpose": { "kind": "turn" },
            "sections": [{
                "section": { "owner": PLUGIN, "key": "notes" },
                "owner": { "plugin": PLUGIN, "behavior_revision": 1 },
                "placement": "initial_instructions",
                "placement_source": "host",
                "wraps": [{
                    "wrap": { "owner": PLUGIN, "key": "frame" },
                    "owner": { "plugin": PLUGIN, "behavior_revision": 1 },
                    "target": { "owner": PLUGIN, "key": "notes" },
                    "ordinal": 0
                }]
            }],
            "limits": {
                "max_sections": 128,
                "max_wrappers": 256,
                "max_section_bytes": 32768,
                "max_total_bytes": 262144,
                "render_budget_ms": 2000
            }
        },
        "sections": [{
            "section": { "owner": PLUGIN, "key": "notes" },
            "placement": "initial_instructions",
            "base": { "kind": "omitted" },
            "wraps": [{
                "wrap": { "owner": PLUGIN, "key": "frame" },
                "output": { "kind": "omitted" }
            }],
            "value": { "kind": "omitted" }
        }]
    }))
    .expect("a version-1 snapshot decodes through the facade");
    assert_eq!(snapshot.plan.sections[0].section, notes());
    assert_eq!(snapshot.sections[0].value, RecordedSectionText::Omitted);
    core.shutdown().await.expect("shutdown");
}
