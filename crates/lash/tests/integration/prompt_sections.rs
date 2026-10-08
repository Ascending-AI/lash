//! FIG-5254, FIG-5258: a host reaches the whole prompt-section contract
//! through the facade alone. It registers a section, a section family and a
//! wrapper, sets the prompt plan through session config, reads the recorded
//! plan and the registered catalog back, previews the plan's resolution, and
//! decodes a snapshot record.

#![expect(
    clippy::expect_used,
    reason = "test target: the setup helpers around the law are test code too"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash::plugins::{
    OfferedTools, PluginDeclaration, PluginDefinition, PromptFamilySection, PromptInput,
    PromptRenderError, PromptSectionFamilySpec, PromptSectionSource, PromptSectionSpec,
    PromptWrapSpec, PromptWrapTarget, SectionText,
};
use lash::prompt::{
    PlacementSource, PromptPlacement, PromptPlan, PromptPlanError, PromptPurpose, PromptSectionId,
    PromptSectionKey, PromptSectionPlacement, PromptWrapKey, RecordedSectionText,
};

const PLUGIN: &str = "prompt-witness";
const MODEL: &str = "prompt-witness-model";

fn notes() -> PromptSectionId {
    PromptSectionId::new(
        PLUGIN,
        PromptSectionKey::new("notes").expect("valid section key"),
    )
}

fn key(key: &str) -> PromptSectionKey {
    PromptSectionKey::new(key).expect("valid section key")
}

fn id(local: &str) -> PromptSectionId {
    PromptSectionId::new(PLUGIN, key(local))
}

/// Tool guidance: one section per offered tool module.
struct ModuleGuidance;

impl PromptSectionSource for ModuleGuidance {
    fn sections(&self, offered: &OfferedTools) -> Vec<PromptFamilySection> {
        let mut modules = offered
            .manifests()
            .filter_map(|manifest| manifest.module.as_ref().map(|module| module.name.clone()))
            .collect::<Vec<_>>();
        modules.dedup();
        modules
            .into_iter()
            .map(|module| PromptFamilySection {
                suffix: key(&module),
                renderer: Arc::new(move |_: &PromptInput<'_>| {
                    Ok::<_, PromptRenderError>(SectionText::Text(format!("use {module} well")))
                }),
            })
            .collect()
    }
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
        reg.prompt().section(
            PromptSectionSpec::new(key("intro"), PromptPlacement::InitialInstructions),
            Arc::new(|_: &PromptInput<'_>| {
                Ok::<_, PromptRenderError>(SectionText::text("witness intro"))
            }),
        )?;
        reg.prompt().family(
            PromptSectionFamilySpec::new(key("module"), PromptPlacement::InitialInstructions),
            Arc::new(ModuleGuidance),
        )?;
        self.registered.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// A core over SQLite memory stores with the witness plugin installed.
async fn witness_core(registered: Arc<AtomicBool>) -> lash::LashCore {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a memory store set");
    witness_core_over(registered, Arc::new(stores), true).await
}

async fn witness_core_over(
    registered: Arc<AtomicBool>,
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    installed: bool,
) -> lash::LashCore {
    let builder = lash::LashCore::standard_builder(lash_conformance::backend_over(stores))
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
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended());
    let builder = if installed {
        builder.plugin(Arc::new(Witness { registered }))
    } else {
        builder
    };
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "prompt-witness-worker",
            "prompt-witness-boot",
        ))
        .expect("core")
}

#[tokio::test]
async fn a_host_registers_sections_sets_the_plan_and_decodes_a_snapshot_through_the_facade() {
    let registered = Arc::new(AtomicBool::new(false));
    let core = witness_core(Arc::clone(&registered)).await;
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
        .expect("config accepted")
        .await_outcome(&config)
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
        .expect("config accepted")
        .await_outcome(&config)
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

    for (index, plan) in [
        PromptPlan {
            order: vec![id("typo")],
            ..PromptPlan::default()
        },
        PromptPlan {
            placements: vec![PromptSectionPlacement {
                section: id("typo"),
                placement: PromptPlacement::Excluded,
            }],
            ..PromptPlan::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let revision = config.revision().await.expect("revision");
        let outcome = config
            .apply(
                lash::config::ConfigWrite::new(format!("unknown-plan-{index}"), revision),
                lash::config::ConfigTransaction::of(lash::config::SetPromptPlan { plan }),
            )
            .await
            .expect("config accepted")
            .await_outcome(&config)
            .await
            .expect("the refusal settles");
        let lash::config::ConfigTransactionOutcome::Refused { refusal } = outcome else {
            panic!("an unknown section id is refused when the plan is set: {outcome:?}");
        };
        assert_eq!(
            refusal.owner_refusal::<lash::config::CoreConfigRefusal>(),
            Some(lash::config::CoreConfigRefusal::PromptPlanRefused {
                error: PromptPlanError::UnknownSection {
                    section: id("typo")
                },
            })
        );
        assert_eq!(
            config.revision().await.expect("revision"),
            revision,
            "a refusal publishes no config"
        );
    }

    let run = lash::TurnId::parse("prompt-witness-call").expect("run id");
    let turn = session
        .send(lash::TurnInput::text("build the plugin session"))
        .id(run.clone())
        .output()
        .await
        .expect("the turn answers");
    assert!(turn.is_success(), "{turn:?}");
    assert!(
        registered.load(Ordering::SeqCst),
        "the plugin's section and wrapper registered"
    );

    let loaded = session
        .admin()
        .prompt()
        .snapshot(&run, 1)
        .await
        .expect("the recorded snapshot reads through the facade")
        .expect("the call retains a snapshot");
    let note = loaded
        .snapshot
        .sections
        .iter()
        .find(|section| section.section == notes())
        .expect("notes was recorded");
    assert_eq!(note.placement, PromptPlacement::InitialInstructions);
    assert_eq!(loaded.text(&note.base), Some("0 notes"));
    assert_eq!(
        loaded.text(&note.wraps[0].output),
        Some("<notes>0 notes</notes>")
    );
    assert_eq!(loaded.text(&note.value), Some("<notes>0 notes</notes>"));
    core.shutdown().await.expect("shutdown");
}

fn module_tool(name: &str, module: &str) -> lash::tools::ToolDefinition {
    let mut tool = lash::tools::ToolDefinition::raw(
        name,
        name,
        "",
        serde_json::json!({"type": "object"}),
        serde_json::json!({}),
    )
    .expect("tool schema admits")
    .with_execution(std::time::Duration::from_secs(120));
    tool.manifest.module = Some(Arc::new(lash::tools::ToolModule {
        name: module.into(),
    }));
    tool
}

/// A call offering `tools` natively, from their pinned catalog.
fn offering(tools: Vec<lash::tools::ToolDefinition>) -> OfferedTools {
    OfferedTools::new(
        Arc::new(lash::plugins::ToolCatalog::from_tool_definitions(tools)),
        false,
    )
}

/// HOST: a host registers sections, a family and a wrapper, states a plan
/// that orders, places and excludes them, and reads the result back, all
/// through the facade. The recorded plan reads back as written; the catalog
/// lists every registration; and the unadmitted preview resolves the plan
/// for a call: a family section only for an offered module, the excluded
/// section recorded as the host's exclusion, and the wrapper over it absent.
#[tokio::test]
async fn a_host_reads_back_its_plan_and_the_catalog_and_previews_the_resolution() {
    let core = witness_core(Arc::new(AtomicBool::new(false))).await;
    let session = crate::created_session(&core, MODEL, "prompt-host-session")
        .await
        .open()
        .await
        .expect("the session opens");
    let plan = PromptPlan {
        order: vec![id("module.github"), id("intro")],
        placements: vec![
            PromptSectionPlacement {
                section: notes(),
                placement: PromptPlacement::Excluded,
            },
            PromptSectionPlacement {
                section: id("module.github"),
                placement: PromptPlacement::CurrentContext,
            },
        ],
        ..PromptPlan::default()
    };
    let config = session.admin().config();
    let revision = config.revision().await.expect("revision");
    let applied = config
        .apply(
            lash::config::ConfigWrite::new("host-plan", revision),
            lash::config::ConfigTransaction::of(lash::config::SetPromptPlan { plan: plan.clone() }),
        )
        .await
        .expect("config accepted")
        .await_outcome(&config)
        .await
        .expect("the plan settles");
    assert!(
        matches!(
            applied,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{applied:?}"
    );

    let prompt = session.admin().prompt();
    assert_eq!(prompt.plan().await.expect("plan"), plan);
    let catalog = prompt.catalog().await.expect("catalog");
    // The witness's own entries; the session's protocol registers its own.
    assert_eq!(
        catalog
            .sections()
            .into_iter()
            .map(|info| info.section)
            .filter(|section| section.owner == PLUGIN)
            .collect::<Vec<_>>(),
        vec![notes(), id("intro")]
    );
    assert_eq!(
        catalog
            .families()
            .into_iter()
            .map(|info| info.family)
            .collect::<Vec<_>>(),
        vec![id("module")]
    );
    assert_eq!(catalog.wraps().len(), 1);

    let offered = offering(vec![module_tool("search", "github")]);
    let preview = prompt
        .preview(&PromptPurpose::Turn, &offered)
        .await
        .expect("preview")
        .expect("the plan resolves");
    assert_eq!(
        preview
            .sections
            .iter()
            .filter(|section| section.section.owner == PLUGIN)
            .map(|section| (
                section.section.clone(),
                section.placement,
                section.placement_source
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                id("module.github"),
                PromptPlacement::CurrentContext,
                PlacementSource::Host
            ),
            (
                id("intro"),
                PromptPlacement::InitialInstructions,
                PlacementSource::PluginDefault
            ),
            (notes(), PromptPlacement::Excluded, PlacementSource::Host),
        ]
    );
    assert_eq!(
        preview
            .absent_targets
            .iter()
            .map(|wrap| wrap.target.clone())
            .collect::<Vec<_>>(),
        vec![notes()],
        "the wrapper over the excluded section does not run"
    );
    let unoffered = prompt
        .preview(&PromptPurpose::Turn, &OfferedTools::default())
        .await
        .expect("preview")
        .expect("the plan resolves");
    assert!(
        unoffered
            .sections
            .iter()
            .all(|section| section.section != id("module.github")),
        "no offered module, no guidance section"
    );
    let run = lash::TurnId::parse("prompt-excluded-call").expect("run id");
    let turn = session
        .send(lash::TurnInput::text("read without notes"))
        .id(run.clone())
        .output()
        .await
        .expect("the turn answers");
    assert!(turn.is_success(), "{turn:?}");
    let loaded = prompt
        .snapshot(&run, 1)
        .await
        .expect("snapshot read")
        .expect("recorded call");
    let note = loaded
        .snapshot
        .sections
        .iter()
        .find(|section| section.section == notes())
        .expect("excluded section recorded");
    assert_eq!(note.placement, PromptPlacement::Excluded);
    assert_eq!(note.base, RecordedSectionText::Omitted);
    assert_eq!(note.value, RecordedSectionText::Omitted);
    assert!(
        loaded.texts.values().all(|text| !text.contains("0 notes")),
        "excluded text never renders"
    );
    core.shutdown().await.expect("shutdown");
}

/// REMOVED-SECTION: a host removes a plugin after admitting a plan. The
/// next turn succeeds and its recorded call reports the skipped overrides.
#[tokio::test]
async fn a_removed_plugins_plan_override_is_recorded_and_the_turn_succeeds() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("stores"),
    );
    let core = witness_core_over(Arc::new(AtomicBool::new(false)), Arc::clone(&stores), true).await;
    let session = crate::created_session(&core, MODEL, "removed-prompt-session")
        .await
        .open()
        .await
        .expect("open");
    let config = session.admin().config();
    let plan = PromptPlan {
        order: vec![notes()],
        placements: vec![PromptSectionPlacement {
            section: notes(),
            placement: PromptPlacement::CurrentContext,
        }],
        ..PromptPlan::default()
    };
    let applied = config
        .apply(
            lash::config::ConfigWrite::new(
                "before-removal",
                config.revision().await.expect("revision"),
            ),
            lash::config::ConfigTransaction::of(lash::config::SetPromptPlan { plan: plan.clone() }),
        )
        .await
        .expect("config accepted")
        .await_outcome(&config)
        .await
        .expect("plan settles");
    assert!(
        matches!(
            applied,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{applied:?}"
    );
    core.shutdown().await.expect("shutdown before redeploy");
    drop(session);
    drop(core);

    let core = witness_core_over(Arc::new(AtomicBool::new(false)), stores, false).await;
    let session = core
        .session(lash::SessionId::from("removed-prompt-session"))
        .open()
        .await
        .expect("open without plugin");
    assert_eq!(
        session
            .admin()
            .prompt()
            .plan()
            .await
            .expect("plan retained"),
        plan
    );
    let run = lash::TurnId::parse("removed-prompt-call").expect("run id");
    let turn = session
        .send(lash::TurnInput::text("continue without the plugin"))
        .id(run.clone())
        .output()
        .await
        .expect("turn answers");
    assert!(turn.is_success(), "{turn:?}");
    let loaded = session
        .admin()
        .prompt()
        .snapshot(&run, 1)
        .await
        .expect("snapshot read")
        .expect("recorded call");
    assert_eq!(loaded.snapshot.plan.absent_overrides, vec![notes()]);
    assert!(
        loaded
            .snapshot
            .sections
            .iter()
            .all(|section| section.section.owner != PLUGIN)
    );
    core.shutdown().await.expect("shutdown");
}
