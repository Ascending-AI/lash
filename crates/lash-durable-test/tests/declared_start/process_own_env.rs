//! A process runs under its own recorded environment, never its starting
//! session's (FIG-6034, D18): PROCESS-OWN-ENV and NESTED-ENV.
//!
//! A code mode cell of a turn sent with its own model, a tool selection that
//! leaves one tool out and a capability ref starts a detached process
//! through the run's policy, which captures the run's recorded surface over
//! the probe component and the process controls. The process holds in its
//! first step while the host narrows the session's tool selection, changes
//! its model and deletes it; the deployment stops. On a fresh worker over
//! the same stores the process reads its catalog, model and capability refs
//! and starts two children: one that inherits its environment and one under
//! a new one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_protocol_types::SessionId;
use lash_protocol_types::sync::MutexExt as _;

const SESSION_MODEL: &str = "own-env-session-model";
const RUN_MODEL: &str = "own-env-run-model";
const CHANGED_MODEL: &str = "own-env-changed-model";
const SESSION: &str = "own-env-session";

const PROBE: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new(SESSION_TOOLS, lash_core::FormatVersion::ONE);
/// Holds the process's first step on the first worker.
const WAIT: &str = "probe_wait";
/// Records what the step reads.
const READ: &str = "probe_read";
/// Offered at the send and narrowed out of the session afterwards.
const LATER: &str = "probe_later";
/// The probe component provides it; the send's selection leaves it out.
const EXTRA: &str = "probe_extra";
/// The plugin that offers the session its probe tools under the selection
/// its own namespace holds.
const SESSION_TOOLS: &str = "own-env-session-tools";
/// The session offers it; no component provides it.
const SESSION_ONLY: &str = "probe_session_only";

const CELL: &str = r#"const body = async () => {
  await tools.probeWait({});
  await tools.probeLater({ who: "later" });
  const own = await tools.probeRead({ who: "parent" });
  const inherited = await processes.start({
    definition: async () => await tools.probeRead({ who: "inherit" }),
    label: "inherit",
  });
  const fresh = await processes.start({
    definition: async () => await tools.probeRead({ who: "new" }),
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
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    let output = serde_json::json!({ "type": "string" });
    let definition =
        lash_core::ToolDefinition::raw(name, format!("tools.{name}"), "The probe.", object, output)
            .expect("the probe's schemas")
            .with_execution(Duration::from_secs(120));
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
    calls: Mutex<Vec<CallContext>>,
}

/// The recorded caller facts the host handler sees, before and after restart.
#[derive(Clone, Debug)]
struct CallContext {
    process: lash_core::ProcessId,
    provenance: lash::process::ProcessProvenance,
    run: Option<lash_core::TurnId>,
}

fn assert_provenance(
    call: &CallContext,
    record: &lash_execution_contracts::process::ProcessRecord,
) {
    assert_eq!(call.process, record.id);
    assert_eq!(
        call.provenance, record.provenance,
        "the full recorded provenance"
    );
    assert!(
        call.provenance.caused_by.is_some(),
        "the declaring invocation"
    );
    assert_eq!(
        call.run,
        record
            .provenance
            .run
            .as_ref()
            .map(|run| run.turn_id.clone()),
        "a process tool call names the starting logical run"
    );
}

struct ProbeBody {
    seen: Arc<Seen>,
    holds: bool,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for ProbeBody {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.seen.calls.lock_recover().push(CallContext {
            process: call
                .context
                .enclosing_process()
                .expect("a process call")
                .clone(),
            provenance: call
                .context
                .process_spawn_provenance()
                .expect("recorded provenance")
                .clone(),
            run: call.context.logical_run(),
        });
        if call.name() == format!("tools.{WAIT}") {
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
                    .entries()
                    .iter()
                    .map(|entry| entry.manifest.id.to_string())
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

    fn recipe(
        self: Arc<Self>,
    ) -> Result<lash::plugins::ProcessRecipe, lash_execution_contracts::plugin_error::PluginError>
    {
        let mut reg =
            lash::plugins::PluginRegistrar::<Self, lash::plugins::registries::Process>::for_process(
                self.declaration().id.as_str(),
            );
        reg.tools()
            .provider(lash::tool_provider_key!("tools"), |this, _| {
                Ok(Arc::clone(&this.tools))
            })?;
        Ok(lash::plugins::ProcessRecipe::new(
            PROBE.component(),
            reg,
            move |cx| {
                if cx.process.is_some() {
                    let env = cx.env.stable_ref().map_err(|error| {
                        lash_execution_contracts::plugin_error::PluginError::Session(
                            error.to_string(),
                        )
                    })?;
                    self.seen
                        .builds
                        .lock_recover()
                        .push((env, cx.capabilities.clone()));
                }
                Ok(Arc::clone(&self))
            },
        ))
    }
}

/// The policy every start of the law runs: a start labelled `inherit`
/// inherits its enclosing process's environment, one labelled `new` runs
/// under [`new_environment`], and any other is the run's with the probe
/// component and the process controls.
struct Policy;

/// The environment a `new` child states: the read probe alone, linked to
/// the probe component's tool, a tool-call limit of its own and no model.
fn new_environment() -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(7))
        .component(PROBE, &())
        .link_tool(
            PROBE.tool_provider(lash::tool_provider_key!("tools")),
            &probe_definition(READ),
        )
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
            Some("inherit") => cx.restrict(),
            Some("new") => Ok(lash_core::ProcessStartChoice {
                fits: Default::default(),
                env: lash_core::ProcessEnvChoice::New(new_environment()),
                engine_settings: cx.engine_defaults()?,
            }),
            _ => {
                let (Some(run), Some(surface)) = (cx.run, cx.surface) else {
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
                    fits: Default::default(),
                    env: lash_core::ProcessEnvChoice::New(
                        cx.capture(run, surface, entries)
                            .await
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
    stores: &Arc<dyn lash_execution_contracts::backend::StoreSet>,
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
    let lifetime = |_: &lash_execution_contracts::process::StartCx| {
        lash_execution_contracts::process::Lifetime::Detached
    };
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
        .plugin(Arc::new(lash::tools::SelectedToolsPlugin::new(
            SESSION_TOOLS,
            probe_tools(seen, holds, &[WAIT, READ, LATER, EXTRA, SESSION_ONLY]),
        )))
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
async fn labelled(
    backend: &lash::Backend,
    label: &str,
) -> lash_execution_contracts::process::ProcessRecord {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let records = lash_core::testing::process_roster_records_for_fixture(
                backend.process_registry().as_ref(),
                &lash_execution_contracts::process::ProcessListFilter {
                    status: lash_execution_contracts::process::ProcessStatusFilter::Any,
                    ..lash_execution_contracts::process::ProcessListFilter::default()
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
    record: &lash_execution_contracts::process::ProcessRecord,
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
/// FIG-6172: host calls expose full recorded process provenance and the
/// starting run before interruption, on resume and from nested children.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_runs_under_its_own_recorded_environment_after_its_session_is_gone() {
    let stores: Arc<dyn lash_execution_contracts::backend::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let seen = Arc::new(Seen::default());
    let session_id = SessionId::try_from(SESSION.to_owned()).unwrap();

    let (backend, first) = worker(&stores, &seen, true, "first");
    let packages = first
        .host_artifacts()
        .cell_packages()
        .await
        .expect("the start's package snapshot");
    let new_env = new_environment()
        .with_package_roots(lash::dialect::ExportView::namespace_roots(&packages.view()));
    let session = first
        .session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            SESSION_MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(64),
            lash_core::NoProgressBudget::bounded(12),
        )))
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
            .id(lash::TurnId::try_from("own-env-start".to_owned()).unwrap())
            .run(run)
            .model(RUN_MODEL)
            .options(
                lash::RunOptionsKey::<lash::tools::ToolSelectionConfig>::new(SESSION_TOOLS),
                lash::tools::ToolSelectionConfig {
                    tools: Some(lash::tools::ToolSelection::AllExcept(vec![
                        lash_core::ToolId::from(EXTRA),
                    ])),
                },
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
    assert_eq!(
        parent.provenance.run,
        Some(lash::TurnAddress::new(session_id.clone(), "own-env-start")),
        "Lash stamped the starting turn"
    );
    assert_provenance(&seen.calls.lock_recover()[0], &parent);

    // The host narrows the session's tool selection, changes its model and
    // deletes it while the process holds.
    let session = first
        .session(session_id.clone())
        .open()
        .await
        .expect("the session opens");
    let config = session.admin().config();
    let revision = config.revision().await.expect("the config revision");
    let narrowed = config
        .apply(
            lash::config::ConfigWrite::new("own-env:narrow", revision),
            lash::config::ConfigTransaction::of(lash::tools::SetToolSelection {
                tools: lash::tools::ToolSelection::AllExcept(vec![lash_core::ToolId::from(LATER)]),
            }),
        )
        .await
        .expect("the narrowing is accepted")
        .await_outcome(&config)
        .await
        .expect("the narrowing settles");
    assert!(
        matches!(
            narrowed,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{narrowed:?}"
    );
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
    // Stop with WAIT's Repeatable attempt still in flight and no outcome:
    // recovery must read the process record on the new worker.
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
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
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
    // The catalog is the environment's linked tools: the effects the
    // process performs, captured from the run's recorded surface. The
    // narrowed tool stays; a tool the process never performs is never
    // linked, whatever the run offered.
    let linked: BTreeSet<String> = env
        .effects()
        .values()
        .map(|link| link.tool.to_string())
        .collect();
    assert!(own.catalog.is_subset(&linked), "{own:#?} within {linked:?}");
    for name in [WAIT, READ, LATER] {
        assert!(linked.contains(name), "{name}: {linked:?}");
        assert!(own.catalog.contains(name), "{name}: {own:#?}");
    }
    for name in [EXTRA, SESSION_ONLY] {
        assert!(!linked.contains(name), "{name}: {linked:?}");
        assert!(!own.catalog.contains(name), "{name}: {own:#?}");
    }
    assert_eq!(own.model.as_deref(), Some(RUN_MODEL), "the run's model");
    assert_eq!(own.capabilities, granted(), "a step's run capabilities");
    let builds = seen.builds.lock_recover().clone();
    let parent_ref = parent.env_ref().expect("the parent's environment");
    assert!(builds.iter().any(|(env, _)| env == parent_ref));
    let inherited = labelled(&backend, "inherit").await;
    let inherited_ref = inherited.env_ref().expect("the restricted environment");
    for (env, capabilities) in &builds {
        let expected = if env == parent_ref || env == inherited_ref {
            granted()
        } else {
            BTreeMap::new()
        };
        assert_eq!(
            *capabilities, expected,
            "a component build reads its env's capability refs"
        );
    }

    // NESTED-ENV: the child performs the read alone, so restricting the
    // parent's env records a derived env holding the parent's own link for
    // it, never re-resolved.
    let fresh_child = labelled(&backend, "new").await;
    let restricted = stored_env(&backend, &inherited).await;
    let read_effect = lash_core_execution::testing::effect_of(&probe_definition(READ));
    assert_eq!(
        restricted.effects().keys().collect::<Vec<_>>(),
        vec![&read_effect],
        "restrict links the child's effects alone"
    );
    assert_eq!(
        restricted.effects().get(&read_effect),
        env.effects().get(&read_effect),
        "with the parent's recorded link"
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
    assert_eq!(fresh_child.env_ref(), Some(&new_env.stable_ref().unwrap()));
    for child in [&inherited, &fresh_child] {
        assert!(parent.provenance.run.is_some());
        assert_eq!(
            child.provenance.run, parent.provenance.run,
            "a child's run is its parent's"
        );
    }
    assert_eq!(read("inherit").model.as_deref(), Some(RUN_MODEL));
    assert_eq!(read("new").model, None, "the new env names no model");
    let calls = seen.calls.lock_recover().clone();
    assert!(
        calls
            .iter()
            .filter(|call| call.process == parent.id)
            .count()
            >= 3,
        "the parent called before interruption and after resume"
    );
    for record in [&parent, &inherited, &fresh_child] {
        assert!(calls.iter().any(|call| call.process == record.id));
        for call in calls.iter().filter(|call| call.process == record.id) {
            assert_provenance(call, record);
        }
    }
    for child in [&inherited, &fresh_child] {
        assert_eq!(child.provenance.originator, parent.provenance.originator);
        assert_ne!(
            child.provenance.caused_by, parent.provenance.caused_by,
            "a child records its own declaring invocation"
        );
    }
    fresh.shutdown().await.expect("the fresh worker stops");
}
