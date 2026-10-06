//! FIG-4744 lane routing: the build generation `G` folds in the core's
//! plugins in hook order, and exists only once they are registered.
//!
//! - A plugin-only behaviour revision bump yields a new `G`, and so does the
//!   same plugins in another hook order; the same registration yields the
//!   same `G`.
//! - An engine has no `G` until a core registers its plugins over it, and
//!   serves one composition: a core with another is refused, typed.
//! - Every Run's admission records the composition it was admitted under
//!   (FIG-4747): the Run in flight at the roll records build N's revision of
//!   the plugin, and each Run admitted after the bump records N+1's.
//! - In-flight segments finish on the old build. A session's shift starts on
//!   build N; while its first run is in its model call, build N+1 registers
//!   with the plugin's revision bumped, as a deployment of its own with its
//!   own plugins, and N is marked draining. The run in flight finishes on N
//!   under N's plugin; every run after it runs on N+1 under N+1's.
//! - A config owner's reducers are its plugin's behaviour (FIG-4791): builds
//!   that differ only in them differ in `G`, and a config transaction is
//!   resolved by the build whose lane admits its command run. One admitted
//!   on build N resolves there under N's reducers; one submitted through N
//!   and still queued when N drains is admitted and resolved on N+1 under
//!   N+1's. Neither parks.
//!
//! The generation laws build real engines whose generation the core binds.
//! The config law runs on the Restate server double over SQLite memory. The
//! in-flight law runs on the double over SQLite memory, SQLite file and
//! PostgreSQL, each build stamped with the generation its
//! composition computes; the PostgreSQL leg is ignored in ordinary runs and
//! requires `LASH_POSTGRES_DATABASE_URL`.

use super::drain_hand_over::{DrainLever, Model, Storage, open, prepare, provider};
use super::*;

use lash_core::engine::{BuildGeneration, GenerationUnbound, ShiftRequestId, ShiftStop};
use lash_core::plugin::{
    BehaviorRevision, FormatVersion, PluginDeclaration, PluginDeclarationError,
};

const SEED: u64 = 0x4744_91a3;

/// The plugin whose revision the laws move.
const PLUGIN: &str = "revisioned";

/// The inputs the in-flight law's session holds: one run each.
const RUNS: usize = 3;

const WEDGE: std::time::Duration = std::time::Duration::from_secs(120);

/// A plugin that does nothing, registered as `id`.
fn named(id: &'static str) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial(id),
        lash_core::plugin::PluginSpec::new(),
    ))
}

/// [`PLUGIN`] at behaviour `revision`: after each turn it records the
/// revision that ran in `ran`.
fn revisioned(revision: u32, ran: &Arc<std::sync::Mutex<Vec<u32>>>) -> Arc<dyn PluginFactory> {
    let mut declaration = PluginDeclaration::initial(PLUGIN);
    declaration.behavior_revision =
        BehaviorRevision::new(revision).expect("a revision counts from one");
    let ran = Arc::clone(ran);
    Arc::new(StaticPluginFactory::new(
        declaration,
        lash_core::plugin::PluginSpec::new().with_after_turn(
            crate::hook_key!("after-turn-1"),
            Arc::new(move |_| {
                let ran = Arc::clone(&ran);
                Box::pin(async move {
                    ran.lock_recover().push(revision);
                    Ok(Default::default())
                })
            }),
        ),
    ))
}

/// The builder of a standard core over `backend` with `plugins` registered
/// in order, answering with `provider`.
fn builder(
    backend: lash_core::Backend,
    plugins: Vec<Arc<dyn PluginFactory>>,
    provider: ProviderHandle,
) -> crate::core::LashCoreBuilder {
    let mut builder = LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(provider, mock_llm_profile_spec());
    for plugin in plugins {
        builder = builder.plugin(plugin);
    }
    builder
}

/// A Restate engine as a deployment builds one: over fresh SQLite memory
/// stores, with no generation until a core binds it. Nothing here reaches a
/// server.
async fn unbound_engine() -> Arc<lash_restate::RestateEngine> {
    Arc::new(lash_restate::RestateEngine::new(
        sqlite_memory_store_set().await,
        lash_restate::RestateConfig::new(
            "http://127.0.0.1:9",
            "http://127.0.0.1:9",
            lash_restate::RestateAuthorityId::new("plugin-generation").expect("authority"),
        ),
    ))
}

/// The core of `plugins` over `engine`.
fn core_on(
    engine: &Arc<lash_restate::RestateEngine>,
    plugins: Vec<Arc<dyn PluginFactory>>,
) -> Result<LashCore> {
    builder(
        lash_core::Backend::new(engine.clone()),
        plugins,
        mock_provider(),
    )
    .build(crate::testing::runtime_lease_owner())
}

/// The generation a deployment registering `plugins` runs on.
async fn generation_of(plugins: Vec<Arc<dyn PluginFactory>>) -> BuildGeneration {
    let engine = unbound_engine().await;
    let core = core_on(&engine, plugins).expect("build the core");
    assert_eq!(
        engine.build_generation(),
        Ok(core.build_generation()),
        "the engine runs on the generation its core computed"
    );
    core.build_generation().clone()
}

#[tokio::test]
async fn a_plugin_only_revision_bump_yields_a_new_generation() {
    let ran = Arc::default();
    let first = generation_of(vec![revisioned(1, &ran)]).await;
    assert_eq!(
        first,
        generation_of(vec![revisioned(1, &ran)]).await,
        "the same registration is the same generation"
    );
    assert_ne!(first, generation_of(vec![revisioned(2, &ran)]).await);
    assert_ne!(
        first,
        generation_of(Vec::new()).await,
        "a plugin is part of the generation"
    );
}

#[tokio::test]
async fn reordering_hooks_yields_a_new_generation() {
    assert_ne!(
        generation_of(vec![named("first"), named("second")]).await,
        generation_of(vec![named("second"), named("first")]).await
    );
}

#[tokio::test]
async fn an_engine_has_no_generation_until_a_core_registers_its_plugins() {
    let engine = unbound_engine().await;
    assert_eq!(engine.build_generation(), Err(GenerationUnbound));
    assert!(
        matches!(
            engine.endpoint_builder(lash_restate::RestateProcessWorkerSlot::new()),
            Err(GenerationUnbound)
        ),
        "no endpoint names a lane before the generation exists"
    );
    let core = core_on(&engine, vec![named("first")]).expect("build the core");
    assert_eq!(engine.build_generation(), Ok(core.build_generation()));
    assert!(
        engine
            .endpoint_builder(lash_restate::RestateProcessWorkerSlot::new())
            .is_ok()
    );
}

#[tokio::test]
async fn an_engine_serves_one_composition() {
    let ran = Arc::default();
    let engine = unbound_engine().await;
    let first = core_on(&engine, vec![revisioned(1, &ran)]).expect("build the core");
    let bound = first.build_generation().clone();
    let composed = generation_of(vec![revisioned(2, &ran)]).await;
    let refused = core_on(&engine, vec![revisioned(2, &ran)])
        .err()
        .expect("another composition over the same engine is refused");
    let EmbedError::BuildGenerationRebound(rebound) = refused else {
        panic!("the refusal is typed: {refused}");
    };
    assert_eq!(
        rebound,
        lash_core::engine::GenerationRebound {
            bound: bound.clone(),
            composed,
        }
    );
    assert_eq!(engine.build_generation(), Ok(&bound));
    let again = core_on(&engine, vec![revisioned(1, &ran)])
        .expect("the same composition shares the engine");
    assert_eq!(again.build_generation(), &bound);
}

/// A factory whose declaration is `declared`, registered as `id`.
struct Declares {
    id: &'static str,
    declared: PluginDeclaration,
}

impl lash_core::facade_support::PluginFactory for Declares {
    fn id(&self) -> &'static str {
        self.id
    }

    fn build(
        &self,
        ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        named(self.id).build(ctx)
    }
}

impl lash_core::plugin::PluginMetadata for Declares {
    fn plugin_declaration(&self) -> PluginDeclaration {
        self.declared.clone()
    }
}

#[tokio::test]
async fn a_refused_declaration_builds_no_core_and_binds_no_generation() {
    let second = FormatVersion::new(2).expect("a format counts from one");
    let mut unwritable = PluginDeclaration::initial("stateful");
    unwritable.format_version = second;
    for (declared, expected) in [
        (
            Declares {
                id: "registered",
                declared: PluginDeclaration::initial("another"),
            },
            PluginDeclarationError::IdMismatch {
                factory: "registered".to_owned(),
                declared: "another".to_owned(),
            },
        ),
        (
            Declares {
                id: "stateful",
                declared: unwritable,
            },
            PluginDeclarationError::NativeFormatNotWritable {
                plugin: "stateful".to_owned(),
                format_version: second,
            },
        ),
    ] {
        let engine = unbound_engine().await;
        let refused = core_on(&engine, vec![Arc::new(declared)])
            .err()
            .expect("the declaration is refused");
        let EmbedError::PluginDeclaration(refusal) = refused else {
            panic!("the refusal is typed: {refused}");
        };
        assert_eq!(refusal, expected);
        assert_eq!(engine.build_generation(), Err(GenerationUnbound));
    }
}

/// One shift invocation of the law's session, as the engine's
/// `sys_invocation` reports it.
#[derive(Debug, serde::Deserialize)]
struct ShiftRow {
    idempotency_key: Option<String>,
    pinned_deployment_id: Option<String>,
}

/// A core of `plugins` over `backend` whose `SessionShifts` starts no wall-clock
/// reconcile tick: the law's ask is the session's only shift.
fn deployed(
    backend: lash_core::Backend,
    work: Arc<dyn lash_core::SessionWorkEngine>,
    plugins: Vec<Arc<dyn PluginFactory>>,
    model: &Arc<Model>,
) -> LashCore {
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
        .with_session_work(work)
        .into_backend();
    builder(backend, plugins, provider(model))
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core")
}

async fn in_flight_segments_finish_on_the_old_build(storage: Storage) -> Result<()> {
    // Each build runs on the generation its composition computes, and the
    // bump of one plugin's revision alone separates them.
    let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
    let old_generation = generation_of(vec![revisioned(1, &Arc::default())]).await;
    let next_generation = generation_of(vec![revisioned(2, &Arc::default())]).await;
    assert_ne!(old_generation, next_generation);

    let (opening, _keep) = prepare(storage).await;
    let double = lash_restate_test::backend_with_store_set(
        SEED,
        lash_restate_test::ServerConfig {
            build_generation: old_generation.clone(),
            ..lash_restate_test::ServerConfig::default()
        },
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            open(opening, clock, DrainLever::default())
                .await
                .map_err(lash_restate_test::BackendError::Stores)
        },
    )
    .await
    .expect("the Restate double over the law's stores");
    let model = Arc::new(Model::holding(1));
    let old_core = deployed(
        double.lash_backend(),
        double.explicit_reconcile_session_work(),
        vec![revisioned(1, &ran)],
        &model,
    );
    assert_eq!(old_core.build_generation(), &old_generation);

    let session = "plugin-generation";
    let handle = old_core
        .session(crate::SessionId::parse(session).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let session_id = lash_core::SessionId::from(session);
    let store = lash_core::runtime::live_session_view(&old_core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    for index in 0..RUNS {
        store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text(format!("question {index}")),
            ))
            .await
            .expect("enqueue the input");
    }

    // The shift starts on N, the only build, and its first run reaches its
    // model call.
    let port = old_core.substrate_slot.ports().await.queued;
    let request = ShiftRequestId::new("plugin-generation");
    port.schedule_shift(&session_id, request.clone());
    tokio::time::timeout(WEDGE, model.reached.notified())
        .await
        .expect("the shift's first run reaches its model call");
    assert!(
        ran.lock_recover().is_empty(),
        "no turn has ended while the first run is in its model call"
    );

    // The roll: N+1 registers as a deployment of its own, with the plugin's
    // revision bumped, and the operator marks N draining.
    let next = double
        .add_separate_build(
            next_generation.clone(),
            "next",
            lash_restate_test::DeploymentHooks::default(),
        )
        .await
        .expect("register build N+1 on the double");
    let next_core = deployed(
        next.lash_backend(),
        next.explicit_reconcile_session_work(),
        vec![revisioned(2, &ran)],
        &model,
    );
    assert_eq!(next_core.build_generation(), &next_generation);
    assert!(
        double
            .lash_backend()
            .generation_drain()
            .mark_draining(&old_generation, 1)
            .await
            .expect("mark build N draining"),
        "the law's mark is N's first"
    );
    model.release.notify_one();

    let outcome = tokio::time::timeout(WEDGE, port.await_shift(&session_id, &request))
        .await
        .expect("the shift chain ends")
        .expect("the shift is not refused");
    assert_eq!(outcome.stop, ShiftStop::Idle, "{outcome:?}");
    assert_eq!(outcome.ran.len(), RUNS, "{outcome:?}");

    // The run in flight at the roll finished under N's plugin; every run
    // after it ran under N+1's.
    assert_eq!(
        *ran.lock_recover(),
        [1, 2, 2],
        "the in-flight run ended on the old build and the rest on the new"
    );

    // N's leg is pinned to N's deployment; every other shift of the session
    // ran on N+1's.
    let shifts = lash_restate::RestateAdminClient::new(double.connection())
        .query_json::<ShiftRow>(&format!(
            "SELECT idempotency_key, pinned_deployment_id FROM sys_invocation \
             WHERE target_service_key = '{session_id}' AND target_handler_name = 'shift'"
        ))
        .await
        .expect("sys_invocation query");
    let (first, rest): (Vec<_>, Vec<_>) = shifts
        .iter()
        .partition(|row| row.idempotency_key.as_deref() == Some(request.as_str()));
    assert_eq!(first.len(), 1, "the ask's own leg: {shifts:?}");
    assert!(
        !rest.is_empty(),
        "the shift went on past build N: {shifts:?}"
    );
    let old_pin = first[0].pinned_deployment_id.clone();
    let next_pin = rest[0].pinned_deployment_id.clone();
    assert!(old_pin.is_some() && next_pin.is_some(), "{shifts:?}");
    assert_ne!(old_pin, next_pin, "the rest ran on another build");
    assert!(
        rest.iter().all(|row| row.pinned_deployment_id == next_pin),
        "every leg after N's runs on the newest build: {shifts:?}"
    );

    assert!(
        handle.durable().pending_turn_inputs().await?.is_empty(),
        "the shift drained the session"
    );
    assert_eq!(
        handle.durable().turn_input_applications().await?.len(),
        RUNS,
        "every input was applied once"
    );
    // Each Run's admission recorded the composition it was admitted under
    // (FIG-4747): the Run in flight at the roll records N's revision of the
    // plugin, and the Runs admitted after the bump record N+1's. The store
    // answers a run's recorded admission to any later admission of it.
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        store.store(),
        &session_id,
        "plugin-generation",
    )
    .await;
    let mut recorded = Vec::new();
    for ran in &outcome.ran {
        let admission = recorded_admission(&store, &fence, ran.run()).await;
        let composition: Vec<&str> = admission
            .plugins
            .plugins()
            .iter()
            .map(|admitted| admitted.plugin.as_str())
            .collect();
        assert_eq!(
            composition.last(),
            Some(&PLUGIN),
            "the record is the whole composition in hook order: {composition:?}"
        );
        let admitted = admission
            .plugins
            .plugins()
            .last()
            .expect("the admission names the law's plugin");
        assert_eq!(admitted.writer, FormatVersion::ONE);
        recorded.push(admitted.behavior_revision.get());
    }
    assert_eq!(
        recorded,
        [1, 2, 2],
        "a Run admitted after the bump adopts the new composition, and its record shows it"
    );
    drop(next_core);
    drop(old_core);
    Ok(())
}

/// `run`'s recorded admission, read back as a later admission of it under
/// `fence`. The read presents the executor the run records: the store
/// refuses any other engine-held executor before it reads (FIG-4765).
async fn recorded_admission(
    store: &lash_core::store::SessionStore,
    fence: &lash_core::store::ShiftFence,
    run: &lash_core::TurnId,
) -> lash_core::store::RunAdmission {
    let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
        fence,
        run,
        lash_core::store::AdmittedHead::Input(lash_core::InputId::from("recorded")),
    );
    request.executor =
        lash_core::store::RunStore::run_executor(store.store().as_ref(), store.session_id(), run)
            .await
            .expect("read the Run's executor")
            .expect("the Run's executor is recorded");
    store
        .admit_run(&request)
        .await
        .expect("read the Run's admission back")
        .expect("the Run's admission is recorded")
}

macro_rules! in_flight_laws {
    ($($(#[$attr:meta])* $name:ident: $storage:expr;)*) => {
        $(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                in_flight_segments_finish_on_the_old_build($storage).await
            }
        )*
    };
}

in_flight_laws! {
    in_flight_segments_finish_on_the_old_build_sqlite_memory: Storage::SqliteMemory;
    in_flight_segments_finish_on_the_old_build_sqlite_file: Storage::SqliteFile;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    in_flight_segments_finish_on_the_old_build_postgres: Storage::Postgres;
}

/// The plugin that records its own id after each turn, in hook order, into
/// `ran`.
fn ordered(
    id: &'static str,
    ran: &Arc<std::sync::Mutex<Vec<&'static str>>>,
) -> Arc<dyn PluginFactory> {
    let ran = Arc::clone(ran);
    Arc::new(StaticPluginFactory::new(
        PluginDeclaration::initial(id),
        lash_core::plugin::PluginSpec::new().with_after_turn(
            crate::hook_key!("after-turn-2"),
            Arc::new(move |_| {
                let ran = Arc::clone(&ran);
                Box::pin(async move {
                    ran.lock_recover().push(id);
                    Ok(Default::default())
                })
            }),
        ),
    ))
}

/// L3 (FIG-4859): the plugin installation order changed after a session's
/// creation does not rewrite its in-flight or recorded executions. The Run
/// in flight at the roll redrives under the composition its admission
/// recorded — `[order-a, order-b]` — while work admitted under the successor
/// runs the new `[order-b, order-a]` order. The store answers each Run's
/// recorded admission, in the order it was admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_work_keeps_its_recorded_hook_order_across_an_order_change() -> Result<()> {
    let storage = Storage::SqliteMemory;
    let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
    let old_generation = generation_of(vec![
        ordered("order-a", &Arc::default()),
        ordered("order-b", &Arc::default()),
    ])
    .await;
    let next_generation = generation_of(vec![
        ordered("order-b", &Arc::default()),
        ordered("order-a", &Arc::default()),
    ])
    .await;
    assert_ne!(
        old_generation, next_generation,
        "installation order is part of the generation"
    );

    let (opening, _keep) = prepare(storage).await;
    let double = lash_restate_test::backend_with_store_set(
        SEED,
        lash_restate_test::ServerConfig {
            build_generation: old_generation.clone(),
            ..lash_restate_test::ServerConfig::default()
        },
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            open(opening, clock, DrainLever::default())
                .await
                .map_err(lash_restate_test::BackendError::Stores)
        },
    )
    .await
    .expect("the Restate double over the law's stores");
    let model = Arc::new(Model::holding(1));
    let old_core = deployed(
        double.lash_backend(),
        double.explicit_reconcile_session_work(),
        vec![ordered("order-a", &ran), ordered("order-b", &ran)],
        &model,
    );
    assert_eq!(old_core.build_generation(), &old_generation);

    let session = "plugin-order";
    let _handle = old_core
        .session(crate::SessionId::parse(session).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let session_id = lash_core::SessionId::from(session);
    let store = lash_core::runtime::live_session_view(&old_core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    for index in 0..RUNS {
        store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text(format!("question {index}")),
            ))
            .await
            .expect("enqueue the input");
    }

    // The shift starts on N, the only build, and its first run reaches its
    // model call.
    let port = old_core.substrate_slot.ports().await.queued;
    let request = ShiftRequestId::new("plugin-order");
    port.schedule_shift(&session_id, request.clone());
    tokio::time::timeout(WEDGE, model.reached.notified())
        .await
        .expect("the shift's first run reaches its model call");
    assert!(
        ran.lock_recover().is_empty(),
        "no turn has ended while the first run is in its model call"
    );

    // The roll: N+1 registers with the plugins installed in the opposite
    // order — a different composition, so a generation and deployment of
    // its own — and the operator marks N draining.
    let next = double
        .add_separate_build(
            next_generation.clone(),
            "next",
            lash_restate_test::DeploymentHooks::default(),
        )
        .await
        .expect("register build N+1 on the double");
    let next_core = deployed(
        next.lash_backend(),
        next.explicit_reconcile_session_work(),
        vec![ordered("order-b", &ran), ordered("order-a", &ran)],
        &model,
    );
    assert_eq!(next_core.build_generation(), &next_generation);
    assert!(
        double
            .lash_backend()
            .generation_drain()
            .mark_draining(&old_generation, 1)
            .await
            .expect("mark build N draining"),
        "the law's mark is N's first"
    );
    model.release.notify_one();

    let outcome = tokio::time::timeout(WEDGE, port.await_shift(&session_id, &request))
        .await
        .expect("the shift chain ends")
        .expect("the shift is not refused");
    assert_eq!(outcome.stop, ShiftStop::Idle, "{outcome:?}");
    assert_eq!(outcome.ran.len(), RUNS, "{outcome:?}");

    // The run in flight at the roll ran its hooks in creation order; every
    // run after it ran them in the successor's order.
    assert_eq!(
        *ran.lock_recover(),
        [
            "order-a", "order-b", "order-b", "order-a", "order-b", "order-a"
        ],
        "hook order follows each run's admitted composition"
    );

    // Each Run's recorded admission names the composition in the order it
    // was admitted under, and the store answers it to a later admission.
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        store.store(),
        &session_id,
        "plugin-order",
    )
    .await;
    let mut recorded = Vec::new();
    for ran in &outcome.ran {
        let admission = recorded_admission(&store, &fence, ran.run()).await;
        let tail: Vec<String> = admission
            .plugins
            .plugins()
            .iter()
            .map(|admitted| admitted.plugin.as_str().to_owned())
            .collect();
        recorded.push((tail[tail.len() - 2].clone(), tail[tail.len() - 1].clone()));
    }
    assert_eq!(
        recorded,
        [
            ("order-a".to_owned(), "order-b".to_owned()),
            ("order-b".to_owned(), "order-a".to_owned()),
            ("order-b".to_owned(), "order-a".to_owned())
        ],
        "the recorded participating set keeps its admitted hook order"
    );
    drop(next_core);
    drop(old_core);
    Ok(())
}

/// The namespace of the config law's owner: how much its reducers added.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct Tally {
    total: u32,
}

/// The owner of [`PLUGIN`]'s namespace. It refuses nothing.
struct TallyOwner;

impl lash_core::ConfigOwner for TallyOwner {
    type Create = Tally;
    type Recorded = Tally;
    type Refusal = String;
    type RunOptions = lash_core::NoRunOptions;

    fn create(
        &self,
        input: Option<Tally>,
        _facts: lash_core::CreationFacts<'_, Tally>,
    ) -> std::result::Result<Option<Tally>, String> {
        Ok(Some(input.unwrap_or(Tally { total: 0 })))
    }

    fn validate(
        &self,
        _value: &Tally,
        _base: Option<&Tally>,
        _facts: &lash_core::CandidateFacts<'_>,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &Tally,
        _options: lash_core::NoRunOptions,
    ) -> std::result::Result<Tally, String> {
        Ok(recorded.clone())
    }
}

/// Add to the tally. What it adds is the reducer's: the revision of the
/// build that resolves it, which is also its output.
#[derive(
    Clone, Debug, serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct Count {}

impl lash_core::ConfigCommand for Count {
    type Owner = TallyOwner;
    type Output = u32;
    const NAME: &'static str = "count";
}

/// [`PLUGIN`] at behaviour `revision`, owning a config namespace whose
/// reducers are that revision's.
struct Tallying {
    revision: u32,
}

impl lash_core::facade_support::PluginFactory for Tallying {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        named(PLUGIN).build(ctx)
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> std::result::Result<(), lash_core::ConfigRegistrationError> {
        let revision = self.revision;
        registrar.owner(TallyOwner)?;
        registrar.command::<Count>(move |recorded, Count {}| {
            Ok(lash_core::OwnerChange {
                recorded: Tally {
                    total: recorded.total + revision,
                },
                output: revision,
            })
        })
    }
}

impl lash_core::plugin::PluginMetadata for Tallying {
    fn plugin_declaration(&self) -> PluginDeclaration {
        let mut declaration = PluginDeclaration::initial(PLUGIN);
        declaration.behavior_revision =
            BehaviorRevision::new(self.revision).expect("a revision counts from one");
        declaration
    }
}

fn tallying(revision: u32) -> Arc<dyn PluginFactory> {
    Arc::new(Tallying { revision })
}

/// FIG-4791: builds N and N+1 differ only in a config owner's reducers,
/// which is a bump of its plugin's behaviour revision and so another `G`.
/// A transaction whose command run N admits resolves on N under N's
/// reducers. One submitted through N and still queued when N+1 registers and
/// N drains is admitted by N+1, resolves there under N+1's reducers and
/// settles: no run of the session parks.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_config_transaction_resolves_on_the_lane_that_admits_it() -> Result<()> {
    let old_generation = generation_of(vec![tallying(1)]).await;
    let next_generation = generation_of(vec![tallying(2)]).await;
    assert_ne!(
        old_generation, next_generation,
        "an owner's reducers are its plugin's behaviour, which names the lane"
    );

    let (opening, _keep) = prepare(Storage::SqliteMemory).await;
    let double = lash_restate_test::backend_with_store_set(
        SEED,
        lash_restate_test::ServerConfig {
            build_generation: old_generation.clone(),
            ..lash_restate_test::ServerConfig::default()
        },
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            open(opening, clock, DrainLever::default())
                .await
                .map_err(lash_restate_test::BackendError::Stores)
        },
    )
    .await
    .expect("the Restate double over the law's stores");
    let model = Arc::new(Model::holding(0));
    let old_core = deployed(
        double.lash_backend(),
        double.explicit_reconcile_session_work(),
        vec![tallying(1)],
        &model,
    );
    assert_eq!(old_core.build_generation(), &old_generation);

    let session = "config-owner-generation";
    let handle = old_core
        .session(crate::SessionId::parse(session).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let session_id = lash_core::SessionId::from(session);
    let store = lash_core::runtime::live_session_view(&old_core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    let port = old_core.substrate_slot.ports().await.queued;
    let config = handle.admin().config();

    // Submit one `Count` through build N's ingress, shift the session once
    // from N, and read how the transaction settled.
    let count = async |id: &str| -> Result<crate::config::ConfigTransactionOutcome> {
        let revision = config.revision().await?;
        let crate::config::ConfigSettlement::Pending(receipt) = config
            .submit(
                crate::config::ConfigWrite::new(id, revision),
                crate::config::ConfigTransaction::of(Count {}),
            )
            .await?
        else {
            panic!("a store-backed session queues its transaction");
        };
        let request = ShiftRequestId::new(id);
        port.schedule_shift(&session_id, request.clone());
        let outcome = tokio::time::timeout(WEDGE, port.await_shift(&session_id, &request))
            .await
            .expect("the shift ends")
            .expect("the shift is not refused");
        assert_eq!(outcome.stop, ShiftStop::Idle, "{id}: {outcome:?}");
        assert_eq!(
            store.load_turn_park().await.expect("read the park"),
            None,
            "{id}: the command run never parks"
        );
        match config.settle(receipt).await? {
            crate::config::ConfigSettlement::Settled(outcome) => Ok(outcome),
            unsettled => panic!("{id}: the shift settles the transaction: {unsettled:?}"),
        }
    };

    // N is the only build: it admits the transaction's run and its
    // reducers resolve it.
    assert_eq!(
        count("on-n").await?,
        crate::config::ConfigTransactionOutcome::Applied {
            base_revision: 0,
            revision: 1,
            outputs: vec![serde_json::json!(1)],
        }
    );

    // The roll: N+1 registers with the owner's reducers changed, and N is
    // marked draining.
    let next = double
        .add_separate_build(
            next_generation.clone(),
            "next",
            lash_restate_test::DeploymentHooks::default(),
        )
        .await
        .expect("register build N+1 on the double");
    let next_core = deployed(
        next.lash_backend(),
        next.explicit_reconcile_session_work(),
        vec![tallying(2)],
        &model,
    );
    assert_eq!(next_core.build_generation(), &next_generation);
    assert!(
        double
            .lash_backend()
            .generation_drain()
            .mark_draining(&old_generation, 1)
            .await
            .expect("mark build N draining"),
        "the law's mark is N's first"
    );

    // N's ingress still takes the submission. N+1's lane admits its run, so
    // N+1's reducers resolve it, and it settles.
    assert_eq!(
        count("after-the-roll").await?,
        crate::config::ConfigTransactionOutcome::Applied {
            base_revision: 1,
            revision: 2,
            outputs: vec![serde_json::json!(2)],
        }
    );
    drop(next_core);
    drop(old_core);
    Ok(())
}
