//! A send to a session the engine cannot open is answered, never left
//! waiting (FIG-4597), and the store's faults on the durable turn path keep
//! every answer truthful.
//!
//! The session actor opens a session's runtime for each pass that runs its
//! work. Each way that open ends terminally answers the sender of the input
//! the pass was for, within a bound, with the error that names the cause:
//!
//! - no catalog row, or a deleted one: the facade refuses the send before
//!   anything is accepted, as `UnknownSession` or the store's
//!   `SessionDeleted`;
//! - a catalog row with no head: `SessionCreationUnrecorded` (FIG-4553);
//! - a catalog read the store refuses with a typed refusal: that refusal's
//!   own code and cause;
//! - a permanent catalog refusal without a typed carrier: terminal
//!   `StoreRefused`, naming the refusal;
//! - a plugin that refuses to build: the plugin's own refusal;
//! - a stored record the store cannot decode: `RuntimeStoreCorrupt`, once;
//! - a transient store fault, or a lost reply, at any session-store call of
//!   the open, the runtime's assembly, a single or batch admission, or a
//!   committed child's reopen: the actor retries and the input is answered
//!   as the clean run answered it, with the same transcript and the same
//!   input applications, each once.
//!
//! Clean traces derive the store-fault cells: each session-store operation
//! the clean run calls is cut at its first call by a transient fault, a
//! permanent refusal, corrupt data and a lost reply. A permanent or corrupt
//! cell answers the sender typed within the bound, or, met after the run's
//! answer was published, leaves that answer standing. No actor is left
//! parked for an operator by a fault the engine can answer.
//!
//! The faults are injected into the session store under the engine (the
//! session actor's own reads and writes), over SQLite memory and file; the
//! PostgreSQL legs are ignored outside a PostgreSQL gate.

use super::*;
use crate::{SessionId, TurnInput};
use lash_core::testing::runtime_helpers::LayeredStores;
use lash_core::testing::{Call, Op, Phase, Script, StoreOp};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// How long a send to a session that cannot open may take to answer. The
/// bound turns a sender left waiting into a failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

fn builder(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
}

/// The storage a law's deployment runs over.
#[derive(Clone, Copy, Debug)]
enum Storage {
    Memory,
    File,
    Postgres,
}

/// A store set, the test seams of its session store, and what must outlive
/// both.
struct Stores {
    stores: Arc<dyn lash_core::StoreSet>,
    seams: Arc<dyn lash_core::store::StoreTestSupport>,
    _held: Box<dyn std::any::Any + Send>,
}

async fn stores_over(storage: Storage) -> Stores {
    match storage {
        Storage::Memory => {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite memory stores");
            let seams = stores.session_store_factory();
            Stores {
                stores: Arc::new(stores),
                seams,
                _held: Box::new(()),
            }
        }
        Storage::File => {
            let files = tempfile::tempdir().expect("SQLite store directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(files.path().join("lash.db"))
                .await
                .expect("SQLite file stores");
            let seams = stores.session_store_factory();
            Stores {
                stores: Arc::new(stores),
                seams,
                _held: Box::new(files),
            }
        }
        Storage::Postgres => {
            let (stores, database, attachments) = postgres_store_parts().await;
            let seams = stores.session_store_factory();
            Stores {
                stores,
                seams,
                _held: Box::new((database, attachments)),
            }
        }
    }
}

/// `stores` with its session store wrapped by `script` under the engine, and
/// the session store it wraps.
fn scripted(
    stores: &Arc<dyn lash_core::StoreSet>,
    script: &Script,
    actor: &str,
) -> (
    Arc<dyn lash_core::StoreSet>,
    Arc<dyn lash_core::DeploymentStore>,
) {
    let inner = stores.session_store_factory();
    let layered = LayeredStores::over(Arc::clone(stores))
        .map_session_store_factory(|inner| -> Arc<dyn lash_core::DeploymentStore> {
            script.wrap(actor, inner)
        })
        .into_store_set();
    (layered, inner)
}

/// A core over `stores`, serving its sessions' work on its own node or not.
fn core_over(stores: &Arc<dyn lash_core::StoreSet>, serve: bool) -> LashCore {
    builder(lash_conformance::backend_over(Arc::clone(stores)))
        .serve_sessions(serve)
        .build(crate::testing::runtime_lease_owner())
        .expect("fixture core")
}

async fn create(core: &LashCore, id: &str) -> Result<()> {
    core.session(SessionId::fixture(id.to_string()))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    Ok(())
}

/// The session actor of `id`, as the durable store holds it.
async fn session_actor(
    core: &LashCore,
    id: &str,
) -> Option<lash_core::durable_port::ActorSnapshot> {
    let actor = lash_core::durable_port::ActorKey::session(id).expect("a session actor key");
    core.backend()
        .durable()
        .actor(&actor)
        .await
        .expect("read the session actor")
}

/// Wait until `id`'s session actor has no mail left and no turn unfinished:
/// what the sends left behind has run. An owner with nothing to do keeps
/// the actor hot until its idle eviction. Answers whether it settled.
async fn settled(core: &LashCore, id: &str) -> bool {
    let session = SessionId::fixture(id.to_string());
    tokio::time::timeout(ANSWERS_WITHIN, async {
        loop {
            let idle = session_actor(core, id).await.is_none_or(|actor| {
                actor.pending_mail == 0
                    && !matches!(actor.state, lash_core::durable_port::ActorState::Ready)
            });
            let turn = core
                .backend()
                .durable()
                .turn(&session)
                .await
                .expect("read the session's turn");
            if idle && turn.is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// No actor is set aside for an operator: the refusal ended what met it.
async fn assert_nothing_parked(core: &LashCore, id: &str) {
    let actor = session_actor(core, id).await;
    assert!(
        !actor.as_ref().is_some_and(|actor| matches!(
            actor.state,
            lash_core::durable_port::ActorState::Parked
        )),
        "{id}: the session actor is not parked: {actor:?}"
    );
}

/// The error a send to `id` is answered with, within [`ANSWERS_WITHIN`].
async fn refusal_of(core: &LashCore, id: &str) -> EmbedError {
    let sent = tokio::time::timeout(ANSWERS_WITHIN, async {
        core.session(SessionId::fixture(id.to_string()))
            .durable()
            .await?
            .send(TurnInput::text("to a session that cannot open"))
            .output()
            .await
    })
    .await;
    let Ok(answer) = sent else {
        panic!(
            "{id}: the send is answered in {ANSWERS_WITHIN:?}; the session actor is {:?}",
            session_actor(core, id).await
        );
    };
    let error = match answer {
        Ok(output) => panic!("{id}: the session cannot open, got {:?}", output.result),
        Err(error) => error,
    };
    assert_nothing_parked(core, id).await;
    error
}

/// The runtime error the engine refused the send with.
fn runtime_refusal(id: &str, error: EmbedError) -> lash_core::RuntimeError {
    match error {
        EmbedError::Runtime(error) => error,
        EmbedError::Store(error)
        | EmbedError::Session(lash_core::SessionError::Store { source: error, .. }) => {
            lash_core::RuntimeEffectControllerError::from(error).into_runtime_error()
        }
        EmbedError::Plugin(error) => error.into_turn_failure(lash_core::RuntimeErrorCode::Plugin),
        other => panic!("{id}: the refusal is the engine's typed error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_an_unknown_session_is_answered_unknown() -> Result<()> {
    const ID: &str = "never-created";
    let stores = stores_over(Storage::Memory).await;
    let core = core_over(&stores.stores, true);
    let error = refusal_of(&core, ID).await;
    assert!(
        matches!(&error, EmbedError::UnknownSession { session_id } if session_id.as_str() == ID),
        "{error:?}"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_deleted_session_is_answered_deleted() -> Result<()> {
    const ID: &str = "deleted-before-the-send";
    let stores = stores_over(Storage::Memory).await;
    let core = core_over(&stores.stores, true);
    create(&core, ID).await?;
    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(administration.delete_context(ID)?).await?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Requested { .. }),
        "{deletion:?}; the session actor is {:?}",
        session_actor(&core, ID).await
    );
    assert_eq!(
        core.await_session_deletion(&SessionId::from(ID)).await?,
        crate::core::SessionDeleteCompletion::Deleted
    );
    let error = refusal_of(&core, ID).await;
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::SessionDeleted { session_id }) if session_id.as_str() == ID
        ),
        "{error:?}"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the sender waiting"]
async fn a_send_to_a_catalog_row_with_no_head_is_answered_creation_unrecorded() -> Result<()> {
    const ID: &str = "row-with-no-head";
    let stores = stores_over(Storage::Memory).await;
    let core = core_over(&stores.stores, true);
    create(&core, ID).await?;
    stores
        .seams
        .delete_session_head_for_testing(&SessionId::from(ID))
        .await
        .map_err(EmbedError::Store)?;
    let error = runtime_refusal(ID, refusal_of(&core, ID).await);
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionCreationUnrecorded,
        "{error:?}"
    );
    assert!(error.is_terminal(), "{error:?}");
    core.shutdown().await?;
    Ok(())
}

/// A plugin factory that builds until `refuse` is set.
struct RefusingFactory {
    refuse: Arc<AtomicBool>,
}

impl lash_core::facade_support::PluginFactory for RefusingFactory {
    fn id(&self) -> &'static str {
        "fig4597-refusing-factory"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(lash_core::PluginError::Session(
                "the plugin refuses to build".to_string(),
            ));
        }
        Ok(Arc::new(InertPlugin))
    }
}

impl lash_core::plugin::PluginDefinition for RefusingFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("fig4597-refusing-factory")
    }
}

struct InertPlugin;

impl lash_core::facade_support::SessionPlugin for InertPlugin {
    fn id(&self) -> &'static str {
        "fig4597-refusing-factory"
    }

    fn register(
        &self,
        _reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_session_whose_plugin_refuses_to_build_is_answered_with_the_refusal()
-> Result<()> {
    const ID: &str = "plugin-refuses";
    let stores = stores_over(Storage::Memory).await;
    let refuse = Arc::new(AtomicBool::new(false));
    let core = builder(lash_conformance::backend_over(Arc::clone(&stores.stores)))
        .plugin(Arc::new(RefusingFactory {
            refuse: Arc::clone(&refuse),
        }))
        .build(crate::testing::runtime_lease_owner())?;
    create(&core, ID).await?;
    refuse.store(true, Ordering::SeqCst);
    let error = runtime_refusal(ID, refusal_of(&core, ID).await);
    assert_eq!(error.code, lash_core::RuntimeErrorCode::Plugin, "{error:?}");
    assert!(
        error.message.contains("the plugin refuses to build"),
        "the refusal names the plugin's own: {error:?}"
    );
    core.shutdown().await?;
    Ok(())
}

/// How the catalog refuses the engine's lookup.
#[derive(Clone, Copy, Debug)]
enum CatalogRefusal {
    /// A refusal with a typed carrier past the store.
    WriterFenced,
    /// A refusal the store states and nothing carries typed.
    Unsupported,
}

impl CatalogRefusal {
    fn error(self) -> StoreError {
        match self {
            Self::WriterFenced => StoreError::WriterFenced {
                recorded: 2,
                writable: lash_core::compat::VersionRange::exactly(1),
            },
            Self::Unsupported => StoreError::UnsupportedStoreOperation {
                operation: "lookup_session",
            },
        }
    }
}

/// The send is accepted, and then the catalog refuses every lookup the
/// engine's open makes: the sender is answered with the engine's refusal.
async fn a_send_whose_open_meets_a_refusing_catalog_is_answered(
    refusal: CatalogRefusal,
) -> Result<lash_core::RuntimeError> {
    let id = format!("catalog-refuses-{refusal:?}").to_lowercase();
    let stores = stores_over(Storage::Memory).await;
    let script = Script::new();
    let (engine_stores, _) = scripted(&stores.stores, &script, "catalog");
    // The send is accepted by a core that serves no session; the engine
    // opens the session only once the catalog refuses.
    let accepting = core_over(&stores.stores, false);
    create(&accepting, &id).await?;
    let sent = accepting
        .session(SessionId::fixture(id.clone()))
        .durable()
        .await?
        .send(TurnInput::text("accepted before the engine's open"))
        .await?;
    script
        .on(StoreOp::lookup_session)
        .from_nth(script.calls(StoreOp::lookup_session) + 1)
        .before()
        .fail(move || refusal.error());
    let serving = core_over(&engine_stores, true);
    let Ok(answer) = tokio::time::timeout(ANSWERS_WITHIN, sent.output()).await else {
        panic!(
            "{id}: the send is answered in {ANSWERS_WITHIN:?}; the session actor is {:?}",
            session_actor(&serving, &id).await
        );
    };
    let error = match answer {
        Ok(output) => panic!("{id}: the session cannot open, got {:?}", output.result),
        Err(error) => runtime_refusal(&id, error),
    };
    assert_nothing_parked(&serving, &id).await;
    serving.shutdown().await?;
    accepting.shutdown().await?;
    Ok(error)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the sender waiting"]
async fn a_send_whose_open_meets_a_typed_store_refusal_is_answered_with_it() -> Result<()> {
    let error =
        a_send_whose_open_meets_a_refusing_catalog_is_answered(CatalogRefusal::WriterFenced)
            .await?;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::WriterFenced,
        "{error:?}"
    );
    let Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) = &error.cause else {
        panic!("the refusal carries its typed cause: {error:?}");
    };
    assert_eq!(
        refusal.clone().into_store_error().variant_name(),
        CatalogRefusal::WriterFenced.error().variant_name()
    );
    assert!(error.is_terminal(), "{error:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the sender waiting"]
async fn a_send_whose_open_meets_an_untyped_store_refusal_is_answered_naming_it() -> Result<()> {
    let error =
        a_send_whose_open_meets_a_refusing_catalog_is_answered(CatalogRefusal::Unsupported).await?;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::StoreRefused,
        "{error:?}"
    );
    assert!(error.is_terminal(), "{error:?}");
    assert!(
        error.message.contains("lookup_session"),
        "the refusal names the store's own: {error:?}"
    );
    Ok(())
}

mod sweep {
    use super::*;
    use futures_util::FutureExt as _;

    const ID: &str = "store-fault-sweep";
    const WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

    #[derive(Clone, Copy, Debug)]
    enum Scenario {
        Open,
        Assembly,
        Admission,
        Child,
    }

    /// Every kind of fault a cell injects.
    const EVERY: &[Kind] = &[
        Kind::Transient,
        Kind::Permanent,
        Kind::Corrupt,
        Kind::LostReply,
    ];
    /// The faults a retry recovers from.
    const RECOVERED: &[Kind] = &[Kind::Transient, Kind::LostReply];

    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Transient,
        Permanent,
        Corrupt,
        LostReply,
    }

    impl Kind {
        fn error(self) -> StoreError {
            match self {
                Self::Transient | Self::LostReply => StoreError::StorageFailure {
                    backend: "store-fault-sweep",
                    message: "injected unavailable store".into(),
                },
                Self::Permanent => StoreError::SessionStateVersionNewerThanRuntime {
                    found: lash_core::store::CURRENT_SESSION_STATE_VERSION + 1,
                    current: lash_core::store::CURRENT_SESSION_STATE_VERSION,
                },
                Self::Corrupt => StoreError::StoredDataCorrupt {
                    record_kind: "store-fault-sweep",
                    message: "injected unreadable record".into(),
                },
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct Cell {
        op: Op,
        kind: Kind,
    }

    impl std::fmt::Display for Cell {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}#1/{:?}", self.op, self.kind)
        }
    }

    fn arm(script: &Script, cell: Cell) {
        let first = script.calls(cell.op) + 1;
        match cell.kind {
            Kind::Transient => script
                .on(cell.op)
                .nth(first)
                .times(2)
                .before()
                .fail(|| Kind::Transient.error()),
            Kind::LostReply => script.on(cell.op).nth(first).after().lose_reply(),
            kind => script
                .on(cell.op)
                .from_nth(first)
                .before()
                .fail(move || kind.error()),
        }
    }

    #[derive(Debug, PartialEq)]
    enum Answer {
        Success(Vec<String>),
        Stopped {
            status: String,
        },
        Refused {
            code: String,
            cause: Option<serde_json::Value>,
            terminal: bool,
        },
    }

    fn refused(error: EmbedError) -> Answer {
        let error = runtime_refusal(ID, error);
        Answer::Refused {
            code: error.code.as_str().to_owned(),
            terminal: error.is_terminal() && !error.is_retryable(),
            cause: error
                .cause
                .map(|cause| serde_json::to_value(cause).expect("typed cause")),
        }
    }

    // The host retries keyed admission and observation, while the engine
    // alone retries execution of an accepted input.
    async fn submit(
        durable: &crate::DurableSession,
        batch: bool,
    ) -> Result<Vec<crate::SendHandle>> {
        loop {
            let result = if batch {
                durable
                    .send_batch([
                        (
                            crate::TurnId::parse("sweep-b").expect("nonblank host identity"),
                            TurnInput::text("second"),
                        ),
                        (
                            crate::TurnId::parse("sweep-c").expect("nonblank host identity"),
                            TurnInput::text("third"),
                        ),
                    ])
                    .await
            } else {
                durable
                    .send(TurnInput::text("first"))
                    .id(crate::TurnId::parse("sweep-a").expect("nonblank host identity"))
                    .await
                    .map(|handle| vec![handle])
            };
            match result {
                Err(error) if error.is_retryable() => tokio::task::yield_now().await,
                result => return result,
            }
        }
    }

    async fn answers(
        durable: &crate::DurableSession,
        handles: Vec<crate::SendHandle>,
    ) -> Result<Answer> {
        let mut answers = Vec::new();
        for mut handle in handles {
            let input = handle.input_id().clone();
            let output = loop {
                match handle.output().await {
                    Err(error) if error.is_retryable() => {
                        handle = durable.attach(input.clone());
                        tokio::task::yield_now().await;
                    }
                    result => break result?,
                }
            };
            if !output.is_success() {
                return Ok(Answer::Stopped {
                    status: format!("{:?}", output.result.outcome),
                });
            }
            answers.push(output.assistant_message().expect("echo answer").to_owned());
        }
        Ok(Answer::Success(answers))
    }

    #[derive(Debug)]
    struct Run {
        answer: std::result::Result<Answer, String>,
        transcript: Vec<(String, String)>,
        applications: Vec<String>,
        inputs: Vec<String>,
        trace: Vec<Call>,
        parked: bool,
    }

    async fn start_child(core: &LashCore) -> Result<lash_core::ProcessId> {
        let started = core
            .processes()
            .start(
                lash_core::ProcessStartRequest::new(
                    lash_core::ProcessInput::SessionTurn {
                        definition_key: "sweep-child".into(),
                        create_request: Box::new(
                            lash_core::SessionCreateRequest::root(
                                lash_core::SessionStartPoint::Empty,
                                lash_core::PluginOptions::default(),
                            )
                            .with_spec(&mock_session_spec())
                            .expect("a root spec")
                            .with_session_id(ID),
                        ),
                        turn_input: Box::new(TurnInput::text("first")),
                        result: lash_core::SessionTurnOutcome::FinalValue { schema: None },
                    },
                    lash_core::ProcessOriginator::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_host_start_key("sweep-child"),
                core.effect_host(),
            )
            .await?;
        Ok(started.process_id)
    }

    async fn child_answer(core: &LashCore, process: &lash_core::ProcessId) -> Result<Answer> {
        let output = core.processes().await_output(process).await?;
        let lash_core::ProcessAwaitOutput::Settled { output } = output else {
            panic!("the child settles: {output:?}");
        };
        Ok(match output.outcome {
            lash_core::ToolCallOutcome::Success(value) => Answer::Success(vec![
                value
                    .to_json_value()
                    .as_str()
                    .expect("child echo")
                    .to_owned(),
            ]),
            lash_core::ToolCallOutcome::Failure(failure) => Answer::Refused {
                code: failure.code,
                cause: None,
                terminal: true,
            },
            other => panic!("the child answers: {other:?}"),
        })
    }

    async fn run(storage: Storage, scenario: Scenario, cell: Option<Cell>) -> Run {
        let stores = stores_over(storage).await;
        let script = Script::new();
        let (engine_stores, inner) = scripted(&stores.stores, &script, "deployment");
        let accepting = core_over(&stores.stores, false);
        create(&accepting, ID).await.expect("commit the session");
        if matches!(scenario, Scenario::Child) {
            // The child is committed before its process runs, so its run
            // takes the committed child's reopen path.
            let runtime_store: Arc<dyn lash_core::RuntimeStore> = inner.clone();
            let view = lash_core::store::SessionStore::new(runtime_store, SessionId::from(ID))
                .expect("bind the recorded child");
            let mut state = lash_core::store::load_session_window_state(
                &view,
                lash_core::store::WindowSelector::Current,
            )
            .await
            .expect("read the created child")
            .expect("created head")
            .state;
            state.ensure_agent_frame_initialized();
            inner
                .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(&state))
                .await
                .expect("commit the child's initial head without running its first turn");
            let head = inner
                .load_session_head_meta(&SessionId::from(ID))
                .await
                .expect("committed child metadata")
                .expect("child head");
            assert!(
                head.head_revision > 0,
                "a committed child takes the reopen path"
            );
        }
        let durable = accepting
            .session(SessionId::parse(ID).expect("nonblank host identity"))
            .durable()
            .await
            .expect("durable handle");
        durable
            .pending_turn_inputs()
            .await
            .expect("resolve before arming");
        // Open and Assembly accept the input before the engine opens the
        // session: the accepting core serves no session.
        let accepted = if matches!(scenario, Scenario::Open | Scenario::Assembly) {
            Some(
                submit(&durable, false)
                    .await
                    .expect("accept before the open"),
            )
        } else {
            None
        };
        // Assembly holds the open past its window read, so the fault meets
        // the runtime's assembly rather than the open.
        let ready = matches!(scenario, Scenario::Assembly).then(|| {
            script
                .on(StoreOp::load_session_window)
                .nth(script.calls(StoreOp::load_session_window) + 1)
                .after()
                .pause()
        });
        let serving = core_over(&engine_stores, true);
        if let Some(gate) = &ready {
            gate.reached(1).await;
        }
        let start = script.trace().len();
        if let Some(cell) = cell {
            arm(&script, cell);
        }
        if let Some(gate) = ready {
            gate.open_all();
        }
        let mut child = None;
        let work = async {
            if matches!(scenario, Scenario::Child) {
                let process = start_child(&serving).await?;
                child = Some(process.to_string());
                return child_answer(&serving, &process).await;
            }
            let handles = match accepted {
                Some(handles) => handles,
                None => submit(&durable, false).await?,
            };
            let first = answers(&durable, handles).await?;
            let Answer::Success(mut output) = first else {
                return Ok(first);
            };
            if matches!(scenario, Scenario::Admission) {
                let batch = answers(&durable, submit(&durable, true).await?).await?;
                let Answer::Success(batch) = batch else {
                    return Ok(batch);
                };
                output.extend(batch);
            }
            Ok::<_, EmbedError>(Answer::Success(output))
        };
        let answer = match tokio::time::timeout(WITHIN, work).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(error)) => Ok(refused(error)),
            Err(_) => Err(format!("no answer within {WITHIN:?}")),
        };
        // Finish the session's background work before inspecting the
        // durable result. A sticky fault that wedges the actor is
        // evidence, not a hung law.
        let mut answer = answer;
        if !settled(&serving, ID).await {
            answer = Err("the session actor did not settle".into());
        }
        let parked = session_actor(&serving, ID).await.is_some_and(|actor| {
            matches!(actor.state, lash_core::durable_port::ActorState::Parked)
        });
        let trace = script.trace()[start..].to_vec();
        // The oracle reads through the underlying store so sticky faults do
        // not hide the committed evidence they are meant to test.
        let transcript = match inner
            .load_session_window(
                &SessionId::from(ID),
                lash_core::store::WindowSelector::Current,
            )
            .await
            .expect("inspect committed transcript")
        {
            Some(window) => window
                .window
                .read_model()
                .messages
                .iter()
                .map(|message| {
                    (
                        crate::turn::message_role(message).into(),
                        crate::turn::message_text(message),
                    )
                })
                .collect(),
            None => Vec::new(),
        };
        let mut applications = inner
            .list_turn_input_applications(&SessionId::from(ID))
            .await
            .expect("applications")
            .into_iter()
            .map(|row| {
                let key = row.source_key.expect("keyed input");
                if child.as_ref() == Some(&key) {
                    "sweep-child".into()
                } else {
                    key
                }
            })
            .collect::<Vec<_>>();
        let mut inputs = inner
            .list_pending_turn_inputs(&SessionId::from(ID))
            .await
            .expect("inputs")
            .into_iter()
            .map(|row| row.input.source_key.expect("keyed input"))
            .collect::<Vec<_>>();
        applications.sort();
        inputs.sort();
        let _ = serving.shutdown().await;
        let _ = accepting.shutdown().await;
        drop(script); // An unfired rule fails this cell even if its answer matched.
        Run {
            answer,
            transcript,
            applications,
            inputs,
            trace,
            parked,
        }
    }

    fn verdict(
        scenario: Scenario,
        cell: Cell,
        clean: &Run,
        run: &Run,
    ) -> std::result::Result<(), String> {
        if run.parked {
            return Err("the session actor is parked for an operator".into());
        }
        let answer = run.answer.as_ref().map_err(Clone::clone)?;
        let unchanged = run.answer == clean.answer
            && run.transcript == clean.transcript
            && run.applications == clean.applications
            && run.inputs == clean.inputs;
        match cell.kind {
            Kind::Transient | Kind::LostReply => {
                if !unchanged {
                    return Err(
                        "recovery changed the answer, transcript, or admitted-once evidence".into(),
                    );
                }
            }
            Kind::Permanent | Kind::Corrupt => {
                let (expected_code, expected_cause) = match cell.kind {
                    Kind::Permanent => (
                        "session_state_version_newer_than_runtime",
                        serde_json::json!({
                            "kind": "store_refusal", "refusal": {
                                "type": "session_state_version_newer_than_runtime",
                                "found": lash_core::store::CURRENT_SESSION_STATE_VERSION + 1,
                                "current": lash_core::store::CURRENT_SESSION_STATE_VERSION,
                            },
                        }),
                    ),
                    Kind::Corrupt => (
                        "runtime_store_corrupt",
                        serde_json::json!({
                            "kind": "stored_data_corrupt", "record_kind": "store-fault-sweep",
                            "message": "injected unreadable record",
                        }),
                    ),
                    _ => unreachable!("terminal fault"),
                };
                let typed = matches!(answer, Answer::Refused { code, terminal: true, cause }
                    if code == expected_code && (matches!(scenario, Scenario::Child) || cause.as_ref() == Some(&expected_cause)));
                // Met after the run's answer was published, the fault leaves
                // the published answer standing.
                if !typed && !unchanged {
                    return Err(format!(
                        "expected terminal {expected_code} with its cause, or the published answer"
                    ));
                }
            }
        }
        Ok(())
    }

    async fn sweep(storage: Storage, scenario: Scenario, kinds: &[Kind]) {
        let clean = run(storage, scenario, None).await;
        assert!(
            matches!(clean.answer, Ok(Answer::Success(_))),
            "clean run: {clean:?}"
        );
        let keys = if matches!(scenario, Scenario::Admission) {
            vec!["sweep-a", "sweep-b", "sweep-c"]
        } else if matches!(scenario, Scenario::Child) {
            vec!["sweep-child"]
        } else {
            vec!["sweep-a"]
        };
        assert!(
            clean.inputs.is_empty(),
            "the clean run leaves no pending inputs"
        );
        let expected = if matches!(scenario, Scenario::Admission) {
            vec![
                "echo: first".to_owned(),
                "echo: second".to_owned(),
                "echo: third".to_owned(),
            ]
        } else {
            vec!["echo: first".to_owned()]
        };
        assert_eq!(
            clean.answer,
            Ok(Answer::Success(expected.clone())),
            "the clean run answers the submitted input"
        );
        let texts = if matches!(scenario, Scenario::Admission) {
            vec!["first", "second", "third"]
        } else {
            vec!["first"]
        };
        let transcript = texts
            .into_iter()
            .zip(expected)
            .flat_map(|(input, answer)| {
                [
                    ("user".to_owned(), input.to_owned()),
                    ("assistant".to_owned(), answer),
                ]
            })
            .collect::<Vec<_>>();
        assert_eq!(
            clean.transcript, transcript,
            "the clean transcript records each input and answer once"
        );
        assert_eq!(
            clean.applications, keys,
            "the clean run applies each requested input once"
        );
        let ops: BTreeMap<_, _> = clean
            .trace
            .iter()
            .filter(|call| call.phase == Phase::Before)
            .map(|call| (call.op.name(), call.op))
            .collect();
        assert!(!ops.is_empty(), "the clean trace must produce cells");
        #[expect(
            clippy::disallowed_methods,
            reason = "the test host selects one fault cell for diagnosis"
        )]
        let filter = std::env::var("LASH_STORE_FAULT_CELL").ok();
        let mut failures = Vec::new();
        let mut executed = 0;
        for op in ops.values() {
            for &kind in kinds {
                let cell = Cell { op: *op, kind };
                if filter
                    .as_ref()
                    .is_some_and(|filter| *filter != cell.to_string())
                {
                    continue;
                }
                executed += 1;
                let result = std::panic::AssertUnwindSafe(run(storage, scenario, Some(cell)))
                    .catch_unwind()
                    .await;
                match result {
                    Ok(run) => {
                        if let Err(reason) = verdict(scenario, cell, &clean, &run) {
                            let trace = run
                                .trace
                                .iter()
                                .map(Call::to_string)
                                .collect::<Vec<_>>()
                                .join("\n");
                            failures.push(format!(
                                "cell = {cell}: {reason}\nanswer = {:?}\npass 0 = {:?}\ntranscript = {:?}\npass 0 transcript = {:?}\napplications = {:?}\npass 0 applications = {:?}\ninputs = {:?}\ntrace:\n{trace}",
                                run.answer,
                                clean.answer,
                                run.transcript,
                                clean.transcript,
                                run.applications,
                                clean.applications,
                                run.inputs
                            ));
                        }
                    }
                    Err(panic) => {
                        let reason = panic
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| panic.downcast_ref::<&str>().copied())
                            .unwrap_or("cell panicked");
                        failures.push(format!("cell = {cell}: {reason}"));
                    }
                }
            }
        }
        assert!(executed > 0, "no cells matched {filter:?}");
        println!(
            "store fault sweep {storage:?}/{scenario:?}: {executed} cells, {} failed",
            failures.len()
        );
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    // Returned-data corruption needs an argument-aware decorator. Script
    // injects errors; this fixture proves the loader detects a foreign window.
    struct ForeignWindow {
        inner: Arc<dyn lash_core::DeploymentStore>,
        armed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl lash_core::store::RuntimeStoreDecorator for ForeignWindow {
        type Inner = dyn lash_core::DeploymentStore;
        fn inner(&self) -> &Self::Inner {
            self.inner.as_ref()
        }
        async fn load_session_window(
            &self,
            id: &SessionId,
            selector: lash_core::store::WindowSelector,
        ) -> std::result::Result<Option<lash_core::store::SessionWindowRead>, StoreError> {
            let mut window = self.inner.load_session_window(id, selector).await?;
            if self.armed.load(Ordering::SeqCst) {
                window.as_mut().expect("committed head").session_id =
                    SessionId::from("another-session");
            }
            Ok(window)
        }
    }
    impl lash_core::DeploymentStoreDecorator for ForeignWindow {}

    fn assert_typed_refusal(error: lash_core::RuntimeError, expected: serde_json::Value) {
        assert_eq!(
            error.code.as_str(),
            expected["type"].as_str().expect("refusal code")
        );
        assert!(error.is_terminal() && !error.is_retryable(), "{error:?}");
        let Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) = &error.cause else {
            panic!("the refusal stays typed: {error:?}");
        };
        assert_eq!(
            serde_json::to_value(refusal).expect("refusal fields"),
            expected
        );
        let plugin = lash_core::PluginError::from(refusal.clone().into_store_error());
        let plugin: lash_core::PluginError =
            serde_json::from_value(serde_json::to_value(plugin).expect("encode the plugin error"))
                .expect("decode the plugin error");
        let controller = lash_core::RuntimeEffectControllerError::from(plugin.clone());
        for plugin in [
            plugin,
            lash_core::PluginError::RuntimeEffectController(controller.clone()),
            lash_core::PluginError::Runtime(controller.into_runtime_error()),
        ] {
            let mapped = plugin.into_turn_failure(lash_core::RuntimeErrorCode::Plugin);
            assert_eq!(mapped.code, error.code);
            assert_eq!(mapped.cause, error.cause);
        }
    }

    /// An input accepted over `stores`, before the engine that serves it
    /// opens the session; the accepting core and the handle.
    async fn accepted(
        stores: &Arc<dyn lash_core::StoreSet>,
        id: &str,
    ) -> (LashCore, crate::SendHandle) {
        let accepting = core_over(stores, false);
        create(&accepting, id).await.expect("create the session");
        let sent = accepting
            .session(SessionId::parse(id).expect("nonblank host identity"))
            .durable()
            .await
            .expect("durable handle")
            .send(TurnInput::text("first"))
            .id(crate::TurnId::parse(format!("{id}-input")).expect("nonblank host identity"))
            .await
            .expect("accept before the open");
        (accepting, sent)
    }

    async fn foreign_window_is_refused(storage: Storage) {
        const SESSION: &str = "typed-open-fault";
        let stores = stores_over(storage).await;
        let armed = Arc::new(AtomicBool::new(false));
        let engine_stores = LayeredStores::over(Arc::clone(&stores.stores))
            .map_session_store_factory(|inner| -> Arc<dyn lash_core::DeploymentStore> {
                Arc::new(ForeignWindow {
                    inner,
                    armed: Arc::clone(&armed),
                })
            })
            .into_store_set();
        let (accepting, sent) = accepted(&stores.stores, SESSION).await;
        armed.store(true, Ordering::SeqCst);
        let serving = core_over(&engine_stores, true);
        let error = tokio::time::timeout(ANSWERS_WITHIN, sent.output())
            .await
            .expect("the open answers")
            .expect_err("the foreign window is refused");
        assert_nothing_parked(&serving, SESSION).await;
        assert_typed_refusal(
            runtime_refusal(SESSION, error),
            serde_json::json!({
                "type": "store_session_mismatch", "loaded": "another-session", "requested": SESSION,
            }),
        );
        let _ = serving.shutdown().await;
        let _ = accepting.shutdown().await;
    }

    async fn unsupported_generation_is_refused(storage: Storage) {
        const SESSION: &str = "unsupported-head";
        let stores = stores_over(storage).await;
        let script = Script::new();
        let (engine_stores, _) = scripted(&stores.stores, &script, "deployment");
        let (accepting, sent) = accepted(&stores.stores, SESSION).await;
        stores
            .seams
            .stamp_session_state_version_for_testing(&SessionId::from(SESSION), 0)
            .await
            .expect("unsupported generation");
        let before = script.calls(StoreOp::read_session_state_version);
        let serving = core_over(&engine_stores, true);
        let error = tokio::time::timeout(ANSWERS_WITHIN, sent.output())
            .await
            .expect("the open answers")
            .expect_err("the generation is refused");
        assert!(
            settled(&serving, SESSION).await,
            "the session actor settles"
        );
        assert_nothing_parked(&serving, SESSION).await;
        assert_typed_refusal(
            runtime_refusal(SESSION, error),
            serde_json::json!({
                "type": "session_state_version_unsupported", "found": 0,
                "current": lash_core::store::CURRENT_SESSION_STATE_VERSION,
            }),
        );
        assert_eq!(
            script.calls(StoreOp::read_session_state_version) - before,
            1,
            "a permanent refusal is not read again"
        );
        let _ = serving.shutdown().await;
        let _ = accepting.shutdown().await;
    }

    /// The engine's open meets an unreadable head: the sender is refused
    /// once, as corrupt stored data, and the head is not read again.
    async fn undecodable_head_is_refused_corrupt(storage: Storage) {
        const SESSION: &str = "undecodable-head";
        let stores = stores_over(storage).await;
        let script = Script::new();
        let (engine_stores, inner) = scripted(&stores.stores, &script, "deployment");
        let (accepting, sent) = accepted(&stores.stores, SESSION).await;
        let id = SessionId::from(SESSION);
        let generation = inner
            .read_session_state_version(&id)
            .await
            .expect("the session's generation");
        stores
            .seams
            .stamp_session_state_version_and_corrupt_payload_for_testing(&id, generation)
            .await
            .expect("corrupt the stored head");
        let version_reads = script.calls(StoreOp::read_session_state_version);
        let window_reads = script.calls(StoreOp::load_session_window);
        let serving = core_over(&engine_stores, true);
        let answer = tokio::time::timeout(ANSWERS_WITHIN, sent.output())
            .await
            .expect("the open answers");
        assert!(
            settled(&serving, SESSION).await,
            "the session actor settles"
        );
        assert_nothing_parked(&serving, SESSION).await;
        let error = runtime_refusal(SESSION, answer.expect_err("the open is refused"));
        assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::RuntimeStoreCorrupt,
            "{error:?}"
        );
        assert!(
            error.message.contains("SessionHeadMeta") && error.message.contains("corrupt"),
            "the refusal names the corrupt record: {error:?}"
        );
        assert!(error.is_terminal() && !error.is_retryable(), "{error:?}");
        assert_eq!(
            (
                script.calls(StoreOp::read_session_state_version) - version_reads,
                script.calls(StoreOp::load_session_window) - window_reads,
            ),
            (1, 0),
            "corrupt stored data is not read again"
        );
        let _ = serving.shutdown().await;
        let _ = accepting.shutdown().await;
    }

    /// A session-turn process whose child is committed under a generation
    /// outside this worker's window: the reopen's refusal ends the process
    /// with its own code on the first attempt.
    async fn committed_child_outside_the_window_ends_its_process(storage: Storage) {
        const CHILD: &str = "committed-child-outside-the-window";
        let stores = stores_over(storage).await;
        let script = Script::new();
        let (engine_stores, _) = scripted(&stores.stores, &script, "deployment");
        let core = core_over(&engine_stores, true);
        create(&core, CHILD).await.expect("commit the child");
        let newer = lash_core::store::CURRENT_SESSION_STATE_VERSION + 1;
        stores
            .seams
            .stamp_session_state_version_for_testing(&SessionId::from(CHILD), newer)
            .await
            .expect("stamp a generation this worker cannot read");
        let version_reads = script.calls(StoreOp::read_session_state_version);
        let started = core
            .processes()
            .start(
                lash_core::ProcessStartRequest::new(
                    lash_core::ProcessInput::SessionTurn {
                        definition_key: "fig4628-session-turn".into(),
                        create_request: Box::new(
                            lash_core::SessionCreateRequest::root(
                                lash_core::SessionStartPoint::Empty,
                                lash_core::PluginOptions::default(),
                            )
                            .with_spec(&mock_session_spec())
                            .expect("a root spec")
                            .with_session_id(CHILD),
                        ),
                        turn_input: Box::new(TurnInput::text("run the committed child")),
                        result: lash_core::SessionTurnOutcome::Turn,
                    },
                    lash_core::ProcessOriginator::host(),
                    lash_core::Lifetime::Detached,
                )
                .with_host_start_key("fig4628-committed-child"),
                core.effect_host(),
            )
            .await
            .expect("the start is admitted");
        let output = tokio::time::timeout(
            ANSWERS_WITHIN,
            core.processes().await_output(&started.process_id),
        )
        .await
        .expect("the process ends")
        .expect("the process's terminal output");
        let lash_core::ProcessAwaitOutput::Settled { output } = output else {
            panic!("the process settles the refusal: {output:?}");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("the process fails: {:?}", output.outcome);
        };
        assert_eq!(
            failure.code,
            lash_core::RuntimeErrorCode::SessionStateVersionNewerThanRuntime.as_str(),
            "{failure:?}"
        );
        assert_nothing_parked(&core, CHILD).await;
        assert_eq!(
            script.calls(StoreOp::read_session_state_version) - version_reads,
            1,
            "a permanent refusal is not retried"
        );
        let _ = core.shutdown().await;
    }

    macro_rules! laws {
        ($module:ident, $storage:expr $(, $ignore:meta)?) => {
            mod $module {
                use super::*;
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn open_and_send_transient_fault_cells() { sweep($storage, Scenario::Open, RECOVERED).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn runtime_assembly_transient_fault_cells() { sweep($storage, Scenario::Assembly, RECOVERED).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn single_and_batch_admission_transient_fault_cells() { sweep($storage, Scenario::Admission, RECOVERED).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn committed_child_reopen_transient_fault_cells() { sweep($storage, Scenario::Child, RECOVERED).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a permanent or corrupt cell parks the session actor and leaves the sender waiting"]
                async fn open_and_send_fault_cells() { sweep($storage, Scenario::Open, EVERY).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a permanent or corrupt cell parks the session actor and leaves the sender waiting"]
                async fn runtime_assembly_fault_cells() { sweep($storage, Scenario::Assembly, EVERY).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a permanent or corrupt cell parks the session actor and leaves the sender waiting"]
                async fn single_and_batch_admission_fault_cells() { sweep($storage, Scenario::Admission, EVERY).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a permanent or corrupt cell parks the session actor and leaves the sender waiting"]
                async fn committed_child_reopen_fault_cells() { sweep($storage, Scenario::Child, EVERY).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the sender waiting"]
                async fn wrong_session_refusal_reaches_the_sender() { foreign_window_is_refused($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the sender waiting"]
                async fn unsupported_generation_refusal_reaches_the_sender() { unsupported_generation_is_refused($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the sender waiting"]
                async fn an_undecodable_head_is_refused_corrupt_and_not_retried() { undecodable_head_is_refused_corrupt($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                #[ignore = "FIG-5358: a terminal store refusal met by the open parks the session actor and leaves the process waiting"]
                async fn a_committed_child_outside_the_window_ends_its_process_typed() { committed_child_outside_the_window_ends_its_process($storage).await; }
            }
        };
    }
    laws!(sqlite_memory, Storage::Memory);
    laws!(sqlite_file, Storage::File);
    laws!(
        postgres,
        Storage::Postgres,
        ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"
    );
}
