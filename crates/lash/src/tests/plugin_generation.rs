//! FIG-4744 lane routing: the build generation `G` folds in the core's
//! plugins in hook order, and exists only once they are registered.
//!
//! - A plugin-only behaviour revision bump yields a new `G`, and so does the
//!   same plugins in another hook order; the same registration yields the
//!   same `G`.
//! - An engine has no `G` until a core registers its plugins over it, and
//!   serves one composition: a core with another is refused, typed.
//! - In-flight segments finish on the old build. A session's drive starts on
//!   build N; while its first root is in its model call, build N+1 registers
//!   with the plugin's revision bumped, as a deployment of its own with its
//!   own plugins, and N is marked draining. The root in flight finishes on N
//!   under N's plugin; every root after it runs on N+1 under N+1's.
//!
//! The generation laws build real engines whose generation the core binds.
//! The in-flight law runs on the Restate server double over SQLite memory,
//! SQLite file and PostgreSQL, each build stamped with the generation its
//! composition computes; the PostgreSQL leg is ignored in ordinary runs and
//! requires `LASH_POSTGRES_DATABASE_URL`.

use super::drain_hand_over::{DrainLever, Model, Storage, open, prepare, provider};
use super::*;

use lash_core::engine::{BuildGeneration, DriveRequestId, DriveStop, GenerationUnbound};
use lash_core::plugin::{
    BehaviorRevision, FormatVersion, PluginDeclaration, PluginDeclarationError, PluginId,
};

const SEED: u64 = 0x4744_91a3;

/// The plugin whose revision the laws move.
const PLUGIN: &str = "revisioned";

/// The inputs the in-flight law's session holds: one root each.
const ROOTS: usize = 3;

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
        lash_core::plugin::PluginSpec::new().with_after_turn(Arc::new(move |_| {
            let ran = Arc::clone(&ran);
            Box::pin(async move {
                ran.lock_recover().push(revision);
                Ok(Vec::new())
            })
        })),
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
        .serve_test_model(provider, mock_model_spec());
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

    fn declaration(&self) -> PluginDeclaration {
        self.declared.clone()
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
                factory: PluginId::new("registered"),
                declared: PluginId::new("another"),
            },
        ),
        (
            Declares {
                id: "stateful",
                declared: unwritable,
            },
            PluginDeclarationError::NativeFormatNotWritable {
                plugin: PluginId::new("stateful"),
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

/// One drive invocation of the law's session, as the engine's
/// `sys_invocation` reports it.
#[derive(Debug, serde::Deserialize)]
struct DriveRow {
    idempotency_key: Option<String>,
    pinned_deployment_id: Option<String>,
}

/// A core of `plugins` over `backend` whose driver starts no wall-clock
/// reconcile tick: the law's ask is the session's only drive.
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
    let handle = old_core.session(session).created().await.open().await?;
    let session_id = lash_core::SessionId::from(session);
    let store = lash_core::runtime::live_session_view(&old_core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    for index in 0..ROOTS {
        store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text(format!("question {index}")),
            ))
            .await
            .expect("enqueue the input");
    }

    // The drive starts on N, the only build, and its first root reaches its
    // model call.
    let port = old_core.substrate_slot.ports().await.queued;
    let request = DriveRequestId::new("plugin-generation");
    port.schedule_drive(&session_id, request.clone());
    tokio::time::timeout(WEDGE, model.reached.notified())
        .await
        .expect("the drive's first root reaches its model call");
    assert!(
        ran.lock_recover().is_empty(),
        "no turn has ended while the first root is in its model call"
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

    let outcome = tokio::time::timeout(WEDGE, port.await_drive(&session_id, &request))
        .await
        .expect("the drive chain ends")
        .expect("the drive is not refused");
    assert_eq!(outcome.stop, DriveStop::Idle, "{outcome:?}");
    assert_eq!(outcome.ran.len(), ROOTS, "{outcome:?}");

    // The root in flight at the roll finished under N's plugin; every root
    // after it ran under N+1's.
    assert_eq!(
        *ran.lock_recover(),
        [1, 2, 2],
        "the in-flight root ended on the old build and the rest on the new"
    );

    // N's leg is pinned to N's deployment; every other drive of the session
    // ran on N+1's.
    let drives = lash_restate::RestateAdminClient::new(double.connection())
        .query_json::<DriveRow>(&format!(
            "SELECT idempotency_key, pinned_deployment_id FROM sys_invocation \
             WHERE target_service_key = '{session_id}' AND target_handler_name = 'drive'"
        ))
        .await
        .expect("sys_invocation query");
    let (first, rest): (Vec<_>, Vec<_>) = drives
        .iter()
        .partition(|row| row.idempotency_key.as_deref() == Some(request.as_str()));
    assert_eq!(first.len(), 1, "the ask's own leg: {drives:?}");
    assert!(
        !rest.is_empty(),
        "the drive went on past build N: {drives:?}"
    );
    let old_pin = first[0].pinned_deployment_id.clone();
    let next_pin = rest[0].pinned_deployment_id.clone();
    assert!(old_pin.is_some() && next_pin.is_some(), "{drives:?}");
    assert_ne!(old_pin, next_pin, "the rest ran on another build");
    assert!(
        rest.iter().all(|row| row.pinned_deployment_id == next_pin),
        "every leg after N's runs on the newest build: {drives:?}"
    );

    assert!(
        handle.durable().pending_turn_inputs().await?.is_empty(),
        "the drive drained the session"
    );
    assert_eq!(
        handle.durable().turn_input_applications().await?.len(),
        ROOTS,
        "every input was applied once"
    );
    drop(next_core);
    drop(old_core);
    Ok(())
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
