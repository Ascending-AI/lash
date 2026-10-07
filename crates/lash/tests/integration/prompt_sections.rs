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
    PromptSectionKey, PromptSectionPlacement, PromptSnapshot, PromptWrapKey, RecordedSectionText,
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
    lash::LashCore::standard_builder(lash_conformance::backend_over(Arc::new(stores)))
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

fn module_tool(name: &str, module: &str) -> lash::tools::ToolDefinition {
    let mut tool = lash::tools::ToolDefinition::raw(
        name,
        name,
        "",
        serde_json::json!({"type": "object"}),
        serde_json::json!({}),
    )
    .expect("tool schema admits");
    tool.manifest.module = Some(Arc::new(lash::tools::ToolModule {
        name: module.into(),
    }));
    tool
}

/// A call offering `tools` natively, from their pinned catalog.
fn offering(tools: Vec<lash::tools::ToolDefinition>) -> OfferedTools {
    OfferedTools {
        native: tools
            .iter()
            .map(|tool| tool.manifest.name.clone())
            .collect(),
        callable: Vec::new(),
        catalog: Arc::new(lash::plugins::ToolCatalog::from_tool_definitions(tools)),
    }
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
    core.shutdown().await.expect("shutdown");
}
