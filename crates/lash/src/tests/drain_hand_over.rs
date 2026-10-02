//! FIG-4639: a session drive on a draining build hands over to the newest
//! build after its current root (ADR 0106 §1): a rolling deploy's drain is
//! bounded by one root per drive, never by the backlog.
//!
//! Each law opens a session with a backlog of inputs, each its own root, and
//! starts one drive of it on build N. While that drive's first root is in
//! its model call, build N+1 registers and an operator marks N draining.
//!
//! - **hand-over**: the drive on N ends after the root it was running and
//!   admits no other; N+1 runs every remaining root. Every input is applied
//!   exactly once.
//! - **replay**: the same, with N dying after it journaled the admission
//!   that read the drain mark and before it sent the rest of the drive on.
//!   The mark is removed before anything reads it again, so a replay that
//!   decided again would admit the next root on N. It hands over as its
//!   first execution did: the decision is the journaled admission's.
//!
//! - **stamp** (FIG-4742): a root is stamped with the generation of the build
//!   that runs it, which is the build whose drive admitted it unless a newer
//!   build registered in between and the stable name handed the root to it.
//!   While N's root is in flight N counts it; once N+1 has admitted the next
//!   root from the hand-over, N counts none and its drain is complete, and a
//!   park of that root names N+1. These run on the double only.
//!
//! Both run on the Restate server double over SQLite memory, SQLite file
//! and PostgreSQL, and on a live `restate-server` over SQLite memory and
//! PostgreSQL. The PostgreSQL legs are ignored in ordinary runs and require
//! `LASH_POSTGRES_DATABASE_URL`. On the double, both builds' endpoints serve
//! one core, as its sibling builds share a session driver; on the live
//! server each build is its own engine and core over the shared stores, as
//! two processes of a roll are. The `drain-hand-over` suites of
//! `scripts/restate-suites.toml` run the live laws on their live and replay
//! legs. Under replay every await suspends, so a drive runs in legs of one
//! root and N's leg hands off at its root's boundary before it admits
//! again: the same bound, by the leg rule (ADR 0105 §7).

use super::*;

use lash_core::engine::{BuildGeneration, DriveOutcome, DriveRequest, DriveRequestId, DriveStop};
use lash_core::store::generation_drain::{
    DrainingGeneration, GenerationDrainStore, GenerationWork,
};

const SEED: u64 = 0x4639_d7a1;

/// The inputs the session holds when its drive starts: one root each.
const ROOTS: usize = 4;

/// The name of the session driver's service.
const SESSION_DRIVER_SERVICE: &str = "LashSession";

#[derive(Clone, Copy, Debug)]
pub(super) enum Storage {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

/// The stores' drain marks, with the replay law's lever: once `armed`, the
/// next read of the marks first removes `generation`'s, as an operator
/// ending the drain would.
struct ClearingDrain {
    inner: Arc<dyn GenerationDrainStore>,
    lever: DrainLever,
}

#[derive(Clone, Default)]
pub(super) struct DrainLever {
    armed: Arc<std::sync::atomic::AtomicBool>,
    generation: Arc<std::sync::OnceLock<BuildGeneration>>,
}

#[async_trait::async_trait]
impl GenerationDrainStore for ClearingDrain {
    async fn mark_draining(
        &self,
        generation: &BuildGeneration,
        now_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        self.inner.mark_draining(generation, now_ms).await
    }

    async fn clear_draining(
        &self,
        generation: &BuildGeneration,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        self.inner.clear_draining(generation).await
    }

    async fn draining_generations(
        &self,
    ) -> std::result::Result<Vec<DrainingGeneration>, lash_core::StoreError> {
        if self.lever.armed.swap(false, Ordering::SeqCst)
            && let Some(generation) = self.lever.generation.get()
        {
            self.inner.clear_draining(generation).await?;
        }
        self.inner.draining_generations().await
    }

    async fn generation_work(
        &self,
        generation: &BuildGeneration,
    ) -> std::result::Result<GenerationWork, lash_core::StoreError> {
        self.inner.generation_work(generation).await
    }

    async fn live_processes(
        &self,
        generation: &BuildGeneration,
        after: Option<&lash_core::ProcessId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash_core::ProcessId>, lash_core::StoreError> {
        self.inner.live_processes(generation, after, limit).await
    }
}

/// What a law's stores need to outlive them.
#[derive(Default)]
pub(super) struct Keep {
    _files: Option<tempfile::TempDir>,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

/// A store set a law is about to open on its engine's clock.
pub(super) enum Opening {
    SqliteMemory,
    SqliteFile(std::path::PathBuf),
    Postgres(lash_postgres_store::PostgresStorage, std::path::PathBuf),
}

/// The PostgreSQL URL required by the PostgreSQL legs.
#[allow(
    clippy::disallowed_methods,
    reason = "the PostgreSQL legs read the required service URL"
)]
fn postgres_url() -> String {
    lash_postgres_store::testing::required_database_url()
}

pub(super) async fn prepare(storage: Storage) -> (Opening, Keep) {
    match storage {
        Storage::SqliteMemory => (Opening::SqliteMemory, Keep::default()),
        Storage::SqliteFile => {
            let files = tempfile::tempdir().expect("a SQLite store directory");
            let root = files.path().to_owned();
            (
                Opening::SqliteFile(root),
                Keep {
                    _files: Some(files),
                    _database: None,
                },
            )
        }
        Storage::Postgres => {
            let database =
                lash_postgres_store::testing::IsolatedDatabase::create(&postgres_url()).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open the provisioned PostgreSQL storage");
            let files = tempfile::tempdir().expect("an attachment directory");
            let attachments = files.path().to_owned();
            (
                Opening::Postgres(storage, attachments),
                Keep {
                    _files: Some(files),
                    _database: Some(database),
                },
            )
        }
    }
}

/// Open `opening` on `clock`, with its drain marks behind `lever`.
pub(super) async fn open(
    opening: Opening,
    clock: Arc<dyn lash_core::Clock>,
    lever: DrainLever,
) -> std::result::Result<Arc<dyn lash_core::StoreSet>, String> {
    let stores: Arc<dyn lash_core::StoreSet> = match opening {
        Opening::SqliteMemory => Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                .await
                .map_err(|error| error.to_string())?,
        ),
        Opening::SqliteFile(root) => Arc::new(
            lash_sqlite_store::SqliteStoreSet::open_with_clock(root, clock)
                .await
                .map_err(|error| error.to_string())?,
        ),
        Opening::Postgres(storage, attachments) => {
            Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                    attachments,
                )),
                lash_core::WakeDeliveryConfig::default(),
                clock,
            ))
        }
    };
    Ok(
        lash_core::testing::runtime_helpers::LayeredStores::over(stores)
            .map_generation_drain(|inner| Arc::new(ClearingDrain { inner, lever }))
            .into_store_set(),
    )
}

/// The provider both builds' cores answer with: it echoes the last user
/// text, records every question it was asked, and holds each of its first
/// `held` calls until the law releases it.
pub(super) struct Model {
    held: usize,
    pub(super) asked: std::sync::Mutex<Vec<String>>,
    pub(super) reached: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

impl Model {
    pub(super) fn holding(held: usize) -> Self {
        Self {
            held,
            asked: std::sync::Mutex::default(),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

pub(super) fn provider(model: &Arc<Model>) -> ProviderHandle {
    let model = Arc::clone(model);
    crate::testing::TestProvider::builder()
        .kind("drain-hand-over")
        .complete(move |request| {
            let model = Arc::clone(&model);
            async move {
                let question = last_user_text(&request);
                let call = {
                    let mut asked = model.asked.lock_recover();
                    asked.push(question.clone());
                    asked.len()
                };
                if call <= model.held {
                    model.reached.notify_one();
                    model.release.notified().await;
                }
                Ok(text_response(&format!("echo: {question}")))
            }
        })
        .build()
        .into_handle()
}

/// A core over `backend` whose driver starts no wall-clock reconcile tick:
/// the law's ask is the session's only drive.
fn core_over(
    backend: lash_core::Backend,
    work: Arc<dyn lash_core::SessionWorkEngine>,
    model: &Arc<Model>,
) -> LashCore {
    core_with_protocol(backend, work, model, None)
}

/// [`core_over`], with `protocol` as the sessions' protocol plugin.
fn core_with_protocol(
    backend: lash_core::Backend,
    work: Arc<dyn lash_core::SessionWorkEngine>,
    model: &Arc<Model>,
    protocol: Option<Arc<dyn lash_core::plugin::ProtocolSessionPlugin>>,
) -> LashCore {
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
        .with_session_work(work)
        .into_backend();
    let builder = LashCore::standard_builder(backend);
    let builder = match protocol {
        Some(protocol) => builder.protocol_plugin(
            lash_core::testing::test_standard_protocol_factory_with_runtime_state(protocol, None),
        ),
        None => builder,
    };
    builder
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(provider(model), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core")
}

type Live = lash_restate_test::live::LiveRestateBackend<dyn lash_core::StoreSet>;

/// The engine a law's builds run on.
enum Engine {
    Double(lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>),
    Live {
        old: Live,
        next: std::sync::Mutex<Option<Live>>,
    },
}

/// One drive invocation of the law's session, as the engine's
/// `sys_invocation` reports it.
#[derive(Debug, serde::Deserialize)]
struct DriveRow {
    idempotency_key: Option<String>,
    pinned_deployment_id: Option<String>,
}

#[allow(
    clippy::disallowed_methods,
    reason = "the live laws read the suite's server and endpoint addresses"
)]
fn suite_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment sets {name}"))
}

impl Engine {
    fn old_backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(double) => double.lash_backend(),
            Self::Live { old, .. } => old.lash_backend(),
        }
    }

    fn old_work(&self) -> Arc<dyn lash_core::SessionWorkEngine> {
        match self {
            Self::Double(double) => double.explicit_reconcile_session_work(),
            Self::Live { old, .. } => old.explicit_reconcile_session_work(),
        }
    }

    /// Bring build N+1 up beside N. On the double its endpoint serves the
    /// law's one core; on a live server it is an engine of its own, and the
    /// core returned serves it.
    async fn roll(&self, next: BuildGeneration, model: &Arc<Model>) -> Option<LashCore> {
        match self {
            Self::Double(double) => {
                double
                    .add_build(next, "next", lash_restate_test::DeploymentHooks::default())
                    .await
                    .expect("register build N+1 on the double");
                None
            }
            Self::Live { old, next: slot } => {
                let build = old
                    .add_build(
                        suite_env("DH_B_BIND").parse().expect("a socket address"),
                        suite_env("DH_B_URL"),
                        next,
                    )
                    .await
                    .expect("serve and register build N+1");
                let core = core_over(
                    build.lash_backend(),
                    build.explicit_reconcile_session_work(),
                    model,
                );
                *slot.lock_recover() = Some(build);
                Some(core)
            }
        }
    }

    /// N dies before it sends the rest of `session`'s drive on, once.
    /// `crashed` runs as it dies.
    fn crash_before_the_hand_over(
        &self,
        session: &lash_core::SessionId,
        crashed: lash_restate_test::CrashListener,
    ) {
        let rule = lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
            ty: lash_restate_test::protocol::MessageType::OneWayCallCommand,
        })
        .service(SESSION_DRIVER_SERVICE)
        .handler("drive")
        .key(session.as_str());
        let listening = match self {
            Self::Double(double) => {
                double.server().crash_on(rule);
                double.server().on_crash(crashed)
            }
            Self::Live { old, .. } => {
                old.crash_on(rule);
                old.on_crash(crashed)
            }
        };
        assert!(
            listening,
            "the law's crash listener is the engine's only one"
        );
    }

    /// Serve the replay: the double replays a crashed attempt by itself; a
    /// live deployment that died at its crash listens again, once its dying
    /// listener has let go of the endpoint's address.
    async fn serve_the_replay(&self) {
        if let Self::Live { old, .. } = self {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            old.start_serving()
                .await
                .expect("build N's deployment comes back");
        }
    }

    /// One leg of `session`'s drive: the outcome of the invocation
    /// `request` names, without following its continuation.
    async fn leg(&self, session: &lash_core::SessionId, request: &DriveRequestId) -> DriveOutcome {
        let attached = match self {
            Self::Double(double) => {
                double
                    .restate()
                    .session_work_engine()
                    .attach_drive(session, request.clone())
                    .await
            }
            Self::Live { old, .. } => old.attach_drive(session, request.clone()).await,
        };
        attached.unwrap_or_else(|error| panic!("attach to leg `{}`: {error}", request.as_str()))
    }

    /// Every drive invocation of `session`.
    async fn drives(&self, session: &lash_core::SessionId) -> Vec<DriveRow> {
        let admin = match self {
            Self::Double(double) => lash_restate::RestateAdminClient::new(double.connection()),
            Self::Live { .. } => lash_restate::RestateAdminClient::new(
                lash_restate::RestateConnection::new(suite_env("RESTATE_ADMIN_URL")),
            ),
        };
        admin
            .query_json::<DriveRow>(&format!(
                "SELECT idempotency_key, pinned_deployment_id FROM sys_invocation \
                 WHERE target_service_key = '{session}' AND target_handler_name = 'drive'"
            ))
            .await
            .expect("sys_invocation query")
    }

    async fn finish(&self) {
        if let Self::Live { old, next } = self {
            let next = next.lock_recover().take();
            if let Some(next) = next {
                next.finish().await;
            }
            old.finish().await;
        }
    }
}

/// A law's world: its engine with build N serving, and what its stores need
/// to outlive it.
struct World {
    engine: Engine,
    lever: DrainLever,
    /// Whether every await suspends and replays: the double's always-replay,
    /// or the live suite's replay leg.
    always_replay: bool,
    _keep: Keep,
}

async fn double_world(storage: Storage) -> World {
    let (opening, keep) = prepare(storage).await;
    let lever = DrainLever::default();
    let opened = lever.clone();
    let double = lash_restate_test::backend_with_store_set(
        SEED,
        lash_restate_test::ServerConfig::default(),
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            open(opening, clock, opened)
                .await
                .map_err(lash_restate_test::BackendError::Stores)
        },
    )
    .await
    .expect("the Restate double over the law's stores");
    World {
        engine: Engine::Double(double),
        lever,
        always_replay: false,
        _keep: keep,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the live laws read which leg of the suite runs them"
)]
async fn live_world(storage: Storage) -> World {
    let (opening, keep) = prepare(storage).await;
    let lever = DrainLever::default();
    let opened = lever.clone();
    let old = lash_restate_test::live::LiveRestateBackend::start_with_store_set(
        lash_restate_test::live::LiveConfig {
            ingress_url: suite_env("RESTATE_INGRESS_URL"),
            admin_url: suite_env("RESTATE_ADMIN_URL"),
            endpoint_bind: suite_env("DH_A_BIND").parse().expect("a socket address"),
            endpoint_url: suite_env("DH_A_URL"),
            // One authority for every run on the suite's server: a run
            // registers build N over the names the run before it left
            // claimed at build N+1's address, which only their own
            // authority may take.
            run_tag: "drain-hand-over".to_owned(),
            namespace: lash_restate::RestateNamespace::default(),
        },
        |clock| async move {
            open(opening, clock, opened)
                .await
                .map_err(lash_restate_test::live::LiveError::Stores)
        },
    )
    .await
    .expect("serve build N on the live server");
    World {
        engine: Engine::Live {
            old,
            next: std::sync::Mutex::default(),
        },
        lever,
        always_replay: std::env::var("LASH_RESTATE_SUITE_LEG").is_ok_and(|leg| leg == "replay"),
        _keep: keep,
    }
}

/// How long a law waits on a step that only a wedge delays.
const WEDGE: std::time::Duration = std::time::Duration::from_secs(120);

/// The law. With `crash`, N dies between journaling the admission that read
/// the drain mark and sending the rest of the drive on, and the mark is
/// removed before anything reads it again.
async fn a_drive_on_a_draining_build_hands_over_after_its_current_root(
    world: World,
    session: &str,
    crash: bool,
) -> Result<()> {
    let World {
        engine,
        lever,
        always_replay,
        _keep,
    } = world;
    let model = Arc::new(Model::holding(1));
    let old_generation = engine
        .old_backend()
        .build_generation()
        .expect("the engine's generation is bound")
        .clone();
    lever
        .generation
        .set(old_generation.clone())
        .expect("the lever names one generation");
    let old_core = core_over(engine.old_backend(), engine.old_work(), &model);

    let handle = old_core.session(session).created().await.open().await?;
    let session_id = lash_core::SessionId::from(session);
    let store = lash_core::runtime::live_session_view(&old_core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    let questions: Vec<String> = (0..ROOTS)
        .map(|index| format!("question {index}"))
        .collect();
    for question in &questions {
        store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text(question.clone()),
            ))
            .await
            .expect("enqueue the input");
    }

    let mut crashes = lash_restate_test::CrashCount::new();
    if crash {
        let armed = Arc::clone(&lever.armed);
        engine.crash_before_the_hand_over(
            &session_id,
            crashes.listener_with(move |_| {
                // The operator ends the drain while N is down: whatever
                // reads the mark next finds none.
                armed.store(true, Ordering::SeqCst);
            }),
        );
    }

    // The drive starts on N, the only build, and its first root reaches its
    // model call.
    let port = old_core.substrate_slot.ports().await.queued;
    let request = DriveRequestId::new("drain-hand-over");
    port.schedule_drive(&session_id, request.clone());
    tokio::time::timeout(WEDGE, model.reached.notified())
        .await
        .expect("the drive's first root reaches its model call");

    // The roll: N+1 registers, and the operator marks N draining.
    let next_generation = BuildGeneration::for_test("drain-hand-over-next");
    let next_core = engine.roll(next_generation, &model).await;
    assert!(
        engine
            .old_backend()
            .generation_drain()
            .mark_draining(&old_generation, 1)
            .await
            .expect("mark build N draining"),
        "the law's mark is N's first"
    );
    model.release.notify_one();
    if crash {
        tokio::time::timeout(WEDGE, crashes.wait_until(1))
            .await
            .expect("build N dies before it hands the drive over")
            .expect("observe the crash");
        engine.serve_the_replay().await;
    }

    // A waiter on the ask follows the drive across the hand-over to its end.
    let outcome = tokio::time::timeout(WEDGE, port.await_drive(&session_id, &request))
        .await
        .expect("the drive chain ends")
        .expect("the drive is not refused");
    assert_eq!(outcome.stop, DriveStop::Idle, "{outcome:?}");
    assert_eq!(
        outcome.ran.len(),
        ROOTS,
        "the waiter saw every root across the hand-over: {outcome:?}"
    );

    // The legs of the drive: N's, then the ones it was handed on to.
    let mut legs = Vec::new();
    let mut leg = DriveRequest {
        session: session_id.clone(),
        request,
        intended_lane: None,
    };
    loop {
        let ended = engine.leg(&session_id, &leg.request).await;
        let handed_on = matches!(
            ended.stop,
            DriveStop::HandedOff { .. } | DriveStop::Draining { .. }
        );
        legs.push((leg.request.clone(), ended));
        if !handed_on {
            break;
        }
        leg.request = lash_core::engine::drive_continuation_request(&leg);
    }
    let (first_request, first) = &legs[0];
    assert_eq!(
        first.ran.len(),
        1,
        "the drive on the draining build ran the root it was running and admitted no other: \
         {legs:?}"
    );
    if always_replay {
        assert!(
            matches!(first.stop, DriveStop::HandedOff { .. }),
            "a replayed leg hands off at its root's boundary: {first:?}"
        );
    } else {
        assert_eq!(
            first.stop,
            DriveStop::Draining {
                generation: old_generation.clone()
            },
            "the admission after the running root recorded the drain"
        );
    }
    assert!(legs.len() > 1, "the drive went on past build N: {legs:?}");

    // N's leg is pinned to N; every leg after it runs on the other build,
    // the newest, and no other drive of the session ran.
    let drives = engine.drives(&session_id).await;
    let pin = |request: &DriveRequestId| {
        drives
            .iter()
            .find(|row| row.idempotency_key.as_deref() == Some(request.as_str()))
            .and_then(|row| row.pinned_deployment_id.clone())
            .unwrap_or_else(|| panic!("leg `{}` has no pin: {drives:?}", request.as_str()))
    };
    let old_pin = pin(first_request);
    let next_pin = pin(&legs[1].0);
    assert_ne!(
        old_pin, next_pin,
        "the rest of the drive ran on another build"
    );
    for (request, _) in &legs[1..] {
        assert_eq!(
            pin(request),
            next_pin,
            "every leg after N's runs on the newest build: {drives:?}"
        );
    }
    assert_eq!(
        drives.len(),
        legs.len(),
        "the legs are the session's only drives: {drives:?}"
    );

    // Exactly once: every input was asked and applied once, none lost.
    let roots: std::collections::BTreeSet<_> = legs
        .iter()
        .flat_map(|(_, leg)| leg.ran.iter())
        .map(|root| {
            assert!(
                matches!(root, lash_core::engine::RootOutcome::Committed { .. }),
                "every root committed: {legs:?}"
            );
            root.root().clone()
        })
        .collect();
    assert_eq!(roots.len(), ROOTS, "each root ran in one leg: {legs:?}");
    let mut asked = model.asked.lock_recover().clone();
    asked.sort();
    assert_eq!(asked, questions, "every input was asked once");
    assert!(
        handle.durable().pending_turn_inputs().await?.is_empty(),
        "the drive drained the session"
    );
    assert_eq!(
        handle.durable().turn_input_applications().await?.len(),
        ROOTS,
        "every input was applied once"
    );

    if crash {
        assert_eq!(crashes.get(), 1, "build N died once");
        if !always_replay {
            // The newest build's later admissions read the marks, so the
            // operator's removal has run: the replay on N had no mark to
            // read, and handed over on its journaled admission alone.
            assert!(
                !lever.armed.load(Ordering::SeqCst),
                "an admission read the drain marks after N died"
            );
            assert_eq!(
                engine
                    .old_backend()
                    .generation_drain()
                    .draining_generations()
                    .await
                    .expect("read the drain marks"),
                Vec::new(),
                "the drain mark was removed before N replayed past its admission"
            );
        }
    }
    drop(next_core);
    drop(old_core);
    engine.finish().await;
    Ok(())
}

async fn on_the_double(storage: Storage, crash: bool) -> Result<()> {
    a_drive_on_a_draining_build_hands_over_after_its_current_root(
        double_world(storage).await,
        "drain-hand-over",
        crash,
    )
    .await
}

/// The law on a live `restate-server`. Its state outlives a run, so each run
/// names its own session.
async fn on_live_restate(storage: Storage, crash: bool) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after the epoch")
        .as_nanos();
    a_drive_on_a_draining_build_hands_over_after_its_current_root(
        live_world(storage).await,
        &format!("drain-hand-over-{crash}-{nonce}"),
        crash,
    )
    .await
}

/// A protocol whose `before_llm_call` meets a replay refusal whenever the
/// model is about to be asked `question`, as a code cell does when its
/// re-execution diverges from its journal.
struct DivergingAt {
    question: String,
}

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for DivergingAt {
    async fn before_llm_call(
        &self,
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        request: &LlmRequest,
    ) -> std::result::Result<Option<lash_core::ProtocolLlmCallAction>, lash_core::PluginError> {
        if last_user_text(request) != self.question {
            return Ok(None);
        }
        Err(lash_core::PluginError::RuntimeEffectController(
            lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::LashlangCellReplayDivergence,
                "lashlang run diverged from its journal at issue ordinal 0",
            ),
        ))
    }
}

/// A double's session mid-roll: its drive's first root is held in its model
/// call on build N, build N+1 is registered, and N is marked draining.
struct MidRoll {
    engine: Engine,
    core: LashCore,
    model: Arc<Model>,
    session: lash_core::SessionId,
    request: DriveRequestId,
    old: BuildGeneration,
    next: BuildGeneration,
    _keep: Keep,
}

impl MidRoll {
    /// Open `session` with [`ROOTS`] inputs, start its drive on N and roll
    /// while the first root is in its model call. The model holds its first
    /// `held` calls; `protocol` is the session's protocol plugin.
    async fn start(
        storage: Storage,
        session: &str,
        held: usize,
        protocol: Option<Arc<dyn lash_core::plugin::ProtocolSessionPlugin>>,
    ) -> Result<Self> {
        let World { engine, _keep, .. } = double_world(storage).await;
        let model = Arc::new(Model::holding(held));
        let old = engine
            .old_backend()
            .build_generation()
            .expect("the engine's generation is bound")
            .clone();
        let core = core_with_protocol(engine.old_backend(), engine.old_work(), &model, protocol);
        core.session(session).created().await.open().await?;
        let session = lash_core::SessionId::from(session);
        let store = lash_core::runtime::live_session_view(&core.store_factory, &session)
            .await?
            .expect("an opened session has a store");
        for index in 0..ROOTS {
            store
                .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                    session.clone(),
                    lash_core::TurnInputIngress::NextTurn,
                    TurnInput::text(format!("question {index}")),
                ))
                .await
                .expect("enqueue the input");
        }
        let request = DriveRequestId::new("drain-hand-over");
        core.substrate_slot
            .ports()
            .await
            .queued
            .schedule_drive(&session, request.clone());
        tokio::time::timeout(WEDGE, model.reached.notified())
            .await
            .expect("the drive's first root reaches its model call");
        let next = BuildGeneration::for_test("drain-hand-over-next");
        engine.roll(next.clone(), &model).await;
        assert!(
            engine
                .old_backend()
                .generation_drain()
                .mark_draining(&old, 1)
                .await
                .expect("mark build N draining"),
            "the law's mark is N's first"
        );
        Ok(Self {
            engine,
            core,
            model,
            session,
            request,
            old,
            next,
            _keep,
        })
    }

    /// The turns `generation` holds: `(in flight, parked)`.
    async fn turns(&self, generation: &BuildGeneration) -> (u64, u64) {
        let work = self
            .engine
            .old_backend()
            .generation_drain()
            .generation_work(generation)
            .await
            .expect("read the generation's work");
        (work.in_flight_turns, work.parked_turns)
    }

    /// Whether N's drain is complete.
    async fn old_drained(&self) -> Result<bool> {
        Ok(self
            .core
            .generation_drain_status(&self.old)
            .await?
            .drained())
    }
}

/// FIG-4742 (a), (c): the draining build counts the root still running on
/// it, and none of the roots the newest build admits from its hand-over, so
/// its drain completes while the newest build is still working the backlog.
async fn a_root_admitted_from_a_hand_over_counts_in_the_admitting_generation(
    storage: Storage,
) -> Result<()> {
    let roll = MidRoll::start(storage, "drain-hand-over-stamp", 2, None).await?;

    // N's root is in its model call: N holds it, and its drain waits on it.
    assert_eq!(
        (roll.turns(&roll.old).await, roll.turns(&roll.next).await),
        ((1, 0), (0, 0)),
        "the root still running on the draining build counts there"
    );
    assert!(
        !roll.old_drained().await?,
        "the drain waits for the root running on its build"
    );

    // N's root ends and N hands over; N+1 admits the next root, which
    // reaches its model call.
    roll.model.release.notify_one();
    tokio::time::timeout(WEDGE, roll.model.reached.notified())
        .await
        .expect("the newest build's first root reaches its model call");
    assert_eq!(
        (roll.turns(&roll.old).await, roll.turns(&roll.next).await),
        ((0, 0), (1, 0)),
        "a root admitted from the hand-over counts in the generation that admitted it"
    );
    assert!(
        roll.old_drained().await?,
        "the drain completes while the newest build runs the backlog"
    );

    roll.model.release.notify_one();
    let port = roll.core.substrate_slot.ports().await.queued;
    let outcome = tokio::time::timeout(WEDGE, port.await_drive(&roll.session, &roll.request))
        .await
        .expect("the drive chain ends")
        .expect("the drive is not refused");
    assert_eq!(outcome.stop, DriveStop::Idle, "{outcome:?}");
    assert_eq!(outcome.ran.len(), ROOTS, "{outcome:?}");
    assert_eq!(
        (roll.turns(&roll.old).await, roll.turns(&roll.next).await),
        ((0, 0), (0, 0)),
        "every root ended"
    );
    assert!(roll.old_drained().await?, "the drain stays complete");
    Ok(())
}

/// FIG-4742 (b): a root the newest build admits from a hand-over and then
/// parks names the newest build's generation, so the draining build's drain
/// does not wait on a park it cannot redrive.
async fn a_park_of_a_root_admitted_from_a_hand_over_names_the_admitting_generation(
    storage: Storage,
) -> Result<()> {
    let roll = MidRoll::start(
        storage,
        "drain-hand-over-park",
        1,
        Some(Arc::new(DivergingAt {
            question: "question 1".to_owned(),
        })),
    )
    .await?;
    roll.model.release.notify_one();

    // N's root commits, N hands over, and the root N+1 admits next parks.
    let store = lash_core::runtime::live_session_view(&roll.core.store_factory, &roll.session)
        .await?
        .expect("an opened session has a store");
    let park = tokio::time::timeout(WEDGE, async {
        loop {
            if let Some(park) = store.load_turn_park().await.expect("read the park") {
                break park;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the root the newest build admitted parks");
    assert_eq!(
        park.build_generation.as_ref(),
        Some(&roll.next),
        "the park names the generation that admitted its root: {park:?}"
    );
    assert_eq!(
        (roll.turns(&roll.old).await, roll.turns(&roll.next).await),
        ((0, 0), (1, 1)),
        "the park is the admitting generation's"
    );
    assert!(
        roll.old_drained().await?,
        "the draining build holds neither the parked root nor its park"
    );
    assert_eq!(
        *roll.model.asked.lock_recover(),
        ["question 0"],
        "the parked root never reached the model, and nothing ran past it"
    );
    Ok(())
}

/// FIG-4742 residual (FIG-4739): a root is held by the build that runs it.
/// A drive on N that is not draining admits its next root and calls it on
/// the stable name, which the newest build serves: the root's journal is
/// N+1's, so N+1 counts it and N holds nothing, whichever build's drive
/// admitted it.
async fn a_root_started_on_a_newer_build_counts_in_the_build_that_runs_it(
    storage: Storage,
) -> Result<()> {
    let World { engine, _keep, .. } = double_world(storage).await;
    let model = Arc::new(Model::holding(2));
    let old = engine
        .old_backend()
        .build_generation()
        .expect("the engine's generation is bound")
        .clone();
    let core = core_over(engine.old_backend(), engine.old_work(), &model);
    let session = lash_core::SessionId::from("drain-hand-over-runner-stamp");
    core.session(session.as_str())
        .created()
        .await
        .open()
        .await?;
    let store = lash_core::runtime::live_session_view(&core.store_factory, &session)
        .await?
        .expect("an opened session has a store");
    for index in 0..2 {
        store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                session.clone(),
                lash_core::TurnInputIngress::NextTurn,
                TurnInput::text(format!("question {index}")),
            ))
            .await
            .expect("enqueue the input");
    }
    let request = DriveRequestId::new("drain-hand-over");
    let port = core.substrate_slot.ports().await.queued;
    port.schedule_drive(&session, request.clone());
    tokio::time::timeout(WEDGE, model.reached.notified())
        .await
        .expect("the drive's first root reaches its model call");

    // N+1 registers beside N, and nothing is marked draining: N's drive goes
    // on admitting.
    let next = BuildGeneration::for_test("drain-hand-over-next");
    engine.roll(next.clone(), &model).await;
    model.release.notify_one();
    tokio::time::timeout(WEDGE, model.reached.notified())
        .await
        .expect("the root N's drive admitted next reaches its model call");
    let in_flight = |generation: BuildGeneration| {
        let drain = engine.old_backend().generation_drain();
        async move {
            drain
                .generation_work(&generation)
                .await
                .expect("read the generation's work")
                .in_flight_turns
        }
    };
    assert_eq!(
        (in_flight(old.clone()).await, in_flight(next.clone()).await),
        (0, 1),
        "the root counts in the generation of the build running it"
    );

    model.release.notify_one();
    let outcome = tokio::time::timeout(WEDGE, port.await_drive(&session, &request))
        .await
        .expect("the drive ends")
        .expect("the drive is not refused");
    assert_eq!(outcome.stop, DriveStop::Idle, "{outcome:?}");
    assert_eq!(outcome.ran.len(), 2, "{outcome:?}");
    assert_eq!(
        (in_flight(old).await, in_flight(next).await),
        (0, 0),
        "every root ended"
    );
    drop(core);
    engine.finish().await;
    Ok(())
}

async fn stamp_runner(storage: Storage, (): ()) -> Result<()> {
    a_root_started_on_a_newer_build_counts_in_the_build_that_runs_it(storage).await
}

async fn stamp_counts(storage: Storage, (): ()) -> Result<()> {
    a_root_admitted_from_a_hand_over_counts_in_the_admitting_generation(storage).await
}

async fn stamp_park(storage: Storage, (): ()) -> Result<()> {
    a_park_of_a_root_admitted_from_a_hand_over_names_the_admitting_generation(storage).await
}

macro_rules! drain_hand_over_laws {
    ($($(#[$attr:meta])* $name:ident: $run:ident, $storage:expr, $crash:expr;)*) => {
        $(
            $(#[$attr])*
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() -> Result<()> {
                $run($storage, $crash).await
            }
        )*
    };
}

drain_hand_over_laws! {
    hands_over_sqlite_memory: on_the_double, Storage::SqliteMemory, false;
    hands_over_sqlite_file: on_the_double, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    hands_over_postgres: on_the_double, Storage::Postgres, false;
    replay_hands_over_sqlite_memory: on_the_double, Storage::SqliteMemory, true;
    replay_hands_over_sqlite_file: on_the_double, Storage::SqliteFile, true;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    replay_hands_over_postgres: on_the_double, Storage::Postgres, true;
    stamp_counts_sqlite_memory: stamp_counts, Storage::SqliteMemory, ();
    stamp_counts_sqlite_file: stamp_counts, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    stamp_counts_postgres: stamp_counts, Storage::Postgres, ();
    stamp_runner_sqlite_memory: stamp_runner, Storage::SqliteMemory, ();
    stamp_runner_sqlite_file: stamp_runner, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    stamp_runner_postgres: stamp_runner, Storage::Postgres, ();
    stamp_park_sqlite_memory: stamp_park, Storage::SqliteMemory, ();
    stamp_park_sqlite_file: stamp_park, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    stamp_park_postgres: stamp_park, Storage::Postgres, ();
    #[ignore = "requires an isolated Restate server; run by the drain-hand-over suite"]
    live_restate_hands_over: on_live_restate, Storage::SqliteMemory, false;
    #[ignore = "requires an isolated Restate server; run by the drain-hand-over suite"]
    live_restate_replay_hands_over: on_live_restate, Storage::SqliteMemory, true;
    #[ignore = "requires a Restate server and PostgreSQL; run by the drain-hand-over-postgres suite"]
    postgres_live_restate_hands_over: on_live_restate, Storage::Postgres, false;
    #[ignore = "requires a Restate server and PostgreSQL; run by the drain-hand-over-postgres suite"]
    postgres_live_restate_replay_hands_over: on_live_restate, Storage::Postgres, true;
}

mod run_segment;
