//! Process claim requirements retain typed component callbacks and fence
//! incompatible component state formats after release and reopen.

use super::*;

/// A write step named `step` on `x`.
fn write_step(step: &str, x: i64) -> lash_execution_contracts::process::StepRequest {
    lash_execution_contracts::process::StepRequest::Tool {
        step: lash_execution_contracts::process::StepName(step.to_owned()),
        use_: operations::use_for(&write_definition()),
        input: serde_json::json!({ "x": x }),
        site: None,
    }
}

/// The idle engine: it writes once, then sleeps until its start's
/// `until_ms` holding no step, and answers once it wakes.
const IDLE_ENGINE: &str = "core-node-idle-engine";

fn idle_engine_advance(
    state: &mut serde_json::Value,
    event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    match event {
        lash_execution_contracts::process::EngineEvent::Started { payload } => {
            *state = payload;
            lash_execution_contracts::process::EngineAction::Steps {
                steps: vec![write_step("write", 1)],
                wake: None,
            }
        }
        lash_execution_contracts::process::EngineEvent::StepSettled { .. } => {
            lash_execution_contracts::process::EngineAction::Sleep {
                until: lash_core::durable_port::DurableInstant(
                    state["until_ms"].as_i64().unwrap_or_default(),
                ),
                site: None,
            }
        }
        lash_execution_contracts::process::EngineEvent::Woke => {
            answer(serde_json::json!({ "woke": true }))
        }
        lash_execution_contracts::process::EngineEvent::Cancelled { origin, .. } => {
            cancelled(origin)
        }
        _ => lash_execution_contracts::process::EngineAction::Idle,
    }
}

/// The two-write engine: it writes twice at once, a quick write and a held
/// one, and waits.
const TWO_WRITE_ENGINE: &str = "core-node-two-write-engine";

fn two_write_engine_advance(
    _state: &mut serde_json::Value,
    event: lash_execution_contracts::process::EngineEvent,
) -> lash_execution_contracts::process::EngineAction {
    match event {
        lash_execution_contracts::process::EngineEvent::Started { .. } => {
            lash_execution_contracts::process::EngineAction::Steps {
                steps: vec![write_step("quick", 1), write_step("held", -1)],
                wake: None,
            }
        }
        lash_execution_contracts::process::EngineEvent::Cancelled { origin, .. } => {
            cancelled(origin)
        }
        _ => lash_execution_contracts::process::EngineAction::Idle,
    }
}

fn wall_millis() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the wall clock is past the epoch")
            .as_millis(),
    )
    .expect("milliseconds fit")
}

/// What the process actor `process` carried as its claim requirements,
/// read by a probe node that serves `serves` and claims it once released.
async fn claimed_requirements(
    backend: &lash::Backend,
    process: &lash_vocabulary::identity::ProcessId,
    serves: Vec<String>,
) -> Vec<String> {
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec, Release};
    let store = backend.durable();
    let probe = store
        .register_node(&NodeSpec {
            node: NodeId::new("law-requirement-probe"),
            decodes: backend.formats().decodes(),
            serves,
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .expect("register the probe node");
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let snapshot = store.actor(&actor).await.expect("read the actor");
    let mut requires = None;
    for claim in store.claim(&probe, 64).await.expect("claim") {
        if claim.actor == actor {
            requires = Some(claim.requires.clone());
        }
        let mut tx = store
            .begin(&claim.actor, claim.epoch)
            .await
            .expect("open the probe's transaction");
        tx.give_up(Release::Ready);
        store
            .commit(tx, CommitLabel::new("law.probe_requires"))
            .await
            .expect("release the probed actor");
    }
    store
        .release_node(&probe)
        .await
        .expect("release the probe node");
    requires.unwrap_or_else(|| panic!("the stopped node released the process ready: {snapshot:?}"))
}

/// The config a typed write component decodes: a component whose config is
/// not `()` (FIG-6075 R-1).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TypedWriteConfig {
    label: String,
}

/// The typed write component's config key.
const TYPED_WRITE_COMPONENT: lash_core::ProcessConfigKey<TypedWriteConfig> =
    lash_core::ProcessConfigKey::new(
        "core-node-typed-write",
        lash_kernel_value::providers::FormatVersion::ONE,
    );

/// A component that contributes the write tool over `world` and decodes a
/// [`TypedWriteConfig`].
struct TypedWrite {
    world: Arc<World>,
}

impl lash::process::ProcessComponentFactory for TypedWrite {
    fn declaration(&self) -> lash::process::ProcessComponentDeclaration {
        lash::process::ProcessComponentDeclaration::initial(
            TYPED_WRITE_COMPONENT.component(),
            TYPED_WRITE_COMPONENT.format(),
        )
        .expect("the typed write component's id")
    }

    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<TypedWriteConfig>()
    }

    fn validate(
        &self,
        entry: &lash_core::ProcessComponentEntry,
        _env: &lash::process::ProcessEnv,
    ) -> Result<(), lash_core::ProcessEnvRefusal> {
        entry
            .config()
            .decode::<TypedWriteConfig>()
            .map(drop)
            .map_err(|error| lash_core::ProcessEnvRefusal::ConfigRefused {
                id: entry.id().to_string(),
                reason: error.to_string(),
            })
    }

    fn recipe(
        self: Arc<Self>,
    ) -> Result<lash::plugins::ProcessRecipe, lash_execution_contracts::plugin_error::PluginError>
    {
        lash::process::ProcessComponentSpec::new()
            .with_tool_provider(lash::tool_provider_key!("tools"), write_tool(&self.world))
            .recipe(TYPED_WRITE_COMPONENT.component())
    }
}

/// IDLE-RUN-NEVER-STRANDED for a component whose config is not `()`
/// (FIG-6075 R-1; OWED-IS-NOT-INSTALLED, FIG-6089): a process writes twice
/// through a typed-config component, and its held write is unsettled when
/// its node stops. The node serves the component's write provider from its
/// installed registrations, so a node of the same deployment claims the
/// released process: what it requires is served.
async fn a_typed_config_components_unsettled_step_is_served_after_release(tier: Tier) {
    let world = Arc::new(World::default());
    let (stores, _keep) = stores(tier).await;
    let (backend, core) = node_over(
        stores,
        lash_core_execution::DurableSettings::default(),
        vec![Arc::new(ScriptEngine {
            kind: TWO_WRITE_ENGINE,
            advance: two_write_engine_advance,
        })],
        |backend| {
            lash::LashCore::standard_builder(backend.clone()).process_component(
                Arc::new(TypedWrite {
                    world: Arc::clone(&world),
                }),
                TYPED_WRITE_COMPONENT,
            )
        },
    );
    let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(
            TYPED_WRITE_COMPONENT,
            &TypedWriteConfig {
                label: "typed".to_owned(),
            },
        )
        .execution(operations::environment(
            TYPED_WRITE_COMPONENT.tool_provider(lash::tool_provider_key!("tools")),
            &[write_definition()],
        ))
        .build()
        .expect("the typed environment builds");
    let process = start_under(
        &core,
        &environment,
        TWO_WRITE_ENGINE,
        serde_json::json!({}),
        |request| request,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), world.held.notified())
        .await
        .expect("the held write starts");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let serves = lash::testing::node_serves(&core);
    core.shutdown().await.expect("the node stops");
    let provider = TYPED_WRITE_COMPONENT.tool_provider(lash::tool_provider_key!("tools"));
    let token = lash_vocabulary::claims::ObligationToken::for_callback(
        provider.registry(),
        &provider.callback_identity(),
    )
    .to_string();
    let requires = claimed_requirements(&backend, &process, serves).await;
    assert!(
        requires.contains(&token),
        "the unsettled held write keeps the typed component's provider required: {requires:?}"
    );
}

on_every_tier!(a_typed_config_components_unsettled_step_is_served_after_release);

/// COMPONENT-STATE-FORMAT-FENCES-OLD-NODES: native state written by a
/// format-2 component is fenced even when its starting config was format 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn component_state_format_fences_old_nodes_on_sqlite_memory() {
    let world = Arc::new(World::default());
    let (stores, _keep) = stores(Tier::SqliteMemory).await;
    let two = lash_kernel_value::providers::FormatVersion::new(2).unwrap();
    let (backend, core) = node_over(
        Arc::clone(&stores),
        lash_core_execution::DurableSettings::default(),
        vec![Arc::new(ScriptEngine {
            kind: IDLE_ENGINE,
            advance: idle_engine_advance,
        })],
        |backend| {
            lash::LashCore::standard_builder(backend.clone()).process_component(
                Arc::new(FormatWrite {
                    world: Arc::clone(&world),
                    native: lash_kernel_value::providers::FormatVersion::ONE,
                    conversions: Arc::new(AtomicUsize::new(0)),
                    seen: Arc::default(),
                }),
                WRITE_COMPONENT,
            )
        },
    );
    let process = start_under(
        &core,
        &write_environment(),
        IDLE_ENGINE,
        serde_json::json!({ "until_ms": wall_millis() + 60_000 }),
        |request| request,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), async {
        while world.writes.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the native component writes");
    tokio::time::sleep(Duration::from_millis(250)).await;
    let old_serves = lash::testing::node_serves(&core);
    core.shutdown().await.unwrap();
    wake_format_process(&backend, &process).await;
    let converted_view = Arc::new(Mutex::new(Vec::new()));
    let (_, newer) = node_over(
        stores,
        lash_core_execution::DurableSettings::default(),
        vec![Arc::new(ScriptEngine {
            kind: IDLE_ENGINE,
            advance: idle_engine_advance,
        })],
        |backend| {
            lash::LashCore::standard_builder(backend.clone()).process_component(
                Arc::new(FormatWrite {
                    world: Arc::clone(&world),
                    native: two,
                    conversions: Arc::new(AtomicUsize::new(0)),
                    seen: Arc::clone(&converted_view),
                }),
                lash_core::ProcessConfigKey::<()>::new(WRITE_COMPONENT.component(), two),
            )
        },
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if converted_view
                .lock()
                .unwrap()
                .iter()
                .any(|(_, joined)| *joined == serde_json::json!([7, 11]))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the converted view is durably recorded before binding");
    let serves = lash::testing::node_serves(&newer);
    newer.shutdown().await.unwrap();
    wake_format_process(&backend, &process).await;
    use lash_core::durable_port::{NodeId, NodeSpec};
    let old = backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new("format-one-probe"),
            decodes: backend.formats().decodes(),
            serves: old_serves,
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    assert!(
        backend
            .durable()
            .claim(&old, 64)
            .await
            .unwrap()
            .iter()
            .all(|claim| claim.actor
                != lash_core::durable_port::ActorKey::process(process.as_str()).unwrap()),
        "an old node cannot claim the format-2 state"
    );
    backend.durable().release_node(&old).await.unwrap();
    let requires = claimed_requirements(&backend, &process, serves).await;
    assert!(
        requires.contains(
            &lash_vocabulary::claims::ObligationToken::for_component(
                &lash_kernel_value::providers::ProcessComponentId::new(WRITE_COMPONENT.component())
                    .unwrap(),
                two
            )
            .to_string()
        ),
        "native component state requires format 2, even with format-1 starting config: {requires:?}"
    );
}

struct FormatWrite {
    world: Arc<World>,
    native: lash_kernel_value::providers::FormatVersion,
    conversions: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<(serde_json::Value, serde_json::Value)>>>,
}
impl lash::process::ProcessComponentFactory for FormatWrite {
    fn declaration(&self) -> lash::process::ProcessComponentDeclaration {
        let mut declaration = lash::process::ProcessComponentDeclaration::initial(
            WRITE_COMPONENT.component(),
            self.native,
        )
        .unwrap();
        if self.native.get() == 2 {
            declaration
                .readable_formats
                .insert(0, lash_kernel_value::providers::FormatVersion::ONE);
        }
        declaration
    }
    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<()>()
    }
    fn validate(
        &self,
        _entry: &lash_core::ProcessComponentEntry,
        _env: &lash::process::ProcessEnv,
    ) -> Result<(), lash_core::ProcessEnvRefusal> {
        Ok(())
    }
    fn migrate_format(
        &self,
        from: lash_kernel_value::providers::FormatVersion,
        namespace: lash::plugins::FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, lash::plugins::FormatRefusal> {
        assert_eq!(from, lash_kernel_value::providers::FormatVersion::ONE);
        assert_eq!(self.native.get(), 2);
        match namespace {
            lash::plugins::FormatNamespace::State => {
                self.conversions.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    value,
                    serde_json::json!({ "left": 7, "right": 11 }),
                    "the converter sees the whole committed view"
                );
                Ok(serde_json::json!({ "joined": [value["left"], value["right"]] }))
            }
            lash::plugins::FormatNamespace::Config => Ok(value),
        }
    }
    fn recipe(
        self: Arc<Self>,
    ) -> Result<lash::plugins::ProcessRecipe, lash_execution_contracts::plugin_error::PluginError>
    {
        let mut reg = lash::plugins::PluginRegistrar::<
            StateWrite,
            lash::plugins::registries::Process,
        >::for_process(WRITE_COMPONENT.component());
        reg.tools()
            .provider(lash::tool_provider_key!("tools"), |this, _| {
                Ok(Arc::new(lash::tools::StaticToolProvider::new(
                    vec![write_definition()],
                    this.clone(),
                )))
            })?;
        Ok(lash::plugins::ProcessRecipe::new(
            WRITE_COMPONENT.component(),
            reg,
            move |cx| {
                if let Some(view) = &cx.state {
                    self.seen.lock().unwrap().push((
                        view.get("left").unwrap_or_default(),
                        view.get("joined").unwrap_or_default(),
                    ));
                }
                Ok(Arc::new(StateWrite {
                    world: Arc::clone(&self.world),
                }))
            },
        ))
    }
}

#[derive(Clone)]
struct StateWrite {
    world: Arc<World>,
}
#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for StateWrite {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world
            .writes
            .lock()
            .unwrap()
            .push((call.context.call_id().clone(), call.args.clone()));
        lash_core::ToolAttemptOutcome::Done {
            result: lash_core::ToolOutcomeDone::ok(serde_json::json!({ "wrote": true }))
                .with_state(
                    lash::plugins::StateCommands::new()
                        .set("left", 7.into())
                        .set("right", 11.into()),
                ),
            intents: Default::default(),
        }
    }
}

/// COMPONENT-STATE-CONVERTS-AT-A-CUT: convert the whole namespace once,
/// then reopen on the recorded postimage without invoking the converter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn component_state_converts_at_a_cut_on_sqlite_memory() {
    let world = Arc::new(World::default());
    let conversions = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (stores, _keep) = stores(Tier::SqliteMemory).await;
    let engine = || -> Vec<Arc<dyn lash_execution_contracts::process::ProcessEngine>> {
        vec![Arc::new(ScriptEngine {
            kind: IDLE_ENGINE,
            advance: idle_engine_advance,
        })]
    };
    let build = |native| {
        let world = Arc::clone(&world);
        let conversions = Arc::clone(&conversions);
        let seen = Arc::clone(&seen);
        move |backend: &lash::Backend| {
            lash::LashCore::standard_builder(backend.clone()).process_component(
                Arc::new(FormatWrite {
                    world,
                    native,
                    conversions,
                    seen,
                }),
                lash_core::ProcessConfigKey::<()>::new(WRITE_COMPONENT.component(), native),
            )
        }
    };
    let (backend, first) = node_over(
        Arc::clone(&stores),
        lash_core_execution::DurableSettings::default(),
        engine(),
        build(lash_kernel_value::providers::FormatVersion::ONE),
    );
    let process = start_under(
        &first,
        &write_environment(),
        IDLE_ENGINE,
        serde_json::json!({ "until_ms": wall_millis() + 120_000 }),
        |request| request,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let facts = backend
                .durable()
                .run_facts(&lash_core::durable_port::domain::OwnerKey::Process(
                    process.clone(),
                ))
                .await
                .unwrap();
            if facts
                .rows
                .iter()
                .any(|row| row.kind == lash_core::durable_port::domain::RunFactKind::XOutcome)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    first.shutdown().await.unwrap();
    for reopen in 0..2 {
        wake_format_process(&backend, &process).await;
        seen.lock().unwrap().clear();
        let (_, next) = node_over(
            Arc::clone(&stores),
            lash_core_execution::DurableSettings::default(),
            engine(),
            build(lash_kernel_value::providers::FormatVersion::new(2).unwrap()),
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if seen
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(left, joined)| left.is_null() && *joined == serde_json::json!([7, 11]))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a binder sees the recorded whole postimage");
        next.shutdown().await.unwrap();
        assert_eq!(
            conversions.load(Ordering::SeqCst),
            1,
            "reopen {reopen} never reconverts a recorded postimage"
        );
    }
    assert_eq!(
        world.writes.lock().unwrap().len(),
        1,
        "reopening never reruns a settled body"
    );
}

async fn wake_format_process(
    backend: &lash::Backend,
    process: &lash_vocabulary::identity::ProcessId,
) {
    use lash_core::durable_port as durable;
    let mut tx = durable::MailTx::new();
    tx.wake(durable::ActorKey::process(process.as_str()).unwrap());
    backend
        .durable()
        .commit_mail(tx, durable::CommitLabel::MAIL_PROCESS)
        .await
        .unwrap();
}
