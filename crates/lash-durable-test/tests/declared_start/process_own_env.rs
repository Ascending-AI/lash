//! A process runs under its own recorded environment, never its starting
//! session's (FIG-6034, D18): PROCESS-OWN-ENV and NESTED-ENV.
//!
//! A code mode cell of a turn sent with its own model, a hidden tool and a
//! capability ref starts a detached process through the run's policy, which
//! names the probe component and the process controls. The process holds
//! in its first step while the host narrows the session's tools, changes
//! its model and deletes it; the deployment stops. On a fresh worker over
//! the same stores the process reads its catalog, model and capability refs
//! and starts two children: one that inherits its environment and one under
//! a new one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;

const SESSION_MODEL: &str = "own-env-session-model";
const RUN_MODEL: &str = "own-env-run-model";
const CHANGED_MODEL: &str = "own-env-changed-model";
const SESSION: &str = "own-env-session";

const PROBE: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("own-env-probe", lash_core::FormatVersion::ONE);
/// Holds the process's first step on the first worker.
const WAIT: &str = "probe_wait";
/// Records what the step reads.
const READ: &str = "probe_read";
/// Offered at the send and narrowed out of the session afterwards.
const LATER: &str = "probe_later";
/// The probe component provides it; the send hides it.
const EXTRA: &str = "probe_extra";
/// The session offers it; no component provides it.
const SESSION_ONLY: &str = "probe_session_only";

const CELL: &str = r#"const body = async () => {
  await tools.probe_wait({});
  const own = await tools.probe_read({ who: "parent" });
  const inherited = await processes.start({
    definition: async () => await tools.probe_read({ who: "inherit" }),
    label: "inherit",
  });
  const fresh = await processes.start({
    definition: async () => await tools.probe_read({ who: "new" }),
    label: "new",
  });
  return { own, inherited: await inherited, fresh: await fresh };
};
await processes.start({ definition: body, label: "parent" });
await control.finish("started");"#;

fn slot() -> lash::SlotId {
    lash::SlotId::new("search")
}

fn capability() -> lash::CapabilityRef {
    lash::CapabilityRef {
        contract: lash::ContractRef::new("search", 1),
        binding: lash::BindingId::new("search:own-env"),
        args: serde_json::json!({ "scope": "team" }),
    }
}

fn granted() -> BTreeMap<lash::SlotId, lash::CapabilityRef> {
    BTreeMap::from([(slot(), capability())])
}

fn probe_definition(name: &str) -> lash_core::ToolDefinition {
    use lash_core::ToolDefinitionBindingExt as _;
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    let definition =
        lash_core::ToolDefinition::raw(name, name, "The probe.", object.clone(), object)
            .expect("the probe's schemas")
            .with_execution(Duration::from_secs(120))
            .with_tool_binding(lash_core::ToolBinding::new(["tools"], name));
    if name == WAIT {
        definition.with_execution_policy(lash_core::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(4).unwrap(),
            1,
            1,
        ))
    } else {
        definition
    }
}

/// What one `probe_read` step read.
#[derive(Clone, Debug)]
struct Read {
    who: String,
    model: Option<String>,
    capabilities: BTreeMap<lash::SlotId, lash::CapabilityRef>,
    catalog: BTreeSet<String>,
}

/// One component build for a process: its environment's reference and the
/// capability refs its context carried.
type Build = (
    lash_core::ProcessEnvRef,
    BTreeMap<lash::SlotId, lash::CapabilityRef>,
);

/// What the probe saw. It outlives every worker.
#[derive(Default)]
struct Seen {
    reads: Mutex<Vec<Read>>,
    /// The environment and capability refs of each build for a process.
    builds: Mutex<Vec<Build>>,
    entered: tokio::sync::Notify,
}

struct ProbeBody {
    seen: Arc<Seen>,
    holds: bool,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for ProbeBody {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.name() == WAIT {
            if self.holds {
                self.seen.entered.notify_one();
                std::future::pending::<()>().await;
            }
            return lash_core::ToolOutcome::ok(serde_json::json!("waited")).into();
        }
        let who = call.args["who"].as_str().unwrap_or_default().to_owned();
        let model = call
            .context
            .sessions()
            .model()
            .await
            .ok()
            .map(|model| model.model.model.key().as_str().to_owned());
        let catalog = call
            .context
            .tool_catalog()
            .map(|catalog| {
                catalog
                    .tools
                    .iter()
                    .map(|entry| entry.manifest.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.seen.reads.lock_recover().push(Read {
            who: who.clone(),
            model,
            capabilities: call.context.run_capabilities().clone(),
            catalog,
        });
        lash_core::ToolOutcome::ok(serde_json::json!(who)).into()
    }
}

fn probe_tools(seen: &Arc<Seen>, holds: bool, names: &[&str]) -> Arc<dyn lash_core::ToolProvider> {
    Arc::new(lash::tools::StaticToolProvider::new(
        names.iter().map(|name| probe_definition(name)).collect(),
        ProbeBody {
            seen: Arc::clone(seen),
            holds,
        },
    ))
}

/// The probe component: the probe tools, recording the capability refs
/// each build for a process reads from its context.
struct ProbeComponent {
    seen: Arc<Seen>,
    tools: Arc<dyn lash_core::ToolProvider>,
}

impl lash::process::ProcessComponentFactory for ProbeComponent {
    fn declaration(&self) -> lash::process::ProcessComponentDeclaration {
        lash::process::ProcessComponentDeclaration::initial(PROBE.component(), PROBE.format())
            .expect("the component id")
    }

    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<()>()
    }

    fn validate(
        &self,
        entry: &lash::process::ProcessComponentEntry,
        _env: &lash::process::ProcessEnv,
    ) -> Result<(), lash::process::ProcessEnvRefusal> {
        entry.config().decode::<()>().map_err(|error| {
            lash::process::ProcessEnvRefusal::ConfigRefused {
                id: entry.id().to_string(),
                reason: error.to_string(),
            }
        })
    }

    fn build(
        &self,
        cx: &lash::process::ProcessComponentContext<'_>,
    ) -> Result<lash::process::ProcessComponentSpec, lash_core::PluginError> {
        if cx.process.is_some() {
            let env = cx
                .env
                .stable_ref()
                .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
            self.seen
                .builds
                .lock_recover()
                .push((env, cx.capabilities.clone()));
        }
        let mut spec = lash::process::ProcessComponentSpec::default();
        spec.tool_providers
            .push((lash::hook_key!("probe"), Arc::clone(&self.tools)));
        Ok(spec)
    }
}

/// The policy every start of the law runs: a start labelled `inherit`
/// inherits its enclosing process's environment, one labelled `new` runs
/// under [`new_environment`], and any other is the run's with the probe
/// component and the process controls.
struct Policy;

/// The environment a `new` child states: the read probe alone, a tool-call
/// limit of its own and no model.
fn new_environment() -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(
        lash_core::SessionToolAccess::restricted([probe_definition(READ)]).expect("the probe"),
        lash::MaxToolCalls::new(7),
    )
    .component(PROBE, &())
    .build()
    .expect("the environment builds")
}

#[async_trait::async_trait]
impl lash_core::ProcessEnvPolicy for Policy {
    async fn choose(
        &self,
        cx: &lash_core::ProcessEnvCx<'_>,
    ) -> Result<lash_core::ProcessStartChoice, lash_core::ProcessEnvPolicyError> {
        match cx.label {
            Some("inherit") => cx.inherit(),
            Some("new") => Ok(lash_core::ProcessStartChoice {
                env: lash_core::ProcessEnvChoice::New(new_environment()),
                engine_settings: cx.engine_defaults()?,
            }),
            _ => {
                let (Some(run), Some(offered)) = (cx.run, cx.offered) else {
                    return Err(lash_core::ProcessEnvPolicyError::Fault(
                        "a top-level start has its run".into(),
                    ));
                };
                let entries = [PROBE, lash::process_controls::PROCESS_CONTROLS_CONFIG]
                    .into_iter()
                    .map(|key| lash::process::ProcessComponentEntry::new(key, &(), []))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(lash_core::ProcessEnvPolicyError::Refused)?;
                Ok(lash_core::ProcessStartChoice {
                    env: lash_core::ProcessEnvChoice::New(
                        lash::process::ProcessEnv::from_run(run, offered, entries)
                            .map_err(lash_core::ProcessEnvPolicyError::Refused)?,
                    ),
                    engine_settings: cx.engine_defaults()?,
                })
            }
        }
    }
}

fn metadata(model: &str) -> lash_core::LlmProfileMetadata {
    lash_core::LlmProfileMetadata::builder(model)
        .cache_retention(lash_core::provider::CacheRetention::Short)
        .context_window_tokens(200_000)
        .build()
        .expect("the model's metadata")
}

/// One boot `boot` of a code mode core over `stores`, its probe's wait
/// holding when `holds`.
fn worker(
    stores: &Arc<dyn lash_core_execution::StoreSet>,
    seen: &Arc<Seen>,
    holds: bool,
    boot: &str,
) -> (lash::Backend, lash::LashCore) {
    let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(stores))
        .build()
        .expect("the backend assembles");
    let cells = Arc::new(Mutex::new(vec![CELL.to_owned()]));
    let model = lash_core::testing::TestProvider::builder()
        .kind("own-env")
        .complete(move |_request| {
            let code = cells
                .lock_recover()
                .pop()
                .unwrap_or_else(|| "await control.finish(\"idle\");".to_owned());
            async move {
                Ok(lash_core::llm::types::LlmResponse {
                    parts: vec![lash_core::LlmOutputPart::Text {
                        text: format!("<typescript>\n{code}\n</typescript>"),
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        lash::rlm::CellDialect::typescript(),
    )
    .with_worker_service(lash::vm::WorkerService::default());
    let lifetime = |_: &lash_core::StartCx| lash_core::Lifetime::Detached;
    let models = [SESSION_MODEL, RUN_MODEL, CHANGED_MODEL].into_iter().fold(
        lash::LlmProfileRegistry::new(),
        |registry, name| {
            registry
                .register(
                    name,
                    lash::RegisteredLlmProfile::new(metadata(name), model.clone()),
                )
                .expect("each key registers")
        },
    );
    let core = lash::LashCore::rlm_builder(backend.clone(), factory)
        .llm_profiles(Arc::new(models))
        .tools(
            lash::hook_key!("tools"),
            probe_tools(seen, holds, &[WAIT, READ, LATER, EXTRA, SESSION_ONLY]),
        )
        .process_component(
            Arc::new(ProbeComponent {
                seen: Arc::clone(seen),
                tools: probe_tools(seen, holds, &[WAIT, READ, LATER, EXTRA]),
            }),
            PROBE,
        )
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lifetime,
                Arc::new(Policy),
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lifetime,
                Arc::new(Policy),
            ),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(format!("own-env-{boot}")),
            lash::persistence::LeaseIncarnationId::new(format!("own-env-{boot}-boot")),
        ))
        .expect("the core builds");
    (backend, core)
}

/// The process labelled `label`, once it is registered.
async fn labelled(backend: &lash::Backend, label: &str) -> lash_core::ProcessRecord {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let records = lash_core::testing::process_roster_records_for_fixture(
                backend.process_registry().as_ref(),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..lash_core::ProcessListFilter::default()
                },
            )
            .await
            .expect("the registry lists its processes");
            if let Some(record) = records
                .into_iter()
                .find(|record| record.identity.label.as_deref() == Some(label))
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the `{label}` process registers within a minute"))
}

/// The environment `record` runs under, read from the store.
async fn stored_env(
    backend: &lash::Backend,
    record: &lash_core::ProcessRecord,
) -> lash::process::ProcessEnv {
    let bytes = backend
        .stores()
        .process_env_store()
        .get_process_env(
            record
                .env_ref()
                .expect("an engine process has an environment"),
        )
        .await
        .expect("the environment store reads")
        .expect("the environment is stored");
    lash::process::ProcessEnv::from_store_bytes(&bytes).expect("the environment decodes")
}

/// PROCESS-OWN-ENV (D18) and NESTED-ENV (M5). Red claim (D18): this pins
/// Sam's ruling and is not a found bug; main binds no capability ref to a
/// process execution, which no public tool seam there can observe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_runs_under_its_own_recorded_environment_after_its_session_is_gone() {
    let stores: Arc<dyn lash_core_execution::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let seen = Arc::new(Seen::default());
    let session_id = SessionId::try_from(SESSION.to_owned()).unwrap();

    let (backend, first) = worker(&stores, &seen, true, "first");
    let session = first
        .session(session_id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                SESSION_MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(64),
            )
            .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("the session is created");
    let run = lash::RunSpec {
        capabilities: granted(),
        ..lash::RunSpec::default()
    };
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        session
            .send(lash::TurnInput::text("start the process"))
            .run(run)
            .model(RUN_MODEL)
            .tool_access(
                lash::plugins::SessionToolAccess::ambient()
                    .with_hidden_tools([EXTRA])
                    .expect("a hidden tool"),
            )
            .output(),
    )
    .await
    .expect("the turn settles within a minute")
    .expect("the turn settles");
    assert!(output.is_success(), "{output:?}");
    tokio::time::timeout(Duration::from_secs(60), seen.entered.notified())
        .await
        .expect("the process's first step holds");
    let parent = labelled(&backend, "parent").await;
    let env = stored_env(&backend, &parent).await;

    // The host narrows the session's tools, changes its model and deletes
    // it while the process holds.
    let session = first
        .session(session_id.clone())
        .open()
        .await
        .expect("the session opens");
    session
        .admin()
        .tools()
        .set_membership(lash_core::ToolId::from(LATER), false, "own-env:narrow")
        .await
        .expect("the narrowing is submitted")
        .settle_with(
            &session.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect("the narrowing settles");
    let config = session.admin().config();
    let revision = config.revision().await.expect("the config revision");
    let changed = config
        .apply(
            lash::config::ConfigWrite::new("own-env:model", revision),
            lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                model: lash::LlmProfileKey::new(CHANGED_MODEL),
            }),
        )
        .await
        .expect("the model change is accepted")
        .await_outcome(&config)
        .await
        .expect("the model change settles");
    assert!(
        matches!(
            changed,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{changed:?}"
    );
    drop(session);
    let administration = first.session_administration().await;
    lash::LashCore::delete_session(administration.delete_context(&session_id).unwrap())
        .await
        .expect("the session is deleted");
    let catalog = backend.stores().session_store_factory();
    tokio::time::timeout(Duration::from_secs(60), async {
        while !matches!(
            catalog.lookup_session(&session_id).await.unwrap(),
            lash_core::store::SessionLookup::Deleted
        ) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the deletion completes within a minute");
    first.shutdown().await.expect("the first worker stops");
    drop(first);

    let (backend, fresh) = worker(&stores, &seen, false, "fresh");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        fresh.processes().await_output(&parent.id),
    )
    .await
    .expect("the process ends on the fresh worker within a minute")
    .expect("its end is read");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "own": "parent", "inherited": "inherit", "fresh": "new" }),
        "neither child read the deleted session"
    );

    let reads = seen.reads.lock_recover().clone();
    let read = |who: &str| {
        reads
            .iter()
            .find(|read| read.who == who)
            .cloned()
            .unwrap_or_else(|| panic!("the `{who}` step read: {reads:#?}"))
    };
    let own = read("parent");
    // The catalog is the environment's tools that a component provides:
    // the narrowed tool stays, the hidden one and the session-only one
    // never come.
    let admitted: BTreeSet<String> = env
        .tools()
        .restricted_tools()
        .expect("a process's tools are restricted")
        .iter()
        .map(|tool| tool.manifest.name.clone())
        .collect();
    assert!(
        own.catalog.is_subset(&admitted),
        "{own:#?} within {admitted:?}"
    );
    for name in [WAIT, READ, LATER] {
        assert!(own.catalog.contains(name), "{name}: {own:#?}");
    }
    assert!(
        admitted.contains(SESSION_ONLY),
        "the run offered {SESSION_ONLY}"
    );
    for name in [EXTRA, SESSION_ONLY] {
        assert!(!own.catalog.contains(name), "{name}: {own:#?}");
    }
    assert_eq!(own.model.as_deref(), Some(RUN_MODEL), "the run's model");
    assert_eq!(own.capabilities, granted(), "a step's run capabilities");
    let builds = seen.builds.lock_recover().clone();
    let parent_ref = parent.env_ref().expect("the parent's environment");
    assert!(builds.iter().any(|(env, _)| env == parent_ref));
    for (env, capabilities) in &builds {
        let expected = if env == parent_ref {
            granted()
        } else {
            BTreeMap::new()
        };
        assert_eq!(
            *capabilities, expected,
            "a component build reads its env's capability refs"
        );
    }

    // NESTED-ENV.
    let inherited = labelled(&backend, "inherit").await;
    let fresh_child = labelled(&backend, "new").await;
    assert_eq!(
        inherited.env_ref(),
        parent.env_ref(),
        "inherit records the parent's ref"
    );
    assert_eq!(
        inherited.engine_settings(),
        parent.engine_settings(),
        "and its settings"
    );
    assert_ne!(
        fresh_child.env_ref(),
        parent.env_ref(),
        "a new env records its own ref"
    );
    assert_eq!(
        fresh_child.env_ref(),
        Some(&new_environment().stable_ref().unwrap())
    );
    for child in [&inherited, &fresh_child] {
        assert!(parent.provenance.run.is_some());
        assert_eq!(
            child.provenance.run, parent.provenance.run,
            "a child's run is its parent's"
        );
    }
    assert_eq!(read("inherit").model.as_deref(), Some(RUN_MODEL));
    assert_eq!(read("new").model, None, "the new env names no model");
    fresh.shutdown().await.expect("the fresh worker stops");
}
