//! Host model keys (FIG-4374): a host registers opaque model keys in a
//! `ModelRegistry`, several of them served by transports that share one
//! provider kind. A session records the binding its key minted at creation
//! and at every model change; a root runs the recorded binding, or ends its
//! attempt with the typed `ModelUnavailable`, and never falls back to another
//! registration or to today's catalog entry.
//!
//! The host code below uses `lash::` paths only; the store tiers and the
//! Restate server double beneath them are the test's own infrastructure.

#![cfg(all(feature = "restate", feature = "sqlite", feature = "testing"))]
#![allow(clippy::disallowed_methods)]
#![expect(
    clippy::expect_used,
    reason = "acceptance laws establish each step's result"
)]

use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::provider::{LlmResponse, ProviderHandle};
use lash::{LashCore, ModelKey, ModelMetadata, ModelRegistry, RegisteredModel, TurnInput};

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
    _keep: Vec<Box<dyn std::any::Any + Send + Sync>>,
}

async fn double(tier: Tier, replay: bool, seed: u64) -> Option<Double> {
    let config = lash_restate_test::ServerConfig {
        always_replay: replay,
        ..lash_restate_test::ServerConfig::default()
    };
    let hooks = lash_restate_test::DeploymentHooks::default;
    match tier {
        Tier::SqliteMemory => {
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
                    Ok(Arc::new(
                        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                            .await
                            .expect("SQLite memory stores"),
                    ) as Arc<dyn lash::StoreSet>)
                })
                .await
                .expect("SQLite memory Restate double");
            Some(Double {
                double,
                _keep: Vec::new(),
            })
        }
        Tier::SqliteFile => {
            let root = tempfile::tempdir().expect("SQLite store directory");
            let path = root.path().to_path_buf();
            let double =
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
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
                lash_restate_test::backend_with_store_set(seed, config, hooks(), |clock| async {
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
                    wire_model: request.model.clone(),
                    revision: request
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

fn metadata(wire_model: &str, revision: &str) -> ModelMetadata {
    let mut extra_body = serde_json::Map::new();
    extra_body.insert("catalog_revision".to_string(), revision.into());
    ModelMetadata::builder(wire_model)
        .context_window_tokens(64_000)
        .extra_body(extra_body)
        .build()
        .expect("model metadata")
}

fn catalog(entries: &[Entry<'_>]) -> Arc<ModelRegistry> {
    let registry = entries
        .iter()
        .try_fold(ModelRegistry::new(), |registry, entry| {
            registry.register(
                entry.key,
                RegisteredModel::new(
                    metadata(entry.wire_model, entry.revision),
                    entry.route.handle(),
                ),
            )
        })
        .expect("the catalog names each key once");
    Arc::new(registry)
}

/// A core over `entries`, whose first key is the default.
fn core(double: &Double, entries: &[Entry<'_>], worker: &str) -> LashCore {
    let default_key = entries.first().expect("the catalog has a default key").key;
    LashCore::standard_builder(double.double.lash_backend(), lash::TurnBudget::Unbounded)
        .models(catalog(entries))
        .model(default_key)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "model-keys-worker",
            worker,
        ))
        .expect("the host core builds")
}

async fn created_on(core: &LashCore, id: &str, key: &str) -> lash::LashSession {
    core.session(id)
        .create(lash::SessionCreation {
            spec: lash::SessionSpec::new().model(key),
            ..lash::SessionCreation::default()
        })
        .await
        .expect("create the session");
    core.session(id).open().await.expect("open the session")
}

async fn answer_of(handle: lash::SendHandle) -> String {
    let output = handle.output().await.expect("the root settles");
    assert!(output.is_success(), "the root answers: {output:?}");
    output
        .assistant_message()
        .expect("the root answers with text")
        .to_string()
}

fn recorded_key(session: &lash::LashSession) -> ModelKey {
    session
        .policy_snapshot()
        .model
        .expect("the session records a model")
        .key()
        .clone()
}

/// Wait until an attempt on the double ended with the typed refusal of an
/// unbindable recorded model.
async fn await_model_unavailable(double: &Double) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let failure = double
                .double
                .server()
                .invocations()
                .into_iter()
                .filter_map(|view| view.last_failure.map(|(_, message)| message))
                .find(|message| message.contains("code: ModelUnavailable"));
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

// ---- the laws ---------------------------------------------------------------

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
async fn a_catalog_edit_reaches_a_session_only_through_a_model_change(
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
            lash::config::ConfigTransaction::of(lash::config::SetModel {
                model: ModelKey::new(KIMI),
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
/// typed `ModelUnavailable` and calls no other registration, the new default
/// included; a deployment that registers the key again runs it there.
async fn a_recorded_model_whose_key_left_the_catalog_fails_typed_and_never_falls_back(
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
    let failure = await_model_unavailable(&double).await;
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
/// `ModelUnavailable`, and the transport is never sent the request.
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
    let failure = await_model_unavailable(&double).await;
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
            spec: lash::SessionSpec::new().model(unregistered),
            ..lash::SessionCreation::default()
        })
        .await;
    assert!(
        matches!(&created, Err(lash::EmbedError::ModelUnknown(error)) if error.key.as_str() == unregistered),
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
            assert_eq!(error.code, lash::runtime::RuntimeErrorCode::ModelUnknown);
        }
        other => panic!("the send is refused typed, got: {:?}", other.as_ref().err()),
    }

    let before = session.policy_snapshot();
    let config = session.admin().config();
    let revision = config.revision().await.expect("read the config revision");
    let changed = config
        .apply(
            lash::config::ConfigWrite::new("keys-unknown-change", revision),
            lash::config::ConfigTransaction::of(lash::config::SetModel {
                model: ModelKey::new(unregistered),
            }),
        )
        .await
        .expect("the model change settles");
    let lash::config::ConfigTransactionOutcome::Refused { refusal } = changed else {
        panic!("the model change is refused typed: {changed:?}");
    };
    assert_eq!(
        serde_json::from_value::<lash::config::CoreConfigRefusal>(refusal.refusal)
            .expect("the core owner's typed refusal"),
        lash::config::CoreConfigRefusal::UnknownModel {
            key: ModelKey::new(unregistered),
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

/// A host session-turn start that names no model, under host key `key`.
fn session_turn_start(key: &str, text: &str) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "keys-session-turn-start".into(),
            create_request: Box::new(
                lash_core::SessionCreateRequest::root(
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )
                .with_session_id(format!("{key}-child")),
            ),
            turn_input: Box::new(TurnInput::text(text)),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_host_start_key(key)
}

/// Issue `request` on `core` from a host handler named `operation`.
async fn start_on(
    double: &Double,
    core: &LashCore,
    operation: &str,
    request: lash_core::ProcessStartRequest,
) -> Result<lash_core::ProcessStartReceipt, lash::EmbedError> {
    let result = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let core = core.clone();
        let result = Arc::clone(&result);
        Arc::new(move |scoped| {
            let core = core.clone();
            let request = request.clone();
            let result = Arc::clone(&result);
            Box::pin(async move {
                let started = core.processes().start(request, scoped).await;
                *result.lock().expect("start result") = Some(started);
            })
        })
    };
    double
        .double
        .run_in_handler(
            lash_core::AdmittedScope::runtime_operation(operation),
            attempt,
        )
        .await
        .expect("the host handler runs");
    result
        .lock()
        .expect("start result")
        .take()
        .expect("the handler issued the start")
}

/// The default binding the start retained under `process_id` recorded.
async fn recorded_default(double: &Double, process_id: &lash::ProcessId) -> lash::ModelConfig {
    let record = double
        .double
        .lash_backend()
        .process_registry()
        .get_process(process_id)
        .await
        .expect("read the process")
        .expect("the process is retained");
    let lash_core::ProcessInput::SessionTurn { create_request, .. } = record.input.as_ref() else {
        panic!("the retained start is a session turn: {record:?}");
    };
    create_request
        .default_model()
        .expect("the start recorded the default binding")
        .clone()
}

/// A host session-turn start that names no model records the core's default
/// binding once, when it first registers. The same start retried under its
/// host key after the catalog entry was edited, and again after the default
/// moved to another key, is returned the retained process with the binding it
/// recorded; neither retry is a `StartKeyConflict`. A start that states
/// another request under the key still is.
async fn a_session_turn_start_retried_after_a_catalog_edit_keeps_its_retained_start(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let Some(double) = double(tier, replay, seed).await else {
        return;
    };
    let key = "keys-start-retry";
    let glm = Route::new("glm answers");
    let kimi = Route::new("kimi answers");
    let first = core(
        &double,
        &[Entry {
            key: GLM,
            wire_model: "glm-5.3-flash",
            revision: "r1",
            route: &glm,
        }],
        "keys-boot-1",
    );
    let started = start_on(
        &double,
        &first,
        "keys-start-first",
        session_turn_start(key, "run the child"),
    )
    .await
    .expect("the first start registers");
    assert_eq!(
        started.disposition,
        lash_core::ProcessRegistrationOutcome::Created
    );
    let recorded = recorded_default(&double, &started.process_id).await;
    assert_eq!(recorded.key().as_str(), GLM);
    drop(first);

    let edited = core(
        &double,
        &[Entry {
            key: GLM,
            wire_model: "glm-5.3-flash",
            revision: "r2",
            route: &glm,
        }],
        "keys-boot-2",
    );
    let after_edit = start_on(
        &double,
        &edited,
        "keys-start-after-edit",
        session_turn_start(key, "run the child"),
    )
    .await
    .expect("the retry after a catalog edit presents the same start");
    assert_eq!(after_edit.process_id, started.process_id);
    assert_eq!(
        after_edit.disposition,
        lash_core::ProcessRegistrationOutcome::Existing
    );
    drop(edited);

    let moved = core(
        &double,
        &[Entry {
            key: KIMI,
            wire_model: "kimi-k3",
            revision: "r1",
            route: &kimi,
        }],
        "keys-boot-3",
    );
    let after_move = start_on(
        &double,
        &moved,
        "keys-start-after-move",
        session_turn_start(key, "run the child"),
    )
    .await
    .expect("the retry after a change of default presents the same start");
    assert_eq!(after_move.process_id, started.process_id);
    assert_eq!(
        after_move.disposition,
        lash_core::ProcessRegistrationOutcome::Existing
    );
    assert_eq!(
        recorded_default(&double, &started.process_id).await,
        recorded,
        "no retry re-minted the binding the start recorded"
    );

    let other = start_on(
        &double,
        &moved,
        "keys-start-other-request",
        session_turn_start(key, "run another child"),
    )
    .await;
    match &other {
        Err(lash::EmbedError::Plugin(lash_core::PluginError::StartKeyConflict { .. })) => {}
        other => panic!("another request under the key conflicts, got: {other:?}"),
    }
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
    let thinking = ModelMetadata::builder("thinker-1")
        .context_window_tokens(64_000)
        .capability(lash::provider::ModelCapability {
            reasoning: Some(lash::provider::ReasoningCapability {
                efforts: vec!["high".to_string()],
                encoding: lash::provider::ReasoningEncoding::Effort,
                disable: false,
                mandatory: false,
            }),
            ..lash::provider::ModelCapability::default()
        })
        .build()
        .expect("model metadata");
    let registry = ModelRegistry::new()
        .register(THINKER, RegisteredModel::new(thinking, thinker.handle()))
        .and_then(|registry| {
            registry.register(
                GLM,
                RegisteredModel::new(metadata("glm-5.3-flash", "r1"), glm.handle()),
            )
        })
        .expect("the catalog names each key once");
    let core =
        LashCore::standard_builder(double.double.lash_backend(), lash::TurnBudget::Unbounded)
            .models(Arc::new(registry))
            .model(THINKER)
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
            spec: lash::SessionSpec::new().model(GLM).reasoning(high.clone()),
            ..lash::SessionCreation::default()
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
            spec: lash::SessionSpec::new()
                .model(THINKER)
                .reasoning(high.clone()),
            ..lash::SessionCreation::default()
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
                .env_remove("LASH_POSTGRES_DATABASE_URL")
                .env_remove("LASH_REQUIRE_POSTGRES");
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
    two_keys_sharing_a_provider_kind_select_their_own_transport,
    0x4374_1100
);
tiered!(
    a_catalog_edit_reaches_a_session_only_through_a_model_change,
    0x4374_1200
);
tiered!(
    a_recorded_model_whose_key_left_the_catalog_fails_typed_and_never_falls_back,
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
    a_session_turn_start_retried_after_a_catalog_edit_keeps_its_retained_start,
    0x4531_1100
);
tiered!(
    an_unsupported_reasoning_selection_is_refused_where_it_is_stated,
    0x4531_1200
);
