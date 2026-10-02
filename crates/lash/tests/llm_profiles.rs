//! Host model keys (FIG-4374): a host registers opaque model keys in a
//! `LlmProfileRegistry`, several of them served by transports that share one
//! provider kind. A session records the binding its key minted at creation
//! and at every model change; a root runs the recorded binding, or ends its
//! attempt with the typed `LlmProfileUnavailable`, and never falls back to another
//! registration or to today's catalog entry.
//!
//! A recorded model is bound lazily (FIG-4404): only the body of an
//! unjournaled model call asks the host's models, so a replay of recorded
//! work completes on a deployment that retired the key, and a bind fault is
//! the attempt's, retried and never a recorded result.
//!
//! The host code below uses `lash::` paths only; the store tiers and the
//! Restate server double beneath them are the test's own infrastructure.

#![cfg(all(feature = "restate", feature = "sqlite", feature = "testing"))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "acceptance laws establish each step's result"
)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[path = "llm_profiles/session_turn_starts.rs"]
mod session_turn_starts;
use session_turn_starts::{
    a_session_turn_start_retried_after_the_host_changed_what_it_passes_keeps_its_retained_start,
    session_turn_start, start_on, unstated_session_turn_start,
};
#[path = "llm_profiles/parked_group.rs"]
mod parked_group;
use parked_group::{
    a_paused_group_retire_parks_its_process_opener, a_paused_group_retire_parks_its_root_opener,
    a_paused_group_run_parks_its_process_opener, a_paused_group_run_parks_its_root_opener,
    a_process_opened_group_child_parks_resumes_and_reparks_idempotently,
};

use lash::direct::LlmOutputPart;
use lash::provider::{LlmResponse, ProviderHandle};
use lash::{
    LashCore, LlmProfileKey, LlmProfileMetadata, LlmProfileRegistry, RegisteredLlmProfile,
    TurnInput,
};

/// The provider kind every transport of the host's catalog shares.
const KIND: &str = "tensorx-compat";
const GLM: &str = "glm-5.3-flash@tensorx";
const KIMI: &str = "kimi-k3@tensorx";

// ---- store tiers ------------------------------------------------------------

#[derive(Clone, Copy)]
enum Tier {
    SqliteMemory,
    SqliteFile,
    Postgres,
}

struct Double {
    double: lash_restate_test::RestateTestBackend<dyn lash::StoreSet>,
    artifacts: ArtifactProbe,
    _keep: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

enum ArtifactProbe {
    Sqlite(String),
    Postgres(sqlx::PgPool),
}

impl ArtifactProbe {
    async fn counts(&self) -> (i64, i64, i64, i64) {
        match self {
            Self::Sqlite(uri) => {
                let connection = rusqlite::Connection::open_with_flags(
                    uri,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                )
                .expect("inspect the SQLite artifacts");
                connection
                    .query_row(
                        "SELECT (SELECT count(*) FROM artifact_refs),
                                (SELECT count(*) FROM artifact_referrer_edges),
                                (SELECT count(*) FROM artifact_cleanup_obligations),
                                (SELECT count(*) FROM referrer_fences)",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .expect("count SQLite artifact records")
            }
            Self::Postgres(pool) => sqlx::query_as(
                "SELECT (SELECT count(*) FROM lash_lashlang_artifacts),
                        (SELECT count(*) FROM lash_artifact_referrer_edges),
                        (SELECT count(*) FROM lash_artifact_cleanup_obligations),
                        (SELECT count(*) FROM lash_referrer_fences)",
            )
            .fetch_one(pool)
            .await
            .expect("count PostgreSQL artifact records"),
        }
    }
}

async fn double(tier: Tier, replay: bool, seed: u64) -> Option<Double> {
    double_with_hooks(
        tier,
        replay,
        seed,
        lash_restate_test::DeploymentHooks::default(),
    )
    .await
}

async fn double_with_hooks(
    tier: Tier,
    replay: bool,
    seed: u64,
    hooks: lash_restate_test::DeploymentHooks,
) -> Option<Double> {
    let mut config = lash_restate_test::ServerConfig {
        always_replay: replay,
        ..lash_restate_test::ServerConfig::default()
    };
    if hooks.refuse.is_some() {
        config.retry.initial_interval = std::time::Duration::ZERO;
        config.retry.max_interval = std::time::Duration::ZERO;
        config.retry.max_attempts = Some(8);
    }
    match tier {
        Tier::SqliteMemory => {
            let inspection = Arc::new(Mutex::new(None));
            let captured = Arc::clone(&inspection);
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks, |clock| async {
                    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                        .await
                        .expect("SQLite memory stores");
                    *captured.lock().expect("inspection URI") =
                        Some(stores.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore));
                    Ok(Arc::new(stores) as Arc<dyn lash::StoreSet>)
                })
                .await
                .expect("SQLite memory Restate double");
            Some(Double {
                double,
                artifacts: ArtifactProbe::Sqlite(
                    inspection
                        .lock()
                        .expect("inspection URI")
                        .take()
                        .expect("URI captured"),
                ),
                _keep: Vec::new(),
            })
        }
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let path = root.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks, |clock| async {
                    Ok(Arc::new(
                        lash_sqlite_store::SqliteStoreSet::open_with_clock(&path, clock)
                            .await
                            .expect("SQLite file stores"),
                    ) as Arc<dyn lash::StoreSet>)
                })
                .await
                .expect("SQLite file Restate double");
            Some(Double {
                double,
                artifacts: ArtifactProbe::Sqlite(format!(
                    "file:{}",
                    path.join("durable-core.db").display()
                )),
                _keep: vec![Box::new(root)],
            })
        }
        Tier::Postgres => {
            let url = lash_postgres_store::testing::required_database_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let attachment_path = attachments.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks, |clock| async {
                    Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                        &storage,
                        Arc::new(lash::persistence::FileAttachmentStore::new(
                            &attachment_path,
                        )),
                        lash_core::WakeDeliveryConfig::default(),
                        clock,
                    )) as Arc<dyn lash::StoreSet>)
                })
                .await
                .expect("PostgreSQL Restate double");
            Some(Double {
                double,
                artifacts: ArtifactProbe::Postgres(storage.pool().clone()),
                _keep: vec![Box::new(database), Box::new(storage), Box::new(attachments)],
            })
        }
    }
}

// ---- the host ---------------------------------------------------------------

/// What one transport saw of a request: the wire model and its recorded
/// request extensions.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    wire_model: String,
    revision: Option<String>,
}

/// One registered transport: every one has the same kind, answers with its
/// own text and records each request it serves.
struct Route {
    text: &'static str,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Route {
    fn new(text: &'static str) -> Self {
        Self {
            text,
            seen: Arc::default(),
        }
    }

    fn handle(&self) -> ProviderHandle {
        let text = self.text;
        let seen = Arc::clone(&self.seen);
        lash::testing::TestProvider::builder()
            .kind(KIND)
            .complete(move |request| {
                seen.lock().expect("seen requests").push(Seen {
                    wire_model: request.model.wire_model().to_string(),
                    revision: request
                        .model
                        .metadata()
                        .extra_body
                        .get("catalog_revision")
                        .and_then(|value| value.as_str())
                        .map(str::to_string),
                });
                async move {
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: text.to_string(),
                            response_meta: None,
                        }],
                        ..LlmResponse::default()
                    })
                }
            })
            .build()
            .into_handle()
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen requests").clone()
    }

    fn calls(&self) -> usize {
        self.seen().len()
    }
}

/// One catalog entry: a key, the wire model and catalog revision its
/// metadata records, and the transport that serves it.
struct Entry<'a> {
    key: &'a str,
    wire_model: &'a str,
    revision: &'a str,
    route: &'a Route,
}

fn metadata(wire_model: &str, revision: &str) -> LlmProfileMetadata {
    let mut extra_body = serde_json::Map::new();
    extra_body.insert("catalog_revision".to_string(), revision.into());
    LlmProfileMetadata::builder(wire_model)
        .context_window_tokens(64_000)
        .extra_body(extra_body)
        .build()
        .expect("model metadata")
}

fn catalog(entries: &[Entry<'_>]) -> Arc<LlmProfileRegistry> {
    let registry = entries
        .iter()
        .try_fold(LlmProfileRegistry::new(), |registry, entry| {
            registry.register(
                entry.key,
                RegisteredLlmProfile::new(
                    metadata(entry.wire_model, entry.revision),
                    entry.route.handle(),
                ),
            )
        })
        .expect("the catalog names each key once");
    Arc::new(registry)
}

/// A core over `entries`.
fn core(double: &Double, entries: &[Entry<'_>], worker: &str) -> LashCore {
    LashCore::standard_builder(double.double.lash_backend())
        .llm_profiles(catalog(entries))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "model-keys-worker",
            worker,
        ))
        .expect("the host core builds")
}

async fn created_on(core: &LashCore, id: &str, key: &str) -> lash::LashSession {
    core.session(lash_core::SessionId::fixture(id.to_string()))
        .create(lash::SessionCreation {
            spec: lash::SessionSpec::new(
                key,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            ),
            parent: None,
        })
        .await
        .expect("create the session");
    core.session(lash_core::SessionId::fixture(id.to_string()))
        .open()
        .await
        .expect("open the session")
}

async fn answer_of(handle: lash::SendHandle) -> String {
    let output = handle.output().await.expect("the root settles");
    assert!(output.is_success(), "the root answers: {output:?}");
    output
        .assistant_message()
        .expect("the root answers with text")
        .to_string()
}

fn recorded_key(session: &lash::LashSession) -> LlmProfileKey {
    session
        .policy_snapshot()
        .model
        .expect("the session records a model")
        .key()
        .clone()
}

/// Wait until an attempt on the double ended with the typed refusal of an
/// unbindable recorded model.
async fn await_llm_profile_unavailable(double: &Double) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let failure = double
                .double
                .server()
                .invocations()
                .into_iter()
                .filter_map(|view| view.last_failure.map(|(_, message)| message))
                .find(|message| message.contains("llm_profile_unavailable"));
            if let Some(failure) = failure {
                return failure;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let invocations = double
            .double
            .server()
            .invocations()
            .into_iter()
            .map(|view| format!("{view:?}"))
            .collect::<Vec<_>>();
        panic!("an attempt ends with the typed model-unavailable refusal; saw {invocations:#?}")
    })
}

// ---- a catalog edited under a running core ----------------------------------

/// A deployment's models whose catalog the test edits while one core serves
/// it, counting every question the runtime asks it.
#[derive(Default)]
struct LiveCatalog {
    served: Mutex<LlmProfileRegistry>,
    snapshots: AtomicUsize,
    binds: AtomicUsize,
}

impl LiveCatalog {
    fn serving(registry: LlmProfileRegistry) -> Arc<Self> {
        let catalog = Arc::new(Self::default());
        catalog.serve(registry);
        catalog
    }

    /// Replace the catalog, and count resolver calls from here on.
    fn serve(&self, registry: LlmProfileRegistry) {
        *self.served.lock().expect("served catalog") = registry;
        self.snapshots.store(0, Ordering::SeqCst);
        self.binds.store(0, Ordering::SeqCst);
    }

    /// The `(snapshot, bind)` calls made since the catalog was last served.
    fn resolver_calls(&self) -> (usize, usize) {
        (
            self.snapshots.load(Ordering::SeqCst),
            self.binds.load(Ordering::SeqCst),
        )
    }
}

impl lash::LlmProfiles for LiveCatalog {
    fn snapshot(
        &self,
        key: &LlmProfileKey,
    ) -> Result<lash::RecordedLlmProfile, lash::LlmProfileUnavailable> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        self.served.lock().expect("served catalog").snapshot(key)
    }

    fn bind(
        &self,
        recorded: &lash::RecordedLlmProfile,
    ) -> Result<ProviderHandle, lash::LlmProfileUnavailable> {
        self.binds.fetch_add(1, Ordering::SeqCst);
        self.served.lock().expect("served catalog").bind(recorded)
    }
}

fn registry_of(key: &str, wire_model: &str, provider: ProviderHandle) -> LlmProfileRegistry {
    LlmProfileRegistry::new()
        .register(
            key,
            RegisteredLlmProfile::new(metadata(wire_model, "r1"), provider),
        )
        .expect("the catalog names the key once")
}

fn text(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

/// A core over `catalog`, with `plugins`.
fn core_over(
    double: &Double,
    catalog: &Arc<LiveCatalog>,
    plugins: Vec<Arc<dyn lash::plugins::PluginFactory>>,
) -> LashCore {
    let mut builder = LashCore::standard_builder(double.double.lash_backend())
        .llm_profiles(Arc::clone(catalog) as Arc<dyn lash::LlmProfiles>)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1));
    for plugin in plugins {
        builder = builder.plugin(plugin);
    }
    builder
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "model-keys-worker",
            "keys-live-catalog",
        ))
        .expect("the host core builds")
}

/// Wait until the engine stopped retrying an invocation whose attempts
/// failed on the unbindable `key`, and return it: the eight-attempt park.
/// An engine that instead settled every root recorded the fault.
async fn await_parked_on(double: &Double, key: &str) -> lash_restate_test::InvocationView {
    let turns = double
        .double
        .service_name(lash_restate_test::TURN_DRIVER_SERVICE);
    let parked = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let invocations = double.double.server().invocations();
            if let Some(parked) = invocations.iter().find(|view| {
                view.status == "paused"
                    && view
                        .last_failure
                        .as_ref()
                        .is_some_and(|(_, message)| message.contains(key))
            }) {
                return Some(parked.clone());
            }
            let roots = invocations
                .iter()
                .filter(|view| view.target.starts_with(&turns) && view.target.ends_with("/run"))
                .collect::<Vec<_>>();
            if !roots.is_empty() && roots.iter().all(|view| view.status == "completed") {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    let invocations = || {
        double
            .double
            .server()
            .invocations()
            .into_iter()
            .map(|view| format!("{view:?}"))
            .collect::<Vec<_>>()
    };
    match parked {
        Ok(Some(parked)) => parked,
        Ok(None) => panic!(
            "the root settled although its model could not be bound: the bind fault was \
             recorded instead of retried; saw {:#?}",
            invocations()
        ),
        Err(_) => panic!(
            "the engine parks the work whose model cannot be bound; saw {:#?}",
            invocations()
        ),
    }
}

/// No `ctx.run` of `invocation` journaled anything that names the bind
/// fault: the fault ended attempts and was never a step's recorded result.
fn assert_no_recorded_bind_fault(double: &Double, invocation: &lash_restate_test::InvocationView) {
    let journal = double
        .double
        .server()
        .journal(&invocation.id)
        .expect("the parked invocation keeps its journal");
    for entry in journal {
        let recorded = match entry.run_completion() {
            None => continue,
            Some(Ok(value)) => String::from_utf8_lossy(&value).into_owned(),
            Some(Err((code, message))) => format!("{code}: {message}"),
        };
        assert!(
            !recorded.contains("llm_profile_unavailable") && !recorded.contains("is unavailable"),
            "a journaled step result carries the bind fault: {recorded}"
        );
    }
}

/// The park `handle`'s root is in: the park the exhausted retries of the
/// `parked` invocation became.
async fn park_of(
    handle: lash::SendHandle,
    parked: &lash_restate_test::InvocationView,
) -> lash::ParkedTurn {
    let settled = tokio::time::timeout(std::time::Duration::from_secs(60), handle.output())
        .await
        .unwrap_or_else(|_| panic!("the status of the root parked as {parked:?} is readable"));
    let status = match settled {
        Err(lash::EmbedError::Send(error)) => match *error {
            lash::SendError::NotSettled { status, .. } => status,
            other => panic!("the root is parked, got: {other:?}"),
        },
        other => panic!("the root is parked, got: {other:?}"),
    };
    let lash::TurnStatus::Parked(parked) = status else {
        panic!("the root is parked, got: {status:?}");
    };
    assert_eq!(
        parked.reason.code(),
        lash::persistence::ParkReasonCode::EngineRetryExhausted,
        "the root parked on its exhausted retries: {parked:?}"
    );
    parked
}

/// The answer of the redriven `root`. The redrive is accepted before the root
/// moves: its park stands until the resumed attempt gets past the model call.
async fn answer_after_redrive(session: &lash::LashSession, root: &str) -> String {
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            match session
                .attach_id(lash::TurnId::fixture(root.to_string()))
                .output()
                .await
            {
                Ok(output) => return output,
                Err(lash::EmbedError::Send(_)) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(error) => panic!("the redriven root settles: {error:?}"),
            }
        }
    })
    .await
    .expect("the redriven root leaves its park");
    output
        .assistant_message()
        .expect("the root answers with text")
        .to_string()
}

/// Every live park, as the host's park surface lists it: the work, its park,
/// and the refusals the park counts.
async fn listed_parks(
    core: &LashCore,
) -> Vec<(lash::ParkedWorkRef, lash::persistence::ParkId, u32, u64)> {
    let limit = std::num::NonZeroUsize::new(8).expect("non-zero");
    core.parked_work()
        .list(&lash::ParkedWorkQuery::all(limit))
        .await
        .expect("the park surface lists parked work")
        .records
        .into_iter()
        .map(|record| {
            (
                record.target,
                record.park_id,
                record.attempts,
                record.last_refused_ms,
            )
        })
        .collect()
}

/// One park reconcile pass of the engine, run until a pass read the engine
/// without a failure, as the recovery interval retries one.
async fn reconcile_pass(double: &Double) -> lash_core::engine::ParkReconcileReport {
    let backend = double.double.lash_backend();
    let sessions = backend.session_store_factory();
    let clock = backend.clock();
    let writer = lash_core::drive::StoreParkRecovery::new(sessions.as_ref(), clock.as_ref());
    let control = backend.session_work().control();
    let mut last = None;
    for _ in 0..20 {
        let pass = control
            .reconcile_parks(
                &writer,
                lash_core::engine::EnginePage {
                    after: None,
                    limit: std::num::NonZeroUsize::new(16).expect("non-zero"),
                    budget: std::time::Duration::from_secs(5),
                },
            )
            .await;
        if let Ok(report) = &pass
            && report.failed.is_empty()
        {
            return report.clone();
        }
        last = Some(pass);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("a reconcile pass reads the engine: {last:?}");
}

// ---- the laws ---------------------------------------------------------------

#[path = "llm_profiles/child_parent.rs"]
mod child_parent;
use child_parent::fig4669_child_parent_survives_every_runtime_reopen;

/// Two keys served by transports of one provider kind: each send's model
/// key selects its own transport, a send without one runs the session's
/// recorded model, and a per-run key never changes the session's record.
async fn two_keys_sharing_a_provider_kind_select_their_own_transport(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let glm = Route::new("glm answers");
    let kimi = Route::new("kimi answers");
    let core = core(
        &double,
        &[
            Entry {
                key: GLM,
                wire_model: "glm-5.3-flash",
                revision: "r1",
                route: &glm,
            },
            Entry {
                key: KIMI,
                wire_model: "kimi-k3",
                revision: "r1",
                route: &kimi,
            },
        ],
        "keys-boot",
    );
    let session = created_on(&core, "keys-select", GLM).await;

    let on_kimi = session
        .send(TurnInput::text("ask kimi"))
        .model(KIMI)
        .await
        .expect("a registered key is accepted");
    assert_eq!(answer_of(on_kimi).await, "kimi answers");
    assert_eq!(
        recorded_key(&session).as_str(),
        GLM,
        "the session's view never shows a settled root's per-run key"
    );
    let on_session = session
        .send(TurnInput::text("ask the session's model"))
        .await
        .expect("the session's model is accepted");
    assert_eq!(answer_of(on_session).await, "glm answers");

    assert_eq!(
        kimi.seen(),
        vec![Seen {
            wire_model: "kimi-k3".to_string(),
            revision: Some("r1".to_string()),
        }],
        "only the kimi send reached the kimi transport, with kimi's wire model"
    );
    assert_eq!(glm.calls(), 1, "the default send reached the glm transport");
    assert_eq!(
        recorded_key(&session).as_str(),
        GLM,
        "a per-run key leaves the session's recorded model alone"
    );
}

/// A catalog edit under an unchanged key never reaches a session on its
/// own: the session's roots keep sending the metadata it recorded, on a
/// redeployed transport too. Changing the model to the same key re-mints
/// the binding, and the session's next root sends the edited metadata.
async fn a_catalog_edit_reaches_a_session_only_through_a_profile_change(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-catalog-edit";
    let first_kimi = Route::new("kimi answers");
    let first = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "kimi-k3",
            revision: "r1",
            route: &first_kimi,
        }],
        "keys-boot-1",
    );
    drop(created_on(&first, session_id, KIMI).await);
    double
        .double
        .settle_session_drive(&lash::SessionId::from(session_id))
        .await;
    drop(first);

    let edited_kimi = Route::new("edited kimi answers");
    let edited = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "kimi-k3",
            revision: "r2",
            route: &edited_kimi,
        }],
        "keys-boot-2",
    );
    let session = edited
        .session(session_id)
        .open()
        .await
        .expect("the redeployed host opens the session");
    let before = session
        .send(TurnInput::text("before the change"))
        .await
        .expect("the recorded model is accepted");
    assert_eq!(answer_of(before).await, "edited kimi answers");
    assert_eq!(
        edited_kimi.seen(),
        vec![Seen {
            wire_model: "kimi-k3".to_string(),
            revision: Some("r1".to_string()),
        }],
        "the root sends the metadata the session recorded, not the edited catalog's"
    );

    let config = session.admin().config();
    let revision = config.revision().await.expect("read the config revision");
    let reselected = config
        .apply(
            lash::config::ConfigWrite::new("keys-reselect-kimi", revision),
            lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                model: LlmProfileKey::new(KIMI),
            }),
        )
        .await
        .expect("the model change settles");
    assert!(
        matches!(
            reselected,
            lash::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "re-selecting the same key re-mints its binding: {reselected:?}"
    );
    let after = session
        .send(TurnInput::text("after the change"))
        .await
        .expect("the re-minted model is accepted");
    assert_eq!(answer_of(after).await, "edited kimi answers");
    assert_eq!(
        edited_kimi.seen().last(),
        Some(&Seen {
            wire_model: "kimi-k3".to_string(),
            revision: Some("r2".to_string()),
        }),
        "after the model change the root sends the re-minted metadata"
    );
    assert_eq!(first_kimi.calls(), 0);
}

/// A session whose recorded key left the catalog ends each attempt with the
/// typed `LlmProfileUnavailable` and calls no other registration, the new default
/// included; a deployment that registers the key again runs it there.
async fn a_recorded_llm_profile_whose_key_left_the_catalog_fails_typed_and_never_falls_back(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-key-removed";
    let glm = Route::new("glm answers");
    let kimi = Route::new("old kimi answers");
    let first = core(
        &double,
        &[
            Entry {
                key: GLM,
                wire_model: "glm-5.3-flash",
                revision: "r1",
                route: &glm,
            },
            Entry {
                key: KIMI,
                wire_model: "kimi-k3",
                revision: "r1",
                route: &kimi,
            },
        ],
        "keys-boot-1",
    );
    let session = created_on(&first, session_id, KIMI).await;
    let hold = double
        .double
        .hold_session_drive(&lash::SessionId::from(session_id))
        .await;
    session
        .send(TurnInput::text("ask the session's model"))
        .id("keys-key-removed-root")
        .await
        .expect("the held session accepts the input");
    drop(session);
    drop(first);

    let unserving = core(
        &double,
        &[Entry {
            key: GLM,
            wire_model: "glm-5.3-flash",
            revision: "r1",
            route: &glm,
        }],
        "keys-boot-2",
    );
    hold.release();
    let failure = await_llm_profile_unavailable(&double).await;
    assert!(
        failure.contains(KIMI),
        "the refusal names the recorded key: {failure}"
    );
    assert_eq!(glm.calls(), 0, "no other registration is substituted");
    assert_eq!(kimi.calls(), 0);

    drop(unserving);
    let restored_kimi = Route::new("restored kimi answers");
    let restored = core(
        &double,
        &[
            Entry {
                key: GLM,
                wire_model: "glm-5.3-flash",
                revision: "r1",
                route: &glm,
            },
            Entry {
                key: KIMI,
                wire_model: "kimi-k3",
                revision: "r9",
                route: &restored_kimi,
            },
        ],
        "keys-boot-3",
    );
    let session = restored
        .session(session_id)
        .open()
        .await
        .expect("the restored host opens the session");
    assert_eq!(
        answer_of(session.attach_id("keys-key-removed-root")).await,
        "restored kimi answers",
        "the retried root runs its recorded model once the key is served"
    );
    assert_eq!(
        restored_kimi.seen(),
        vec![Seen {
            wire_model: "kimi-k3".to_string(),
            revision: Some("r1".to_string()),
        }],
        "the restored transport serves the recorded metadata"
    );
    assert_eq!(glm.calls(), 0, "no other registration is substituted");
}

/// A key that now names another wire model cannot serve a session that
/// recorded the old one: every attempt ends with the typed
/// `LlmProfileUnavailable`, and the transport is never sent the request.
async fn a_recorded_model_whose_key_serves_another_wire_model_is_refused_typed(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-wire-model-changed";
    let kimi = Route::new("kimi answers");
    let first = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "kimi-k3",
            revision: "r1",
            route: &kimi,
        }],
        "keys-boot-1",
    );
    let session = created_on(&first, session_id, KIMI).await;
    let hold = double
        .double
        .hold_session_drive(&lash::SessionId::from(session_id))
        .await;
    session
        .send(TurnInput::text("ask the session's model"))
        .id("keys-wire-model-changed-root")
        .await
        .expect("the held session accepts the input");
    drop(session);
    drop(first);

    let newer_kimi = Route::new("newer kimi answers");
    let _changed = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "kimi-k4",
            revision: "r2",
            route: &newer_kimi,
        }],
        "keys-boot-2",
    );
    hold.release();
    let failure = await_llm_profile_unavailable(&double).await;
    assert!(
        failure.contains("kimi-k4") && failure.contains("kimi-k3"),
        "the refusal names the served and the recorded wire models: {failure}"
    );
    assert_eq!(newer_kimi.calls(), 0, "the transport never saw the request");
    assert_eq!(kimi.calls(), 0);
}

/// A key the catalog does not register is refused typed and nothing is
/// written: at creation and at send before anything is accepted, and at a
/// model change when its transaction resolves, where the session keeps its
/// recorded model and its config revision.
async fn an_unknown_key_is_refused_before_anything_changes(tier: Tier, replay: bool, seed: u64) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let glm = Route::new("glm answers");
    let core = core(
        &double,
        &[Entry {
            key: GLM,
            wire_model: "glm-5.3-flash",
            revision: "r1",
            route: &glm,
        }],
        "keys-boot",
    );
    let unregistered = "unregistered@nowhere";

    let created = core
        .session("keys-unknown-create")
        .create(lash::SessionCreation {
            spec: lash::SessionSpec::new(
                unregistered,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            ),
            parent: None,
        })
        .await;
    assert!(
        matches!(&created, Err(lash::EmbedError::LlmProfileUnknown(error)) if error.key.as_str() == unregistered),
        "creation refuses the unknown key typed: {:?}",
        created.as_ref().err()
    );
    let reopened = core.session("keys-unknown-create").open().await;
    assert!(
        matches!(&reopened, Err(lash::EmbedError::UnknownSession { .. })),
        "the refused creation created nothing: {:?}",
        reopened.as_ref().err()
    );

    let session = created_on(&core, "keys-unknown-patch", GLM).await;
    let sent = session
        .send(TurnInput::text("ask nobody"))
        .model(unregistered)
        .await;
    match &sent {
        Err(lash::EmbedError::Runtime(error)) => {
            assert_eq!(
                error.code,
                lash::runtime::RuntimeErrorCode::LlmProfileUnknown
            );
        }
        other => panic!("the send is refused typed, got: {:?}", other.as_ref().err()),
    }

    let before = session.policy_snapshot();
    let config = session.admin().config();
    let revision = config.revision().await.expect("read the config revision");
    let changed = config
        .apply(
            lash::config::ConfigWrite::new("keys-unknown-change", revision),
            lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                model: LlmProfileKey::new(unregistered),
            }),
        )
        .await
        .expect("the model change settles");
    let lash::config::ConfigTransactionOutcome::Refused { refusal } = changed else {
        panic!("the model change is refused typed: {changed:?}");
    };
    assert_eq!(
        refusal
            .owner_refusal::<lash::config::CoreConfigRefusal>()
            .expect("the core owner's typed refusal"),
        lash::config::CoreConfigRefusal::UnknownLlmProfile {
            key: LlmProfileKey::new(unregistered),
        }
    );
    assert_eq!(
        config.revision().await.expect("read the config revision"),
        revision,
        "the refused change published nothing"
    );
    assert_eq!(
        session.policy_snapshot().model,
        before.model,
        "the refused change left the recorded model alone"
    );
    assert_eq!(glm.calls(), 0, "no refused request ran");
}

/// An unsupported reasoning selection is refused typed where it is stated,
/// and nothing is written: at creation no session exists afterwards, and at
/// send the input is not accepted and no transport is called. The same key
/// with a selection its capability accepts runs.
async fn an_unsupported_reasoning_selection_is_refused_where_it_is_stated(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    const THINKER: &str = "thinker@tensorx";
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let thinker = Route::new("thinker answers");
    let glm = Route::new("glm answers");
    let high = lash::provider::ReasoningSelection::Effort("high".to_string());
    let thinking = LlmProfileMetadata::builder("thinker-1")
        .context_window_tokens(64_000)
        .capability(lash::provider::LlmProfileCapability {
            reasoning: Some(lash::provider::ReasoningCapability {
                efforts: vec!["high".to_string()],
                encoding: lash::provider::ReasoningEncoding::Effort,
                disable: false,
                mandatory: false,
            }),
            ..lash::provider::LlmProfileCapability::default()
        })
        .build()
        .expect("model metadata");
    let registry = LlmProfileRegistry::new()
        .register(
            THINKER,
            RegisteredLlmProfile::new(thinking, thinker.handle()),
        )
        .and_then(|registry| {
            registry.register(
                GLM,
                RegisteredLlmProfile::new(metadata("glm-5.3-flash", "r1"), glm.handle()),
            )
        })
        .expect("the catalog names each key once");
    let core = LashCore::standard_builder(double.double.lash_backend())
        .llm_profiles(Arc::new(registry))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "model-keys-worker",
            "keys-reasoning",
        ))
        .expect("the host core builds");

    // Creation: a key with no reasoning controls cannot record an effort.
    let created = core
        .session("keys-reasoning-refused")
        .create(lash::SessionCreation {
            spec: lash::SessionSpec::new(
                GLM,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .reasoning(high.clone()),
            parent: None,
        })
        .await;
    match &created {
        Err(lash::EmbedError::ReasoningRefused(refused)) => {
            assert_eq!(refused.key.as_str(), GLM);
            assert_eq!(refused.reasoning, high);
        }
        other => panic!(
            "creation refuses the selection typed, got: {:?}",
            other.as_ref().err()
        ),
    }
    let reopened = core.session("keys-reasoning-refused").open().await;
    assert!(
        matches!(&reopened, Err(lash::EmbedError::UnknownSession { .. })),
        "the refused creation created nothing: {:?}",
        reopened.as_ref().err()
    );

    // Send: the session records `high` on the thinking model.
    core.session("keys-reasoning")
        .create(lash::SessionCreation {
            spec: lash::SessionSpec::new(
                THINKER,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .reasoning(high.clone()),
            parent: None,
        })
        .await
        .expect("the advertised effort is recorded");
    let session = core
        .session("keys-reasoning")
        .open()
        .await
        .expect("open the session");
    let refused_code = |sent: Result<lash::SendHandle, lash::EmbedError>| match sent {
        Err(lash::EmbedError::Runtime(error)) => error.code,
        other => panic!("the send is refused typed, got: {:?}", other.err()),
    };
    // A per-run key keeps the session's reasoning, which this key refuses.
    assert_eq!(
        refused_code(session.send(TurnInput::text("ask glm")).model(GLM).await),
        lash::runtime::RuntimeErrorCode::ReasoningRefused
    );
    // A per-run effort the session's model does not advertise.
    assert_eq!(
        refused_code(
            session
                .send(TurnInput::text("think harder"))
                .reasoning(lash::provider::ReasoningSelection::Effort(
                    "turbo".to_string()
                ))
                .await
        ),
        lash::runtime::RuntimeErrorCode::ReasoningRefused
    );
    assert_eq!(glm.calls() + thinker.calls(), 0, "no refused request ran");

    let on_glm = session
        .send(TurnInput::text("ask glm plainly"))
        .model(GLM)
        .reasoning(lash::provider::ReasoningSelection::ProviderDefault)
        .await
        .expect("the provider's default reasoning fits the key");
    assert_eq!(answer_of(on_glm).await, "glm answers");
    assert_eq!(thinker.calls(), 0);
}

/// A root whose model call is already journaled replays on a deployment
/// that retired its key: the replay serves the recorded call, completes the
/// root, and never asks the deployment's models for anything.
async fn a_replay_after_the_key_left_the_catalog_completes_with_zero_resolver_calls(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let catalog = Arc::new(LiveCatalog::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = {
        // The key is retired while its one model call is in flight: the
        // call's result is journaled, and everything after it is a replay
        // or runs on the deployment without the key.
        let retiring = Arc::clone(&catalog);
        let calls = Arc::clone(&calls);
        lash::testing::TestProvider::builder()
            .kind(KIND)
            .complete(move |_request| {
                calls.fetch_add(1, Ordering::SeqCst);
                retiring.serve(LlmProfileRegistry::new());
                async move { Ok(text("kimi answers")) }
            })
            .build()
            .into_handle()
    };
    catalog.serve(registry_of(KIMI, "kimi-k3", provider));
    let core = core_over(&double, &catalog, Vec::new());
    let session = created_on(&core, "keys-replay-key-removed", KIMI).await;
    // Cut the root's attempt after its model call is journaled, so the
    // engine replays the root from its journal on a plain server too.
    double
        .double
        .crash_turn_drive(lash_restate_test::CrashPoint::BeforeRunResultEnding {
            suffix: REPLAY_CUT_AFTER_MODEL_CALL.to_string(),
        });

    let handle = session
        .send(TurnInput::text("ask the session's model"))
        .await
        .expect("the recorded model is accepted");
    assert_eq!(answer_of(handle).await, "kimi answers");
    double
        .double
        .settle_session_drive(&lash::SessionId::from("keys-replay-key-removed"))
        .await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the journaled model call is served from the journal, never made again"
    );
    let turns = double
        .double
        .service_name(lash_restate_test::TURN_DRIVER_SERVICE);
    let roots = double
        .double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with(&turns))
        .collect::<Vec<_>>();
    assert!(
        roots
            .iter()
            .any(|view| view.attempts > 1 || view.suspensions > 0),
        "the root replayed its journal after the key was retired; its journals name {:#?}",
        roots
            .iter()
            .map(|view| double
                .double
                .server()
                .journal(&view.id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|entry| entry.name)
                .collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        catalog.resolver_calls(),
        (0, 0),
        "the replay made no (snapshot, bind) call on the deployment that retired the key"
    );
}

/// The journal run whose result the removed-key law cuts: the checkpoint
/// that follows a root's journaled model call.
const REPLAY_CUT_AFTER_MODEL_CALL: &str = ":checkpoint:3";

/// A root whose model call is not journaled yet meets a deployment that
/// retired its key: every attempt ends with the typed bind fault, nothing is
/// journaled as the call's result, and the engine parks the root after its
/// eight attempts. Once the key is served again the resumed root makes the
/// call and answers.
async fn an_unjournaled_bind_fault_seals_nothing_and_recovers_after_the_park(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-bind-fault-park";
    let kimi = Route::new("kimi answers");
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", kimi.handle()));
    let core = core_over(&double, &catalog, Vec::new());
    let session = created_on(&core, session_id, KIMI).await;
    let hold = double
        .double
        .hold_session_drive(&lash::SessionId::from(session_id))
        .await;
    session
        .send(TurnInput::text("ask the session's model"))
        .id("keys-bind-fault-park-root")
        .await
        .expect("the held session accepts the input");
    catalog.serve(LlmProfileRegistry::new());
    hold.release();

    let parked = await_parked_on(&double, KIMI).await;
    assert_eq!(
        u64::from(parked.retry_count),
        lash_restate::TURN_HANDLER_MAX_ATTEMPTS,
        "the engine parks the root after its attempt budget: {parked:?}"
    );
    let (_, failure) = parked
        .last_failure
        .clone()
        .expect("the park's last failure");
    assert!(
        failure.contains("llm_profile_unavailable") && failure.contains(KIMI),
        "the park's failure is the typed bind fault and names the recorded key: {failure}"
    );
    assert_no_recorded_bind_fault(&double, &parked);
    let park = park_of(session.attach_id("keys-bind-fault-park-root"), &parked).await;
    assert_eq!(
        park.reason.profile_key(),
        Some(&LlmProfileKey::new(KIMI)),
        "the root's park carries the unbindable key typed: {park:?}"
    );
    assert_eq!(kimi.calls(), 0, "no attempt reached a transport");
    let (snapshots, binds) = catalog.resolver_calls();
    assert_eq!(snapshots, 0, "a recorded model is never minted again");
    assert!(binds >= 1, "the unjournaled call asked for its binding");

    let restored = Route::new("restored kimi answers");
    catalog.serve(registry_of(KIMI, "kimi-k3", restored.handle()));
    core.parked_work()
        .redrive(
            &lash::ParkedWorkRef::Turn {
                session_id: park.session_id.clone(),
                turn_id: park.root.clone(),
            },
            park.park_id,
        )
        .await
        .expect("the operator redrives the parked root");
    assert_eq!(
        answer_after_redrive(&session, "keys-bind-fault-park-root").await,
        "restored kimi answers",
        "the resumed root makes its model call once the key is served"
    );
    assert_eq!(
        restored.seen(),
        vec![Seen {
            wire_model: "kimi-k3".to_string(),
            revision: Some("r1".to_string()),
        }],
        "the restored transport serves the recorded metadata, once"
    );
    assert_eq!(kimi.calls(), 0);
}

const ASK_MODEL: &str = "ask_model";

fn ask_model_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:ask_model",
        ASK_MODEL,
        "Ask the session's model one question through a direct completion.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        serde_json::json!({"type": "string"}),
    )
    .expect("valid declared tool schemas")
}

/// A tool whose attempt makes one direct completion on the session's model.
/// Its first attempt retires the key first, and it reports a completion that
/// failed as its own failed result, as a tool that swallows the error would.
struct AskModel {
    catalog: Arc<LiveCatalog>,
    retired: AtomicBool,
    settled: Arc<Mutex<Vec<Result<String, String>>>>,
}

#[async_trait::async_trait]
impl lash::tools::ToolProvider for AskModel {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        vec![ask_model_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        (name == ASK_MODEL).then(|| Arc::new(ask_model_definition().contract()))
    }

    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        if !self.retired.swap(true, Ordering::SeqCst) {
            self.catalog.serve(LlmProfileRegistry::new());
        }
        let completed = call
            .context
            .direct_completions()
            .complete(
                lash::direct::DirectRequest::text("a direct question"),
                "ask-model",
            )
            .await;
        match completed {
            Ok(completion) => {
                self.settled
                    .lock()
                    .expect("settled attempts")
                    .push(Ok(completion.text.clone()));
                lash::tools::ToolOutcome::ok(serde_json::json!(completion.text)).into()
            }
            Err(error) => {
                self.settled
                    .lock()
                    .expect("settled attempts")
                    .push(Err(error.to_string()));
                lash::tools::ToolOutcome::err_fmt(format!("the direct completion failed: {error}"))
                    .into()
            }
        }
    }
}

/// A direct completion follows the model call's rule. A tool attempt whose
/// direct completion cannot bind the session's recorded model ends with the
/// bind fault, whatever the tool made of the error: no tool result is
/// journaled, and the engine stops retrying the attempt's group child after
/// its attempts.
///
/// The paused child is parked like any other stopped work (FIG-4607): the
/// root that waits for it holds a park with the typed cause, the park surface
/// lists it, and a later reconcile pass over the same paused child writes
/// nothing. Once the key is served again the operator's redrive of that park
/// resumes the child: the attempt completes, the root answers, the park ends,
/// and the model is shown the completion, never the fault.
async fn a_direct_completion_bind_fault_seals_nothing_and_recovers_after_the_park(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-direct-bind-fault";
    // The session's transport, by call: the root's first model call asks for
    // the tool, the tool's direct completion answers it, and the root's
    // second model call answers with what it was shown of the tool.
    let calls = Arc::new(AtomicUsize::new(0));
    let shown = Arc::new(Mutex::new(Vec::<String>::new()));
    let provider = || {
        let calls = Arc::clone(&calls);
        let shown = Arc::clone(&shown);
        lash::testing::TestProvider::builder()
            .kind(KIND)
            .complete(move |request| {
                let response = match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "ask-1".to_string(),
                            tool_name: ASK_MODEL.to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    },
                    1 => text("the direct answer"),
                    _ => {
                        shown
                            .lock()
                            .expect("shown requests")
                            .push(format!("{:?}", request.messages));
                        text("kimi answers")
                    }
                };
                async move { Ok(response) }
            })
            .build()
            .into_handle()
    };
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", provider()));
    let settled = Arc::new(Mutex::new(Vec::new()));
    let tools: Arc<dyn lash::plugins::PluginFactory> =
        Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("keys-ask-model"),
            lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(AskModel {
                catalog: Arc::clone(&catalog),
                retired: AtomicBool::new(false),
                settled: Arc::clone(&settled),
            })),
        ));
    let core = core_over(&double, &catalog, vec![tools]);
    let session = created_on(&core, session_id, KIMI).await;
    session
        .send(TurnInput::text("ask the model through the tool"))
        .id("keys-direct-bind-fault-root")
        .await
        .expect("the session accepts the input");

    let parked = await_parked_on(&double, KIMI).await;
    let (_, failure) = parked
        .last_failure
        .clone()
        .expect("the park's last failure");
    assert!(
        failure.contains("llm_profile_unavailable") && failure.contains(KIMI),
        "the park's failure is the typed bind fault and names the recorded key: {failure}"
    );
    assert_no_recorded_bind_fault(&double, &parked);
    // The tool's group child is the invocation the engine stopped retrying.
    // Its failure carries the fault's typed record, which is what a park of
    // exhausted retries decodes its model key from.
    let fault = lash_core::RuntimeEffectControllerError::in_text(&failure);
    assert_eq!(
        fault
            .as_ref()
            .and_then(|fault| fault.profile_key())
            .map(|key| key.as_str()),
        Some(KIMI),
        "the parked attempt's failure carries the unbindable key typed: {failure}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the root's first model call reached the transport"
    );

    // The root's own run is not stopped, it waits for the child: the child's
    // pause is what parks the root.
    let dispatch = double.double.service_name("EffectGroupDispatch");
    assert!(
        parked.target.starts_with(&dispatch) && parked.target.ends_with("/child"),
        "the engine stopped the tool's group child: {parked:?}"
    );
    let root = lash::TurnId::from("keys-direct-bind-fault-root");
    let work = lash::ParkedWorkRef::Turn {
        session_id: lash::SessionId::from(session_id),
        turn_id: root.clone(),
    };
    let child = lash_core::engine::ParkTarget::RootChild {
        session: lash::SessionId::from(session_id),
        root,
    };
    let first = reconcile_pass(&double).await;
    assert!(
        first.parked == vec![child] || (first.parked.is_empty() && first.attached == 1),
        "a reconcile pass parks the paused child's root, unless the recovery interval already \
         did: {first:?}"
    );
    let limit = std::num::NonZeroUsize::new(8).expect("non-zero");
    let records = core
        .parked_work()
        .list(&lash::ParkedWorkQuery::all(limit))
        .await
        .expect("the park surface lists parked work")
        .records;
    let [park] = records.as_slice() else {
        panic!("the park surface lists the root the paused child parked: {records:?}");
    };
    assert_eq!(park.target, work, "the park names the waiting root");
    assert_eq!(park.attempts, 1, "one park, written once: {park:?}");
    assert_eq!(
        park.reason.code(),
        lash::persistence::ParkReasonCode::EngineRetryExhausted,
        "the root parked on its child's exhausted retries: {park:?}"
    );
    assert_eq!(
        park.reason.profile_key(),
        Some(&LlmProfileKey::new(KIMI)),
        "the root's park carries the unbindable key typed: {park:?}"
    );
    let listed = listed_parks(&core).await;
    let again = reconcile_pass(&double).await;
    assert!(
        again.parked.is_empty() && again.attached == 1,
        "a later pass finds the child's root already parked: {again:?}"
    );
    assert_eq!(
        listed_parks(&core).await,
        listed,
        "a later pass over the same paused child writes nothing"
    );

    catalog.serve(registry_of(KIMI, "kimi-k3", provider()));
    core.parked_work()
        .redrive(&work, park.park_id)
        .await
        .expect("the operator redrives the parked root");
    assert_eq!(
        answer_after_redrive(&session, "keys-direct-bind-fault-root").await,
        "kimi answers",
        "the redriven root completes once the key is served"
    );
    assert_eq!(
        double
            .double
            .server()
            .invocations()
            .iter()
            .find(|view| view.id == parked.id)
            .map(|view| view.status),
        Some("completed"),
        "the redrive resumed the paused child, which ran to its end"
    );
    assert_eq!(
        listed_parks(&core).await,
        Vec::new(),
        "the root's commit ends its park"
    );
    assert_eq!(
        settled.lock().expect("settled attempts").last(),
        Some(&Ok("the direct answer".to_string())),
        "the resumed attempt's direct completion ran"
    );
    let shown = shown.lock().expect("shown requests").join("\n");
    assert!(
        shown.contains("the direct answer") && !shown.contains("the direct completion failed"),
        "the model is shown the completion and never the bind fault: {shown}"
    );
}

const ASK_TWICE: &str = "ask_twice";

fn ask_twice_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:ask_twice",
        ASK_TWICE,
        "Ask the session's model two questions through direct completions.",
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false}),
        serde_json::json!({"type": "string"}),
    )
    .expect("valid declared tool schemas")
}

/// Where an [`AskTwice`] attempt retires the session's key, once.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retire {
    BeforeTheFirstCompletion,
    BetweenTheCompletions,
}

/// A tool whose attempt makes two direct completions on the session's model,
/// `ask-first` then `ask-second`. It catches a completion that failed, acts on
/// it (the side effect `acted_past_a_fault` counts) and reports its own failed
/// result, as a tool that swallows the error would.
struct AskTwice {
    catalog: Arc<LiveCatalog>,
    retire: Retire,
    retired: AtomicBool,
    entered: Arc<AtomicUsize>,
    acted_past_a_fault: Arc<AtomicUsize>,
}

impl AskTwice {
    fn retire_at(&self, point: Retire) {
        if self.retire == point && !self.retired.swap(true, Ordering::SeqCst) {
            self.catalog.serve(LlmProfileRegistry::new());
        }
    }

    async fn ask(&self, call: &lash::tools::ToolCall<'_>, source: &str) -> Result<String, String> {
        let completed = call
            .context
            .direct_completions()
            .complete(
                lash::direct::DirectRequest::text("a direct question"),
                source,
            )
            .await;
        match completed {
            Ok(completion) => Ok(completion.text.clone()),
            Err(error) => {
                self.acted_past_a_fault.fetch_add(1, Ordering::SeqCst);
                Err(format!("{source} failed: {error}"))
            }
        }
    }
}

#[async_trait::async_trait]
impl lash::tools::ToolProvider for AskTwice {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        vec![ask_twice_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        (name == ASK_TWICE).then(|| Arc::new(ask_twice_definition().contract()))
    }

    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        self.entered.fetch_add(1, Ordering::SeqCst);
        self.retire_at(Retire::BeforeTheFirstCompletion);
        let first = match self.ask(&call, "ask-first").await {
            Ok(first) => first,
            Err(failed) => return lash::tools::ToolOutcome::err_fmt(failed).into(),
        };
        self.retire_at(Retire::BetweenTheCompletions);
        match self.ask(&call, "ask-second").await {
            Ok(second) => {
                lash::tools::ToolOutcome::ok(serde_json::json!(format!("{first} {second}"))).into()
            }
            Err(failed) => lash::tools::ToolOutcome::err_fmt(failed).into(),
        }
    }
}

/// A transport for an [`AskTwice`] session, counting its calls in `calls`:
/// the root's first model call asks for the tool, the next `completions`
/// calls answer direct completions, and every later call answers the root.
fn ask_twice_provider(calls: &Arc<AtomicUsize>, completions: usize) -> ProviderHandle {
    let calls = Arc::clone(calls);
    lash::testing::TestProvider::builder()
        .kind(KIND)
        .complete(move |_| {
            let response = match calls.fetch_add(1, Ordering::SeqCst) {
                0 => LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "ask-1".to_string(),
                        tool_name: ASK_TWICE.to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                },
                call if call <= completions => text("a direct answer"),
                _ => text("kimi answers"),
            };
            async move { Ok(response) }
        })
        .build()
        .into_handle()
}

/// The session's usage ledger: its facts and its runs.
async fn ledger(
    core: &LashCore,
    session_id: &str,
) -> (
    Vec<lash_core::UsageFactRecord>,
    Vec<lash_core::UsageRunRecord>,
) {
    let owner = lash_core::RuntimeOwner::Session(lash_core::SessionId::fixture(session_id));
    let limit = std::num::NonZeroU32::new(64).expect("non-zero");
    let facts = core
        .usage_fact_page(&owner, None, limit)
        .await
        .expect("the ledger lists the session's facts");
    let runs = core
        .usage_run_page(&owner, lash_core::UsageRunFilter::All, None, limit)
        .await
        .expect("the ledger lists the session's runs");
    assert!(
        facts.next.is_none() && runs.next.is_none(),
        "one page holds the session's ledger"
    );
    (facts.facts, runs.runs)
}

/// Wait until the session's ledger holds exactly `first` facts of
/// `ask-first` and `second` of `ask-second`, each from its own run, and every
/// run of the session is settled. A settlement is delivered after its effect,
/// so the ledger is read until it agrees.
async fn await_settled_asks(core: &LashCore, session_id: &str, first: usize, second: usize) {
    let agreed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let (facts, runs) = ledger(core, session_id).await;
            let of = |source: &str| {
                facts
                    .iter()
                    .filter(|fact| fact.source == source)
                    .map(|fact| fact.run().cloned())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
            };
            let asks = facts
                .iter()
                .filter(|fact| fact.source.starts_with("ask-"))
                .count();
            if of("ask-first") == first
                && of("ask-second") == second
                && asks == first + second
                && runs.iter().all(|run| run.state.is_settled())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    if agreed.is_err() {
        let (facts, runs) = ledger(core, session_id).await;
        panic!(
            "the ledger holds {first} ask-first and {second} ask-second facts, each settled \
             once, and no run is open, unknown or conflicted; facts {facts:#?}, runs {runs:#?}"
        );
    }
}

/// A bind fault inside a tool attempt ends the attempt where it is met
/// (FIG-4632). The direct completion that cannot bind the session's recorded
/// model hands its tool no error to catch: the tool's body is dropped there,
/// so what the tool would do with the failure never runs, on the first
/// attempt or on any retry of it. Before, every retry ran the tool to its
/// end and repeated that side effect.
async fn a_tool_is_not_run_past_a_bind_fault_of_its_direct_completion(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", ask_twice_provider(&calls, 0)));
    let entered = Arc::new(AtomicUsize::new(0));
    let acted_past_a_fault = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn lash::plugins::PluginFactory> =
        Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("keys-ask-twice"),
            lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(AskTwice {
                catalog: Arc::clone(&catalog),
                retire: Retire::BeforeTheFirstCompletion,
                retired: AtomicBool::new(false),
                entered: Arc::clone(&entered),
                acted_past_a_fault: Arc::clone(&acted_past_a_fault),
            })),
        ));
    let core = core_over(&double, &catalog, vec![tools]);
    let session = created_on(&core, "keys-attempt-ends-at-fault", KIMI).await;
    session
        .send(TurnInput::text("ask the model through the tool"))
        .id("keys-attempt-ends-at-fault-root")
        .await
        .expect("the session accepts the input");

    let parked = await_parked_on(&double, KIMI).await;
    assert_no_recorded_bind_fault(&double, &parked);
    assert!(
        entered.load(Ordering::SeqCst) > 1,
        "the engine retried the attempt before it stopped: {parked:?}"
    );
    assert_eq!(
        acted_past_a_fault.load(Ordering::SeqCst),
        0,
        "no attempt ran its tool past the bind fault, of {} attempts",
        entered.load(Ordering::SeqCst)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the root's model call reached the transport"
    );
}

/// A completion a tool attempt dispatched before a later completion's bind
/// fault keeps its usage (FIG-4632). The fault ends the attempt and nothing
/// journals it, so the usage of the completion already dispatched and billed
/// is settled when the attempt ends, under the run that spent it: its run is
/// not left an unknown liability. The retry that completes once the key is
/// served dispatches both completions under its own run, and settles each
/// once without conflicting with the earlier run's fact.
async fn a_completion_dispatched_before_a_bind_fault_keeps_its_usage_settled_once(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-usage-before-fault";
    let calls = Arc::new(AtomicUsize::new(0));
    // Three direct completions reach the transport: the first attempt's
    // first, and both of the attempt that completes.
    let provider = || ask_twice_provider(&calls, 3);
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", provider()));
    let entered = Arc::new(AtomicUsize::new(0));
    let acted_past_a_fault = Arc::new(AtomicUsize::new(0));
    let tools: Arc<dyn lash::plugins::PluginFactory> =
        Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("keys-ask-twice"),
            lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(AskTwice {
                catalog: Arc::clone(&catalog),
                retire: Retire::BetweenTheCompletions,
                retired: AtomicBool::new(false),
                entered: Arc::clone(&entered),
                acted_past_a_fault: Arc::clone(&acted_past_a_fault),
            })),
        ));
    let core = core_over(&double, &catalog, vec![tools]);
    let session = created_on(&core, session_id, KIMI).await;
    session
        .send(TurnInput::text("ask the model through the tool"))
        .id("keys-usage-before-fault-root")
        .await
        .expect("the session accepts the input");

    let parked = await_parked_on(&double, KIMI).await;
    assert_no_recorded_bind_fault(&double, &parked);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the root's model call and the first attempt's first completion reached the transport; \
         every retry met the bind fault at its first completion"
    );
    // The faulted attempt's dispatched completion is settled, and its run is
    // resolved, although the attempt journaled nothing.
    await_settled_asks(&core, session_id, 1, 0).await;

    let root = lash::TurnId::from("keys-usage-before-fault-root");
    let work = lash::ParkedWorkRef::Turn {
        session_id: lash::SessionId::from(session_id),
        turn_id: root,
    };
    reconcile_pass(&double).await;
    let limit = std::num::NonZeroUsize::new(8).expect("non-zero");
    let records = core
        .parked_work()
        .list(&lash::ParkedWorkQuery::all(limit))
        .await
        .expect("the park surface lists parked work")
        .records;
    let [park] = records.as_slice() else {
        panic!("the park surface lists the root the paused child parked: {records:?}");
    };
    catalog.serve(registry_of(KIMI, "kimi-k3", provider()));
    core.parked_work()
        .redrive(&work, park.park_id)
        .await
        .expect("the operator redrives the parked root");
    assert_eq!(
        answer_after_redrive(&session, "keys-usage-before-fault-root").await,
        "kimi answers",
        "the redriven root completes once the key is served"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "the resumed attempt dispatched both completions, and the root its second model call"
    );
    // Each dispatched completion is settled exactly once: the first
    // completion of the faulted attempt and of the completed one, under their
    // own runs, and the second completion of the completed one.
    await_settled_asks(&core, session_id, 2, 1).await;
    assert_eq!(
        acted_past_a_fault.load(Ordering::SeqCst),
        0,
        "no attempt ran its tool past the bind fault, of {} attempts",
        entered.load(Ordering::SeqCst)
    );
}

/// How long after a park is listed its sender's answer may still arrive: far
/// inside the second a follower's store poll backs off to.
const AT_THE_PARK_COMMIT: std::time::Duration = std::time::Duration::from_millis(250);

/// A sender awaiting its output learns of a park where it is recorded
/// (FIG-4618). The root of a paused group child is parked while its own run
/// still waits for the child in this process, holding the open session's
/// resident runtime. The handle answers Parked, with the typed cause, at the
/// park's commit and from the recorded state: it waits neither for the run to
/// release the runtime nor for its next store poll, which by then is a second
/// away. Before, it answered only once the redriven root had settled.
async fn a_send_answers_parked_at_the_park_commit_while_its_roots_run_is_resident(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let session_id = "keys-resident-park";
    let root = "keys-resident-park-root";
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = || {
        let calls = Arc::clone(&calls);
        lash::testing::TestProvider::builder()
            .kind(KIND)
            .complete(move |_| {
                let response = match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "ask-1".to_string(),
                            tool_name: ASK_MODEL.to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    },
                    1 => text("the direct answer"),
                    _ => text("kimi answers"),
                };
                async move { Ok(response) }
            })
            .build()
            .into_handle()
    };
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", provider()));
    let tools: Arc<dyn lash::plugins::PluginFactory> =
        Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("keys-ask-model"),
            lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(AskModel {
                catalog: Arc::clone(&catalog),
                retired: AtomicBool::new(false),
                settled: Arc::new(Mutex::new(Vec::new())),
            })),
        ));
    let core = core_over(&double, &catalog, vec![tools]);
    let session = created_on(&core, session_id, KIMI).await;
    let handle = session
        .send(TurnInput::text("ask the model through the tool"))
        .id(root)
        .await
        .expect("the session accepts the input");
    // The sender awaits its output from the start, so its follower is at
    // rest, its store poll backed off, when the park is recorded.
    let sender = tokio::spawn(async move {
        let answer = handle.output().await;
        (answer, std::time::Instant::now())
    });

    let paused = await_parked_on(&double, KIMI).await;
    let dispatch = double.double.service_name("EffectGroupDispatch");
    assert!(
        paused.target.starts_with(&dispatch) && paused.target.ends_with("/child"),
        "the engine stopped the tool's group child: {paused:?}"
    );

    // The park is recorded by a reconcile pass, or by the recovery interval
    // ahead of it; either way it is listed no earlier than its commit.
    let listed = async {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if !listed_parks(&core).await.is_empty() {
                    return std::time::Instant::now();
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the paused child's root is parked")
    };
    let (_, listed) = tokio::join!(reconcile_pass(&double), listed);

    let (answer, answered) = tokio::time::timeout(std::time::Duration::from_secs(30), sender)
        .await
        .expect("the sender is answered while its root's run still holds the resident runtime")
        .expect("the sender's task completes");
    let status = match answer {
        Err(lash::EmbedError::Send(error)) => match *error {
            lash::SendError::NotSettled { status, .. } => status,
            other => panic!("the root is parked, got: {other:?}"),
        },
        other => panic!("the root is parked, got: {other:?}"),
    };
    let lash::TurnStatus::Parked(parked) = status else {
        panic!("the root is parked, got: {status:?}");
    };
    assert_eq!(parked.root, lash::TurnId::from(root));
    assert_eq!(
        parked.reason.code(),
        lash::persistence::ParkReasonCode::EngineRetryExhausted,
        "the answer carries the park's typed cause: {parked:?}"
    );
    assert_eq!(
        parked.reason.profile_key(),
        Some(&LlmProfileKey::new(KIMI)),
        "the answer carries the unbindable key typed: {parked:?}"
    );
    let after = answered.saturating_duration_since(listed);
    assert!(
        after < AT_THE_PARK_COMMIT,
        "the park's commit answers the sender, not its next store poll: answered {after:?} \
         after the park was listed"
    );
    // Nothing moved the root meanwhile: its child is still paused, and its
    // run still waits for it.
    assert_eq!(
        double
            .double
            .server()
            .invocations()
            .iter()
            .find(|view| view.id == paused.id)
            .map(|view| view.status),
        Some("paused"),
        "the answer came while the child was still paused"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the root made no further model call before the answer"
    );

    // The park's redrive still completes the root.
    let [(work, park_id, _, _)] = listed_parks(&core).await.try_into().expect("one park");
    catalog.serve(registry_of(KIMI, "kimi-k3", provider()));
    core.parked_work()
        .redrive(&work, park_id)
        .await
        .expect("the operator redrives the parked root");
    assert_eq!(
        answer_after_redrive(&session, root).await,
        "kimi answers",
        "the redriven root completes once the key is served"
    );
}

/// A host refuses a child's key with unsupported inherited reasoning before
/// it publishes an environment, acquires a referrer or registers a process.
/// The request's policy takes precedence over the captured environment.
async fn a_host_process_start_refuses_unsupported_inherited_reasoning_before_recording(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    const THINKER: &str = "host-thinker";
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let route = Route::new("child answers");
    let high = lash::provider::ReasoningSelection::Effort("high".to_string());
    let thinking = LlmProfileMetadata::builder("thinker")
        .context_window_tokens(64_000)
        .capability(lash::provider::LlmProfileCapability {
            reasoning: Some(lash::provider::ReasoningCapability {
                efforts: vec!["high".to_string()],
                encoding: lash::provider::ReasoningEncoding::Effort,
                disable: false,
                mandatory: false,
            }),
            ..lash::provider::LlmProfileCapability::default()
        })
        .build()
        .expect("thinking model metadata");
    let registry = Arc::new(
        LlmProfileRegistry::new()
            .register(THINKER, RegisteredLlmProfile::new(thinking, route.handle()))
            .and_then(|registry| {
                registry.register(
                    GLM,
                    RegisteredLlmProfile::new(metadata("plain", "r1"), route.handle()),
                )
            })
            .expect("registered host models"),
    );
    let policy = lash::runtime::SessionPolicy {
        model: Some(
            lash::LlmProfileConfig::new(
                lash::LlmProfiles::snapshot(registry.as_ref(), &LlmProfileKey::new(THINKER))
                    .expect("the thinking key resolves"),
            )
            .with_reasoning(high),
        ),
        ..lash::runtime::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )
    };
    let host = LashCore::standard_builder(double.double.lash_backend())
        .llm_profiles(registry)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "host-reasoning",
            "boot",
        ))
        .expect("the host core builds");
    let environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        policy.clone(),
    );
    let claim = lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
        lash_core::ExecutionScope::runtime_operation("host-reasoning-fixture")
            .journal_identity()
            .expect("fixture journal"),
    ));
    let env_ref = lash_core::publish_process_execution_env(
        double.double.stores().process_env_store().as_ref(),
        &claim,
        &environment,
    )
    .await
    .expect("publish the captured fixture environment");

    for source in ["policy", "environment"] {
        let key = format!("host-reasoning-{source}");
        let mut request = unstated_session_turn_start(&key, "run the child");
        let lash_core::ProcessStartTarget::Input(lash_core::ProcessInput::SessionTurn {
            create_request,
            ..
        }) = &mut request.input
        else {
            panic!("session turn fixture");
        };
        create_request.model = Some(LlmProfileKey::new(GLM));
        if source == "policy" {
            create_request.policy = Some(policy.clone());
        } else {
            request = request.with_env_ref(env_ref.clone());
        }
        let before = double.artifacts.counts().await;
        let refused = start_on(&double, &host, &key, request).await;
        match refused {
            Err(lash::EmbedError::Plugin(lash_core::PluginError::RuntimeEffectController(
                error,
            ))) => {
                assert_eq!(
                    error.code,
                    lash::runtime::RuntimeErrorCode::ReasoningRefused
                );
            }
            other => panic!("the host call refuses inherited reasoning typed: {other:?}"),
        }
        assert_eq!(
            double.artifacts.counts().await,
            before,
            "the refused start wrote no artifact, edge, cleanup obligation or fence"
        );
        assert!(
            host.processes()
                .list(&lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..lash_core::ProcessListFilter::default()
                })
                .await
                .expect("list every process")
                .is_empty(),
            "the refused call registered no process"
        );
        assert!(matches!(
            host.session(lash_core::SessionId::fixture(format!("{key}-child")))
                .open()
                .await,
            Err(lash::EmbedError::UnknownSession { .. })
        ));
    }
    assert_eq!(route.calls(), 0, "no refused child called a provider");

    // An explicit policy overrides the environment's inherited effort.
    let mut accepted = unstated_session_turn_start("host-reasoning-environment", "run the child")
        .with_env_ref(env_ref);
    let lash_core::ProcessStartTarget::Input(lash_core::ProcessInput::SessionTurn {
        create_request,
        ..
    }) = &mut accepted.input
    else {
        panic!("session turn fixture");
    };
    create_request.model = Some(LlmProfileKey::new(GLM));
    create_request.policy = Some(lash::runtime::SessionPolicy::new(
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    ));
    let first = start_on(&double, &host, "host-reasoning-repaired", accepted.clone())
        .await
        .expect("compatible reasoning starts under the refused key");
    let moved = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "another-model",
            revision: "r2",
            route: &route,
        }],
        "host-reasoning-moved",
    );
    let retry = start_on(&double, &moved, "host-reasoning-retry", accepted)
        .await
        .expect("a retained start never revalidates its removed child key");
    assert_eq!(retry.process_id, first.process_id);
    assert_eq!(
        retry.disposition,
        lash_core::ProcessRegistrationOutcome::Existing
    );
}

// ---- registration -----------------------------------------------------------

#[test]
fn postgres_variants_never_pass_without_a_database_url() {
    let executable = std::env::current_exe().expect("model-keys test executable");
    for variant in ["postgres", "postgres_always_replay"] {
        let law = format!("two_keys_sharing_a_provider_kind_select_their_own_transport::{variant}");
        for url in [None, Some(""), Some(" \t ")] {
            let mut command = std::process::Command::new(&executable);
            command
                .args(["--exact", &law, "--include-ignored", "--nocapture"])
                .env_remove("LASH_POSTGRES_DATABASE_URL");
            if let Some(url) = url {
                command.env("LASH_POSTGRES_DATABASE_URL", url);
            }
            let output = command.output().expect("run the PostgreSQL variant");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stdout.contains("running 1 test"),
                "the selected variant executes: {stdout}\n{stderr}"
            );
            assert!(
                !output.status.success() && stdout.contains("0 passed; 1 failed"),
                "{law} with URL {url:?} must fail instead of passing vacuously: {stdout}\n{stderr}"
            );
            assert!(
                stderr.contains("LASH_POSTGRES_DATABASE_URL"),
                "the failure names the missing service configuration: {stdout}\n{stderr}"
            );
        }
    }
}

macro_rules! tiered {
    ($law:ident, $seed:literal) => {
        mod $law {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite_memory() {
                super::$law(Tier::SqliteMemory, false, $seed + 1).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite_memory_always_replay() {
                super::$law(Tier::SqliteMemory, true, $seed + 2).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite_file() {
                super::$law(Tier::SqliteFile, false, $seed + 3).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn sqlite_file_always_replay() {
                super::$law(Tier::SqliteFile, true, $seed + 4).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
            async fn postgres() {
                super::$law(Tier::Postgres, false, $seed + 5).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
            async fn postgres_always_replay() {
                super::$law(Tier::Postgres, true, $seed + 6).await;
            }
        }
    };
}

tiered!(
    fig4669_child_parent_survives_every_runtime_reopen,
    0x4669_1000
);

tiered!(
    two_keys_sharing_a_provider_kind_select_their_own_transport,
    0x4374_1100
);
tiered!(
    a_catalog_edit_reaches_a_session_only_through_a_profile_change,
    0x4374_1200
);
tiered!(
    a_recorded_llm_profile_whose_key_left_the_catalog_fails_typed_and_never_falls_back,
    0x4374_1300
);
tiered!(
    a_recorded_model_whose_key_serves_another_wire_model_is_refused_typed,
    0x4374_1400
);
tiered!(
    an_unknown_key_is_refused_before_anything_changes,
    0x4374_1500
);
tiered!(
    a_session_turn_start_retried_after_the_host_changed_what_it_passes_keeps_its_retained_start,
    0x4594_1100
);
tiered!(
    an_unsupported_reasoning_selection_is_refused_where_it_is_stated,
    0x4531_1200
);
tiered!(
    a_replay_after_the_key_left_the_catalog_completes_with_zero_resolver_calls,
    0x4404_1100
);
tiered!(
    an_unjournaled_bind_fault_seals_nothing_and_recovers_after_the_park,
    0x4404_1200
);
tiered!(
    a_direct_completion_bind_fault_seals_nothing_and_recovers_after_the_park,
    0x4404_1300
);
tiered!(
    a_tool_is_not_run_past_a_bind_fault_of_its_direct_completion,
    0x4632_1100
);
tiered!(
    a_completion_dispatched_before_a_bind_fault_keeps_its_usage_settled_once,
    0x4632_1200
);
tiered!(
    a_send_answers_parked_at_the_park_commit_while_its_roots_run_is_resident,
    0x4618_1100
);
tiered!(
    a_host_process_start_refuses_unsupported_inherited_reasoning_before_recording,
    0x4603_1100
);

tiered!(
    a_process_opened_group_child_parks_resumes_and_reparks_idempotently,
    0x4617_1100
);

tiered!(a_paused_group_run_parks_its_root_opener, 0x4617_2100);
tiered!(a_paused_group_run_parks_its_process_opener, 0x4617_2200);
tiered!(a_paused_group_retire_parks_its_root_opener, 0x4617_2300);
tiered!(a_paused_group_retire_parks_its_process_opener, 0x4617_2400);
