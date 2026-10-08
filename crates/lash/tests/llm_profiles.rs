//! Host model keys (FIG-4374): a host registers opaque model keys in a
//! `LlmProfileRegistry`, several of them served by transports that share one
//! provider kind. A session records the binding its key minted at creation
//! and at every model change; a run executes the recorded binding, or ends its
//! attempt with the typed `LlmProfileUnavailable`, and never falls back to another
//! registration or to today's catalog entry.
//!
//! A recorded model is bound lazily (FIG-4404): only the body of an
//! unrecorded model call asks the host's models, so work after a recorded
//! call completes on a deployment that retired the key, and a bind fault is
//! the attempt's, retried and never a recorded result: the session parks
//! after its activation budget of failed passes.
//!
//! The host code below uses `lash::` paths only; the store tiers beneath the
//! core's node are the test's own infrastructure (ported by FIG-5307 onto
//! the durable node substrate).

#![cfg(all(feature = "sqlite", feature = "testing"))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "acceptance laws establish each step's result"
)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[path = "llm_profiles/session_turn_starts.rs"]
mod session_turn_starts;
use session_turn_starts::a_session_turn_start_retried_after_the_host_changed_what_it_passes_keeps_its_retained_start;

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

/// One tier's store set, which every core a law builds runs over.
struct Double {
    stores: Arc<dyn lash::StoreSet>,
    _keep: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

impl Double {
    /// A backend over the tier's stores: each core gets its own, as each
    /// build of a deployment does.
    fn backend(&self) -> lash::Backend {
        lash::durable::DurableBackendBuilder::new(Arc::clone(&self.stores))
            .build()
            .expect("the durable backend builds")
    }

    /// The session actor's park, once the session parked: its reason.
    async fn parked(&self, session_id: &str) -> String {
        let actor =
            lash_core::durable_port::ActorKey::session(session_id).expect("a session actor key");
        let durable = Arc::clone(self.backend().durable());
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            loop {
                if let Ok(Some(snapshot)) = durable.actor(&actor).await
                    && snapshot.state == lash_core::durable_port::ActorState::Parked
                {
                    return snapshot.park.unwrap_or_default();
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the session `{session_id}` parks on its bind fault"))
    }
}

async fn double(tier: Tier) -> Option<Double> {
    match tier {
        Tier::SqliteMemory => {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite memory stores");
            Some(Double {
                stores: Arc::new(stores),
                _keep: Vec::new(),
            })
        }
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let path = root.path().join("lash.db");
            let stores = lash_sqlite_store::SqliteStoreSet::open(
                &path,
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .expect("SQLite file stores");
            Some(Double {
                stores: Arc::new(stores),
                _keep: vec![Box::new(root)],
            })
        }
        Tier::Postgres => {
            let url = lash_postgres_store::testing::required_database_url();
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(database.url())
                .await
                .expect("open provisioned PostgreSQL storage");
            let attachments = tempfile::tempdir().expect("attachment directory");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                lash_sqlite_store::SqliteStoreSet::open(
                    (attachments.path()).join("attachments.db"),
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await
                .expect("SQLite attachment store")
                .attachment_store(),
            );
            Some(Double {
                stores: Arc::new(stores),
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
        .cache_retention(lash::provider::CacheRetention::Short)
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

/// A core over `entries`, serving its sessions on its node.
fn core(double: &Double, entries: &[Entry<'_>], worker: &str) -> LashCore {
    core_serving(double, entries, worker, true)
}

/// A core over `entries` that accepts its sessions' input and runs none of
/// it: the input stays pending for the deployment's next build.
fn held_core(double: &Double, entries: &[Entry<'_>], worker: &str) -> LashCore {
    core_serving(double, entries, worker, false)
}

fn core_serving(double: &Double, entries: &[Entry<'_>], worker: &str, serve: bool) -> LashCore {
    LashCore::standard_builder(double.backend())
        .serve_sessions(serve)
        .llm_profiles(catalog(entries))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "model-keys-worker",
            worker,
        ))
        .expect("the host core builds")
}

async fn created_on(core: &LashCore, id: &str, key: &str) -> lash::LashSession {
    core.session(lash::SessionId::parse(id).expect("nonblank host identity"))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                key,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
        .expect("create the session");
    core.session(lash::SessionId::parse(id).expect("nonblank host identity"))
        .open()
        .await
        .expect("open the session")
}

async fn answer_of(handle: lash::SendHandle) -> String {
    let output = handle.output().await.expect("the run settles");
    assert!(output.is_success(), "the run answers: {output:?}");
    output
        .assistant_message()
        .expect("the run answers with text")
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
    let mut builder = LashCore::standard_builder(double.backend())
        .llm_profiles(Arc::clone(catalog) as Arc<dyn lash::LlmProfiles>)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended());
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

/// The park the follower of `run` is told its run is in.
async fn park_of(session: &lash::LashSession, run: &str) -> lash::ParkedTurn {
    let settled = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .attach_id(lash::TurnId::parse(run).expect("nonblank host identity"))
            .output(),
    )
    .await
    .unwrap_or_else(|_| panic!("the follower of the parked run `{run}` is answered"));
    let status = match settled {
        Err(lash::EmbedError::Send(error)) => match *error {
            lash::SendError::NotSettled { status, .. } => status,
            other => panic!("the run is parked, got: {other:?}"),
        },
        other => panic!("the run is parked, got: {other:?}"),
    };
    let lash::TurnStatus::Parked(parked) = status else {
        panic!("the run is parked, got: {status:?}");
    };
    parked
}

// ---- the laws ---------------------------------------------------------------

/// Two keys served by transports of one provider kind: each send's model
/// key selects its own transport, a send without one runs the session's
/// recorded model, and a per-run key never changes the session's record.
async fn two_keys_sharing_a_provider_kind_select_their_own_transport(tier: Tier) {
    let Some(double) = double(tier).await else {
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
        "the session's view never shows a settled run's per-run key"
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
/// own: the session's runs keep sending the metadata it recorded, on a
/// redeployed transport too. Changing the model to the same key re-mints
/// the binding, and the session's next run sends the edited metadata.
async fn a_catalog_edit_reaches_a_session_only_through_a_profile_change(tier: Tier) {
    let Some(double) = double(tier).await else {
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
    first.shutdown().await.expect("the first build shuts down");

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
        .session(lash::SessionId::parse(session_id).expect("nonblank host identity"))
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
        "the run sends the metadata the session recorded, not the edited catalog's"
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
        .expect("config accepted")
        .await_outcome(&config)
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
        "after the model change the run sends the re-minted metadata"
    );
    assert_eq!(first_kimi.calls(), 0);
}

/// A session whose recorded key left the catalog ends each attempt with the
/// typed `LlmProfileUnavailable` and calls no other registration, the new default
/// included: the session parks on the typed fault. (A deployment that
/// registers the key again runs the run there once it is redriven:
/// [`an_unjournaled_bind_fault_seals_nothing_and_recovers_after_the_park`].)
async fn a_recorded_llm_profile_whose_key_left_the_catalog_fails_typed_and_never_falls_back(
    tier: Tier,
) {
    let Some(double) = double(tier).await else {
        return;
    };
    let session_id = "keys-key-removed";
    let glm = Route::new("glm answers");
    let kimi = Route::new("old kimi answers");
    let first = held_core(
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
    session
        .send(TurnInput::text("ask the session's model"))
        .id(lash::TurnId::parse("keys-key-removed-run").expect("nonblank host identity"))
        .await
        .expect("the held build accepts the input");
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
    let failure = double.parked(session_id).await;
    assert!(
        failure.contains(KIMI),
        "the refusal names the recorded key: {failure}"
    );
    assert_eq!(glm.calls(), 0, "no other registration is substituted");
    assert_eq!(kimi.calls(), 0);

    assert!(
        failure.contains("llm_profile_unavailable"),
        "the park's failure is the typed bind fault: {failure}"
    );
    unserving.shutdown().await.expect("the build shuts down");
}

/// A key that now names another wire model cannot serve a session that
/// recorded the old one: every attempt ends with the typed
/// `LlmProfileUnavailable`, and the transport is never sent the request.
async fn a_recorded_model_whose_key_serves_another_wire_model_is_refused_typed(tier: Tier) {
    let Some(double) = double(tier).await else {
        return;
    };
    let session_id = "keys-wire-model-changed";
    let kimi = Route::new("kimi answers");
    let first = held_core(
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
    session
        .send(TurnInput::text("ask the session's model"))
        .id(lash::TurnId::parse("keys-wire-model-changed-run").expect("nonblank host identity"))
        .await
        .expect("the held build accepts the input");
    drop(session);
    drop(first);

    let newer_kimi = Route::new("newer kimi answers");
    let changed = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "kimi-k4",
            revision: "r2",
            route: &newer_kimi,
        }],
        "keys-boot-2",
    );
    let failure = double.parked(session_id).await;
    assert!(
        failure.contains("kimi-k4") && failure.contains("kimi-k3"),
        "the refusal names the served and the recorded wire models: {failure}"
    );
    assert_eq!(newer_kimi.calls(), 0, "the transport never saw the request");
    assert_eq!(kimi.calls(), 0);
    changed.shutdown().await.expect("the build shuts down");
}

/// A key the catalog does not register is refused typed and nothing is
/// written: at creation and at send before anything is accepted, and at a
/// model change when its transaction resolves, where the session keeps its
/// recorded model and its config revision.
async fn an_unknown_key_is_refused_before_anything_changes(tier: Tier) {
    let Some(double) = double(tier).await else {
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
        .session(lash::SessionId::parse("keys-unknown-create").expect("nonblank host identity"))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                unregistered,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await;
    assert!(
        matches!(&created, Err(lash::EmbedError::LlmProfileUnknown(error)) if error.key.as_str() == unregistered),
        "creation refuses the unknown key typed: {:?}",
        created.as_ref().err()
    );
    let reopened = core
        .session(lash::SessionId::parse("keys-unknown-create").expect("nonblank host identity"))
        .open()
        .await;
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
        .expect("config accepted")
        .await_outcome(&config)
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
async fn an_unsupported_reasoning_selection_is_refused_where_it_is_stated(tier: Tier) {
    const THINKER: &str = "thinker@tensorx";
    let Some(double) = double(tier).await else {
        return;
    };
    let thinker = Route::new("thinker answers");
    let glm = Route::new("glm answers");
    let high = lash::provider::ReasoningSelection::Effort("high".to_string());
    let thinking = LlmProfileMetadata::builder("thinker-1")
        .cache_retention(lash::provider::CacheRetention::Short)
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
    let core = LashCore::standard_builder(double.backend())
        .llm_profiles(Arc::new(registry))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "model-keys-worker",
            "keys-reasoning",
        ))
        .expect("the host core builds");

    // Creation: a key with no reasoning controls cannot record an effort.
    let created = core
        .session(lash::SessionId::parse("keys-reasoning-refused").expect("nonblank host identity"))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                GLM,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12))
            .reasoning(high.clone()),
        ))
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
    let reopened = core
        .session(lash::SessionId::parse("keys-reasoning-refused").expect("nonblank host identity"))
        .open()
        .await;
    assert!(
        matches!(&reopened, Err(lash::EmbedError::UnknownSession { .. })),
        "the refused creation created nothing: {:?}",
        reopened.as_ref().err()
    );

    // Send: the session records `high` on the thinking model.
    core.session(lash::SessionId::parse("keys-reasoning").expect("nonblank host identity"))
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                THINKER,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12))
            .reasoning(high.clone()),
        ))
        .await
        .expect("the advertised effort is recorded");
    let session = core
        .session(lash::SessionId::parse("keys-reasoning").expect("nonblank host identity"))
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

/// A run whose model call is already recorded completes on a deployment
/// that retired its key: the work after the recorded call never asks the
/// deployment's models for anything.
async fn a_replay_after_the_key_left_the_catalog_completes_with_zero_resolver_calls(tier: Tier) {
    let Some(double) = double(tier).await else {
        return;
    };
    let catalog = Arc::new(LiveCatalog::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let dispatched = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = {
        // The key is retired while its one model call is in flight: the
        // call's result is journaled, and everything after it is a replay
        // or runs on the deployment without the key.
        let retiring = Arc::clone(&catalog);
        let calls = Arc::clone(&calls);
        let dispatched = Arc::clone(&dispatched);
        lash::testing::TestProvider::builder()
            .kind(KIND)
            .generation_retry_guarantee(lash::provider::GenerationRetryGuarantee::Idempotent)
            .options(lash::provider::ProviderOptions {
                reliability: lash::provider::ProviderReliability::default()
                    .max_attempts(Some(2))
                    .base_delay_ms(0)
                    .max_delay_ms(0),
                ..Default::default()
            })
            .complete(move |request| {
                let ordinal = calls.fetch_add(1, Ordering::SeqCst) + 1;
                dispatched
                    .lock()
                    .expect("decorator attempts")
                    .push((request.request_id().to_owned(), ordinal));
                if ordinal == 2 {
                    retiring.serve(LlmProfileRegistry::new());
                }
                async move {
                    if ordinal == 1 {
                        Err(
                            lash::provider::LlmTransportError::new("retry the first attempt")
                                .with_kind(lash::provider::ProviderFailureKind::Transport)
                                .with_retry_verdict(
                                    lash::provider::TransportRetryVerdict::RetryableTransient,
                                ),
                        )
                    } else {
                        Ok(text("kimi answers"))
                    }
                }
            })
            .build()
            .into_handle()
    };
    catalog.serve(registry_of(KIMI, "kimi-k3", provider));
    let core = core_over(&double, &catalog, Vec::new());
    let session = created_on(&core, "keys-replay-key-removed", KIMI).await;
    let handle = session
        .send(TurnInput::text("ask the session's model"))
        .await
        .expect("the recorded model is accepted");
    assert_eq!(answer_of(handle).await, "kimi answers");

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the journaled model call is served from the journal, never made again"
    );
    let attempts = dispatched.lock().expect("decorator attempts");
    assert_eq!(attempts.len(), 2, "replay dispatches no additional attempt");
    assert_eq!(
        attempts[0].0, attempts[1].0,
        "retries preserve the request id"
    );
    assert!(!attempts[0].0.is_empty());
    assert_eq!(
        (attempts[0].1, attempts[1].1),
        (1, 2),
        "the decorator distinguishes attempts"
    );
    drop(attempts);
    assert_eq!(
        catalog.resolver_calls(),
        (0, 0),
        "the work after the recorded call made no (snapshot, bind) call once the key was retired"
    );
}

/// A run whose model call is not recorded yet meets a deployment that
/// retired its key: every attempt ends with the typed bind fault, nothing is
/// recorded as the call's result, and the session parks after its budget of
/// failed passes. The run's follower is told the run parked, naming the
/// unbindable key, and once the key is served again the redriven run makes
/// the call and answers.
async fn an_unjournaled_bind_fault_seals_nothing_and_recovers_after_the_park(tier: Tier) {
    let Some(double) = double(tier).await else {
        return;
    };
    let session_id = "keys-bind-fault-park";
    let kimi = Route::new("kimi answers");
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", kimi.handle()));
    let core = core_over(&double, &catalog, Vec::new());
    let session = created_on(&core, session_id, KIMI).await;
    catalog.serve(LlmProfileRegistry::new());
    session
        .send(TurnInput::text("ask the session's model"))
        .id(lash::TurnId::parse("keys-bind-fault-park-run").expect("nonblank host identity"))
        .await
        .expect("the session accepts the input");

    let failure = double.parked(session_id).await;
    assert!(
        failure.contains("llm_profile_unavailable") && failure.contains(KIMI),
        "the park's failure is the typed bind fault and names the recorded key: {failure}"
    );
    assert_eq!(kimi.calls(), 0, "no attempt reached a transport");
    let (snapshots, binds) = catalog.resolver_calls();
    assert_eq!(snapshots, 0, "a recorded model is never minted again");
    assert!(binds >= 1, "the unrecorded call asked for its binding");
    let park = park_of(&session, "keys-bind-fault-park-run").await;
    assert!(
        format!("{:?}", park.reason).contains(KIMI),
        "the run's park carries the unbindable key: {park:?}"
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
    .with_execution(std::time::Duration::from_secs(120))
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
/// the run's first model call asks for the tool, the next `completions`
/// calls answer direct completions, and every later call answers the run.
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

/// A bind fault inside a tool attempt ends the attempt where it is met
/// (FIG-4632). The direct completion that cannot bind the session's recorded
/// model hands its tool no error to catch: the tool's body is dropped there,
/// so what the tool would do with the failure never runs, on the first
/// attempt or on any retry of it. Before, every retry ran the tool to its
/// end and repeated that side effect.
async fn a_tool_is_not_run_past_a_bind_fault_of_its_direct_completion(tier: Tier) {
    let Some(double) = double(tier).await else {
        return;
    };
    let session_id = "keys-attempt-ends-at-fault";
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
    let session = created_on(&core, session_id, KIMI).await;
    session
        .send(TurnInput::text("ask the model through the tool"))
        .id(lash::TurnId::parse("keys-attempt-ends-at-fault-run").expect("nonblank host identity"))
        .await
        .expect("the session accepts the input");

    let failure = double.parked(session_id).await;
    assert!(
        failure.contains("llm_profile_unavailable"),
        "the session parked on the typed bind fault: {failure}"
    );
    assert!(
        entered.load(Ordering::SeqCst) > 1,
        "the attempt was retried before the session parked: {failure}"
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
        "only the run's model call reached the transport"
    );
    core.shutdown().await.expect("the core shuts down");
}

/// A completion a tool attempt dispatched before a later completion's bind
/// fault keeps its usage (FIG-4632). The fault ends the attempt and nothing
/// records it, so the usage of the completion already dispatched and billed
/// is settled when the attempt ends, under the run that spent it. The retry
/// that completes once the key is served dispatches both completions again:
/// the opaque attempt had recorded no result. Hosts own receipts for this
/// unrecorded retry window.
async fn a_completion_before_a_bind_fault_is_retried_only_while_unrecorded(tier: Tier) {
    let Some(double) = double(tier).await else {
        return;
    };
    let session_id = "keys-usage-before-fault";
    let calls = Arc::new(AtomicUsize::new(0));
    // Three direct completions reach the transport: the first attempt's
    // first, and both of the attempt that completes.
    let catalog = LiveCatalog::serving(registry_of(KIMI, "kimi-k3", ask_twice_provider(&calls, 3)));
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
        .id(lash::TurnId::parse("keys-usage-before-fault-run").expect("nonblank host identity"))
        .await
        .expect("the session accepts the input");

    double.parked(session_id).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the run's model call and the first attempt's first completion reached the transport; \
         every retry met the bind fault at its first completion"
    );
    // The opaque attempt recorded nothing; its completed call may retry
    // once the run is redriven with the key served again.
    let park = park_of(&session, "keys-usage-before-fault-run").await;
    assert_eq!(park.session_id.as_str(), session_id);
    assert_eq!(
        acted_past_a_fault.load(Ordering::SeqCst),
        0,
        "no attempt ran its tool past the bind fault, of {} attempts",
        entered.load(Ordering::SeqCst)
    );
}

// ---- registration -----------------------------------------------------------

#[test]
fn postgres_variants_never_pass_without_a_database_url() {
    let executable = std::env::current_exe().expect("model-keys test executable");
    let law = "two_keys_sharing_a_provider_kind_select_their_own_transport::postgres";
    for url in [None, Some(""), Some(" \t ")] {
        let mut command = std::process::Command::new(&executable);
        command
            .args(["--exact", law, "--include-ignored", "--nocapture"])
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

macro_rules! tiered {
    ($law:ident) => {
        tiered!(@tiers $law; );
    };
    ($law:ident, ignore = $reason:literal) => {
        tiered!(@tiers $law; #[ignore = $reason]);
    };
    (@tiers $law:ident; $(#[$ignored:meta])?) => {
        mod $law {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            $(#[$ignored])?
            async fn sqlite_memory() {
                super::$law(Tier::SqliteMemory).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            $(#[$ignored])?
            async fn sqlite_file() {
                super::$law(Tier::SqliteFile).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
            async fn postgres() {
                super::$law(Tier::Postgres).await;
            }
        }
    };
}

tiered!(two_keys_sharing_a_provider_kind_select_their_own_transport);
tiered!(a_catalog_edit_reaches_a_session_only_through_a_profile_change);
tiered!(a_recorded_llm_profile_whose_key_left_the_catalog_fails_typed_and_never_falls_back);
tiered!(a_recorded_model_whose_key_serves_another_wire_model_is_refused_typed);
tiered!(an_unknown_key_is_refused_before_anything_changes);
tiered!(
    a_session_turn_start_retried_after_the_host_changed_what_it_passes_keeps_its_retained_start,
    ignore = "FIG-5369: a host's later start through the core's effect host is refused ArtifactReferrerEnded"
);
tiered!(an_unsupported_reasoning_selection_is_refused_where_it_is_stated);
tiered!(a_replay_after_the_key_left_the_catalog_completes_with_zero_resolver_calls);
tiered!(
    an_unjournaled_bind_fault_seals_nothing_and_recovers_after_the_park,
    ignore = "FIG-5367: a parked session's run never answers its follower, and a host has no redrive for it"
);
tiered!(
    a_tool_is_not_run_past_a_bind_fault_of_its_direct_completion,
    ignore = "FIG-5368: a direct completion's bind fault reaches its tool, which records it as its result"
);
tiered!(
    a_completion_before_a_bind_fault_is_retried_only_while_unrecorded,
    ignore = "FIG-5368, FIG-5367: the bind fault reaches the tool instead of parking the session, which no host can redrive"
);
