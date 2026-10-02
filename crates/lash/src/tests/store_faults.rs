//! Clean traces derive store-fault cells for open, assembly, keyed single and
//! batch admission, and a committed child's reopen on every SQL backend.
//! Transient failures and lost replies must preserve the clean answer,
//! transcript and input applications; sticky refusals and corruption must
//! answer typed within a deadline. Corruption met after a root's answer was
//! published leaves that answer standing and is recorded as the session's
//! typed fault (ADR 0109 §9). Only ticketed outcome rows may differ.
//!
//! A send to a session the engine cannot open is answered, never left
//! waiting (FIG-4597).
//!
//! The engine opens a session's runtime for every drive
//! (`core/session_driver.rs`, `open_runtime`). Each way that open ends
//! terminally answers the sender of the input the drive was for, within a
//! bound, with the error that names the cause:
//!
//! - no catalog row, or a deleted one: the facade refuses the send before
//!   anything is accepted, as `UnknownSession` or the store's
//!   `SessionDeleted`;
//! - a catalog row with no head: `SessionCreationUnrecorded` (FIG-4553);
//! - a catalog read the store refuses with a typed refusal: that refusal's
//!   own code and cause;
//! - a permanent catalog refusal without a typed carrier: terminal
//!   `StoreRefused`, naming the refusal;
//! - a plugin that refuses to build: `PluginSessionManager`;
//! - a stored record the store cannot decode: `RuntimeStoreCorrupt`, once;
//! - a transient store fault at the catalog read, the state read or the
//!   lookup runtime assembly makes: the engine retries the open and delivers
//!   the input once the store recovers.
//!
//! Every store read of the open is classified one way (FIG-4628), and so is
//! the reopen of a session-turn process's committed child: a stored
//! generation outside the worker's window ends the process with the typed
//! refusal's code, on its first attempt.
//!
//! A tool source lost under `ToolSourcePolicy::Require` is the runtime
//! build's own refusal; `tool_restore_report.rs` holds its law. A deployment
//! under another Restate authority opens and is refused at its root's
//! admission; `wrong_authority_redeploy.rs` holds that law.

use super::*;
use lash_core::testing::{Script, StoreOp};
use std::sync::atomic::{AtomicBool, Ordering};

const SEED: u64 = 0x4597_0101;

/// How long a send to a session that cannot open may take to answer. The
/// bound turns a sender left waiting into a failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

fn builder(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
}

/// The error a send to `id` is answered with, within [`ANSWERS_WITHIN`].
async fn refusal_of(
    core: &LashCore,
    double: &lash_restate_test::RestateTestBackend,
    id: &str,
) -> EmbedError {
    let sent = tokio::time::timeout(ANSWERS_WITHIN, async {
        core.session(SessionId::fixture(id.to_string()))
            .durable()
            .await?
            .send(TurnInput::text("to a session that cannot open"))
            .output()
            .await
    })
    .await;
    let answer = match sent {
        Ok(answer) => answer,
        Err(_) => panic!(
            "{id}: the send is answered: nothing in {ANSWERS_WITHIN:?}, invocations {:?}",
            invocations(double)
        ),
    };
    let error = match answer {
        Ok(output) => panic!("{id}: the session cannot open, got {:?}", output.result),
        Err(error) => error,
    };
    assert_nothing_paused(double);
    error
}

/// No invocation is left paused: the refusal ended what met it.
fn assert_nothing_paused(double: &lash_restate_test::RestateTestBackend) {
    let paused = invocations(double)
        .into_iter()
        .filter(|invocation| invocation.contains(" paused "))
        .collect::<Vec<_>>();
    assert!(paused.is_empty(), "nothing is left paused: {paused:?}");
}

/// The runtime error the engine's drive refused the send with.
fn drive_refusal(id: &str, error: EmbedError) -> lash_core::RuntimeError {
    match error {
        EmbedError::Runtime(error) => error,
        other => panic!("{id}: the drive's refusal is a runtime error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_an_unknown_session_is_answered_unknown() -> Result<()> {
    const ID: &str = "never-created";
    let double = restate_double(SEED).await;
    let core = builder(double.lash_backend()).build(crate::testing::runtime_lease_owner())?;
    let error = refusal_of(&core, &double, ID).await;
    assert!(
        matches!(&error, EmbedError::UnknownSession { session_id } if session_id.as_str() == ID),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_deleted_session_is_answered_deleted() -> Result<()> {
    const ID: &str = "deleted-before-the-send";
    let double = restate_double(SEED).await;
    let core = builder(double.lash_backend()).build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, ID).await?;
    lash_core::SessionCatalogStore::delete_session(
        core.store_factory.as_ref(),
        &SessionId::from(ID),
    )
    .await
    .expect("delete the catalog session");
    let error = refusal_of(&core, &double, ID).await;
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::SessionDeleted { session_id }) if session_id.as_str() == ID
        ),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_catalog_row_with_no_head_is_answered_creation_unrecorded() -> Result<()> {
    const ID: &str = "row-with-no-head";
    let double = restate_double(SEED).await;
    let core = builder(double.lash_backend()).build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, ID).await?;
    lash_core::store::StoreTestSupport::delete_session_head_for_testing(
        double.stores().session_store_factory().as_ref(),
        &SessionId::from(ID),
    )
    .await
    .map_err(EmbedError::Store)?;
    let error = drive_refusal(ID, refusal_of(&core, &double, ID).await);
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionCreationUnrecorded,
        "{error:?}"
    );
    assert!(error.is_terminal(), "{error:?}");
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

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(self.id())
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
    let double = restate_double(SEED).await;
    let refuse = Arc::new(AtomicBool::new(false));
    let core = builder(double.lash_backend())
        .plugin(Arc::new(RefusingFactory {
            refuse: Arc::clone(&refuse),
        }))
        .build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, ID).await?;
    refuse.store(true, Ordering::SeqCst);
    let error = drive_refusal(ID, refusal_of(&core, &double, ID).await);
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginSessionManager,
        "{error:?}"
    );
    assert!(
        error.message.contains("the plugin refuses to build"),
        "the refusal names the plugin's own: {error:?}"
    );
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

/// The send is accepted, and then the catalog refuses the lookup the
/// engine's open makes: the sender is answered with the drive's refusal.
async fn a_send_whose_drive_meets_a_refusing_catalog_is_answered(
    refusal: CatalogRefusal,
) -> Result<lash_core::RuntimeError> {
    let id = format!("catalog-refuses-{refusal:?}").to_lowercase();
    let double = restate_double(SEED).await;
    let script = Script::new();
    let catalog = script.wrap("catalog", double.lash_backend().session_store_factory());
    let backend = DecoratedBackend::over(double.lash_backend()).session_store_factory({
        let catalog = Arc::clone(&catalog);
        move |_| catalog
    });
    let core = builder(backend.into()).build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, &id).await?;
    let durable = core
        .session(SessionId::fixture(id.clone()))
        .durable()
        .await?;
    // The catalog refuses from before the send, so no drive opens the
    // session ahead of the refusal; the facade's own acquisition was made
    // above.
    durable.pending_turn_inputs().await?;
    script
        .on(StoreOp::lookup_session)
        .from_nth(script.calls(StoreOp::lookup_session) + 1)
        .before()
        .fail(move || refusal.error());
    let answer = tokio::time::timeout(ANSWERS_WITHIN, async {
        durable
            .send(TurnInput::text("accepted before the engine's open"))
            .output()
            .await
    })
    .await;
    let Ok(answer) = answer else {
        panic!(
            "{id}: the send is answered: nothing in {ANSWERS_WITHIN:?}, invocations {:?}",
            invocations(&double)
        );
    };
    let error = match answer {
        Ok(output) => panic!("{id}: the session cannot open, got {:?}", output.result),
        Err(error) => drive_refusal(&id, error),
    };
    assert_nothing_paused(&double);
    Ok(error)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_whose_open_meets_a_typed_store_refusal_is_answered_with_it() -> Result<()> {
    let error =
        a_send_whose_drive_meets_a_refusing_catalog_is_answered(CatalogRefusal::WriterFenced)
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
async fn a_send_whose_open_meets_an_untyped_store_refusal_is_answered_naming_it() -> Result<()> {
    let error =
        a_send_whose_drive_meets_a_refusing_catalog_is_answered(CatalogRefusal::Unsupported)
            .await?;
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
    use lash_core::testing::{Call, Op, Phase, Script, StoreOp};
    use std::collections::BTreeMap;

    const ID: &str = "store-fault-sweep";
    const WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

    #[derive(Clone, Copy, Debug)]
    enum Scenario {
        Open,
        Assembly,
        Admission,
        Child,
    }

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
            codes: Vec<String>,
        },
        Refused {
            code: String,
            cause: Option<serde_json::Value>,
            terminal: bool,
        },
    }

    fn refused(error: EmbedError) -> Answer {
        let error = match error {
            EmbedError::Runtime(error) => error,
            EmbedError::Store(error)
            | EmbedError::Session(lash_core::SessionError::Store { source: error, .. }) => {
                lash_core::RuntimeEffectControllerError::from(error).into_runtime_error()
            }
            EmbedError::Plugin(error) => {
                error.into_turn_failure(lash_core::RuntimeErrorCode::Plugin)
            }
            other => panic!("the sender receives the store's typed error: {other:?}"),
        };
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
                        ("sweep-b", TurnInput::text("second")),
                        ("sweep-c", TurnInput::text("third")),
                    ])
                    .await
            } else {
                durable
                    .send(TurnInput::text("first"))
                    .id("sweep-a")
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
                    codes: output
                        .result
                        .errors
                        .iter()
                        .filter_map(|issue| issue.code.as_ref().map(ToString::to_string))
                        .collect(),
                });
            }
            answers.push(output.assistant_message().expect("echo answer").to_owned());
        }
        Ok(Answer::Success(answers))
    }

    #[derive(Clone, Copy, Debug)]
    enum Storage {
        Memory,
        File,
        Postgres,
    }

    /// The double over `storage`, what its stores need to outlive it, and
    /// the test seams of the session store it runs over.
    async fn double_over(
        storage: Storage,
    ) -> (
        lash_restate_test::RestateTestBackend,
        Box<dyn std::any::Any>,
        Arc<dyn lash_core::store::StoreTestSupport>,
    ) {
        let config = lash_restate_test::ServerConfig::default();
        match storage {
            Storage::Memory => {
                let double = restate_double(SEED).await;
                let seams = double.stores().session_store_factory();
                (double, Box::new(()), seams)
            }
            Storage::File => {
                let files = tempfile::tempdir().expect("SQLite store directory");
                let stores = lash_sqlite_store::SqliteStoreSet::open(files.path())
                    .await
                    .expect("SQLite file stores");
                let seams = stores.session_store_factory();
                let stores: Arc<dyn lash_core::StoreSet> = Arc::new(stores);
                (
                    lash_restate_test::backend_with(SEED, config, move |_| stores)
                        .await
                        .expect("the double over SQLite files"),
                    Box::new(files),
                    seams,
                )
            }
            Storage::Postgres => {
                let url = lash_postgres_store::testing::required_database_url();
                let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
                let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                    .await
                    .expect("connect to PostgreSQL");
                let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
                let stores = lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core::facade_support::FileAttachmentStore::new(
                        attachments.path(),
                    )),
                );
                let seams = stores.session_store_factory();
                let stores: Arc<dyn lash_core::StoreSet> = Arc::new(stores);
                (
                    lash_restate_test::backend_with(SEED, config, move |_| stores)
                        .await
                        .expect("the double over PostgreSQL"),
                    Box::new((database, attachments, storage)),
                    seams,
                )
            }
        }
    }

    #[derive(Debug)]
    struct Run {
        answer: std::result::Result<Answer, String>,
        transcript: Vec<(String, String)>,
        applications: Vec<String>,
        inputs: Vec<String>,
        trace: Vec<Call>,
        paused: Vec<String>,
        faults: Vec<lash_core::store::SessionFault>,
    }

    const FAULTS: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(7);

    async fn run(storage: Storage, scenario: Scenario, cell: Option<Cell>) -> Run {
        let (double, _held, _seams) = double_over(storage).await;
        let script = Script::new();
        let inner = double.lash_backend().session_store_factory();
        let store = script.wrap("deployment", Arc::clone(&inner));
        let backend =
            DecoratedBackend::over(double.lash_backend()).session_store_factory(move |_| store);
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("fixture core");
        crate::tests::create_catalog_session(&core, ID)
            .await
            .expect("commit the session");
        crate::tests::harness::serve_processes_on(&double, &core);
        let durable = core.session(ID).durable().await.expect("durable handle");
        durable
            .pending_turn_inputs()
            .await
            .expect("resolve before arming");
        if matches!(scenario, Scenario::Child) {
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
        let mut ready = None;
        let mut open_hold = None;
        let mut accepted = None;
        if matches!(scenario, Scenario::Open | Scenario::Assembly) {
            open_hold = Some(double.hold_session_drive(&SessionId::from(ID)).await);
            accepted = Some(
                submit(&durable, false)
                    .await
                    .expect("accept before the open"),
            );
            if matches!(scenario, Scenario::Assembly) {
                let gate = script
                    .on(StoreOp::load_session_window)
                    .nth(script.calls(StoreOp::load_session_window) + 1)
                    .after()
                    .pause();
                open_hold.take().expect("held drive").release();
                gate.reached(1).await;
                ready = Some(gate);
            }
        }
        let start = script.trace().len();
        if let Some(cell) = cell {
            arm(&script, cell);
        }
        if let Some(hold) = open_hold {
            hold.release();
        }
        if let Some(gate) = ready {
            gate.open_all();
        }
        let mut child_source = None;
        let work = async {
            if matches!(scenario, Scenario::Child) {
                // Keep the handler owned by the law. Dropping it before the
                // output would close the parent of the admitted process.
                let handler = double
                    .open_handler(lash_core::AdmittedScope::runtime_operation("sweep-child"))
                    .await
                    .expect("host handler");
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
                        handler.scoped(),
                    )
                    .await?;
                child_source = Some(started.process_id.to_string());
                let output = core.processes().await_output(&started.process_id).await?;
                let lash_core::ProcessAwaitOutput::Settled { output } = output else {
                    panic!("the child settles: {output:?}");
                };
                return Ok(match output.outcome {
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
                        terminal: failure.retry == lash_core::ToolRetryStatus::Never,
                    },
                    other => panic!("the child answers: {other:?}"),
                });
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
        // Finish background delivery before inspecting the durable result.
        // A sticky fault that wedges the engine is evidence, not a hung law.
        let settled =
            tokio::time::timeout(WITHIN, double.settle_session_drive(&SessionId::from(ID))).await;
        let mut answer = answer;
        if settled.is_err() {
            answer = Err("the drive did not settle".into());
        }
        // Corruption met after the answer was published is recorded beside
        // the drive: by the root's scope close, or by its next admission.
        let mut faults = Vec::new();
        if matches!(
            (&answer, cell),
            (
                Ok(Answer::Success(_)),
                Some(Cell {
                    kind: Kind::Corrupt,
                    ..
                })
            )
        ) {
            let recorded = tokio::time::timeout(WITHIN, async {
                loop {
                    let faults = inner
                        .list_session_faults(None, FAULTS)
                        .await
                        .expect("session faults");
                    if !faults.is_empty() {
                        return faults;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await;
            faults = recorded.unwrap_or_default();
        }
        let trace = script.trace()[start..].to_vec();
        let paused = invocations(&double)
            .into_iter()
            .filter(|call| call.contains(" paused "))
            .collect();
        // The oracle reads through the underlying store so sticky faults do
        // not hide the committed evidence they are meant to test.
        let window = inner
            .load_session_window(
                &SessionId::from(ID),
                lash_core::store::WindowSelector::Current,
            )
            .await
            .expect("inspect committed transcript")
            .expect("committed head");
        let transcript = window
            .window
            .read_model()
            .messages
            .iter()
            .map(|message| {
                (
                    crate::turn::message_role(message).into(),
                    message_text(message),
                )
            })
            .collect();
        let mut applications = inner
            .list_turn_input_applications(&SessionId::from(ID))
            .await
            .expect("applications")
            .into_iter()
            .map(|row| {
                let key = row.source_key.expect("keyed input");
                if child_source.as_ref() == Some(&key) {
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
        let result = Run {
            answer,
            transcript,
            applications,
            inputs,
            trace,
            paused,
            faults,
        };
        drop(core);
        drop(script); // An unfired rule fails this cell even if its answer matched.
        result
    }

    fn verdict(
        scenario: Scenario,
        cell: Cell,
        clean: &Run,
        run: &Run,
    ) -> std::result::Result<(), String> {
        if !run.paused.is_empty() {
            return Err(format!("paused invocations: {:?}", run.paused));
        }
        let answer = run.answer.as_ref().map_err(Clone::clone)?;
        match cell.kind {
            Kind::Transient | Kind::LostReply => {
                if run.answer != clean.answer
                    || run.transcript != clean.transcript
                    || run.applications != clean.applications
                    || run.inputs != clean.inputs
                {
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
                let published = run.answer == clean.answer
                    && run.transcript == clean.transcript
                    && run.applications == clean.applications
                    && run.inputs == clean.inputs;
                // The published answer stands, and the corruption is the
                // session's one durable fault, typed as the sender of an
                // unpublished answer would have read it.
                let faulted = matches!(run.faults.as_slice(), [fault]
                    if fault.session_id.as_str() == ID
                        && fault.record.code.as_str() == expected_code
                        && fault.record.cause.as_ref().map(|cause| {
                            serde_json::to_value(cause).expect("typed cause")
                        }) == Some(expected_cause.clone()));
                let background = published
                    && match cell.kind {
                        Kind::Permanent => true,
                        _ => faulted,
                    };
                if !typed && !background {
                    return Err(format!(
                        "expected terminal {} with its cause",
                        expected_code
                    ));
                }
            }
        }
        Ok(())
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct KnownDefect {
        backend: String,
        scenario: String,
        cell: String,
        ticket: String,
        reason_contains: String,
    }

    async fn sweep(storage: Storage, scenario: Scenario) {
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
        let defects: Vec<KnownDefect> =
            serde_json::from_str(include_str!("store_fault_outcomes.json"))
                .expect("ticketed store-fault outcomes");
        assert!(
            env!("CARGO_PKG_VERSION").starts_with("0.") || defects.is_empty(),
            "the store-fault outcome table must be empty at the 1.0 cut"
        );
        let mut row_ids = std::collections::BTreeSet::new();
        for defect in &defects {
            assert!(
                matches!(defect.backend.as_str(), "Memory" | "File" | "Postgres")
                    && matches!(
                        defect.scenario.as_str(),
                        "Open" | "Assembly" | "Admission" | "Child"
                    ),
                "a known defect must name a registered backend and scenario"
            );
            assert!(
                row_ids.insert((&defect.backend, &defect.scenario, &defect.cell)),
                "duplicate store-fault outcome row"
            );
            if defect.backend == format!("{storage:?}")
                && defect.scenario == format!("{scenario:?}")
            {
                let (op, kind) = defect
                    .cell
                    .split_once("#1/")
                    .expect("first-occurrence cell id");
                assert!(
                    ops.contains_key(op)
                        && matches!(kind, "Transient" | "Permanent" | "Corrupt" | "LostReply"),
                    "{} no longer belongs to the clean trace; delete its outcome row",
                    defect.cell
                );
            }
            assert!(
                defect.ticket.strip_prefix("FIG-").is_some_and(
                    |id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
                ) && !defect.reason_contains.is_empty(),
                "a known defect must name its ticket and failure"
            );
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the test host selects one fault cell for diagnosis"
        )]
        let filter = std::env::var("LASH_STORE_FAULT_CELL").ok();
        let mut failures = Vec::new();
        let mut executed = 0;
        for op in ops.values() {
            for kind in [
                Kind::Transient,
                Kind::Permanent,
                Kind::Corrupt,
                Kind::LostReply,
            ] {
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
                let mut failure = None;
                match result {
                    Ok(run) => {
                        if let Err(reason) = verdict(scenario, cell, &clean, &run) {
                            let trace = run
                                .trace
                                .iter()
                                .map(Call::to_string)
                                .collect::<Vec<_>>()
                                .join("\n");
                            failure = Some(format!(
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
                        failure = Some(format!("cell = {cell}: {reason}"));
                    }
                }
                let known = defects.iter().find(|defect| {
                    defect.backend == format!("{storage:?}")
                        && defect.scenario == format!("{scenario:?}")
                        && defect.cell == cell.to_string()
                });
                match (known, failure) {
                    (Some(known), Some(failure)) if failure.contains(&known.reason_contains) => {
                        println!(
                            "known defect {}: {}",
                            known.ticket,
                            failure.lines().take(2).collect::<Vec<_>>().join("; ")
                        )
                    }
                    (Some(known), None) => failures.push(format!(
                        "cell = {cell}: {} is fixed; delete its outcome row",
                        known.ticket
                    )),
                    (_, Some(failure)) => failures.push(failure),
                    (None, None) => {}
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
        inner: Arc<dyn DeploymentStore>,
        armed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl lash_core::store::RuntimeStoreDecorator for ForeignWindow {
        type Inner = dyn DeploymentStore;
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
            serde_json::from_value(serde_json::to_value(plugin).expect("journal plugin"))
                .expect("replay plugin");
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

    async fn foreign_window_is_refused(storage: Storage) {
        const SESSION: &str = "typed-open-fault";
        let (double, _held, _seams) = double_over(storage).await;
        let armed = Arc::new(AtomicBool::new(false));
        let store = Arc::new(ForeignWindow {
            inner: double.lash_backend().session_store_factory(),
            armed: Arc::clone(&armed),
        });
        let backend =
            DecoratedBackend::over(double.lash_backend()).session_store_factory(move |_| store);
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("fixture core");
        crate::tests::create_catalog_session(&core, SESSION)
            .await
            .expect("create the session");
        armed.store(true, Ordering::SeqCst);
        let error = drive_refusal(SESSION, refusal_of(&core, &double, SESSION).await);
        assert_typed_refusal(
            error,
            serde_json::json!({
                "type": "store_session_mismatch", "loaded": "another-session", "requested": SESSION,
            }),
        );
    }

    async fn unsupported_generation_is_refused(storage: Storage) {
        const SESSION: &str = "unsupported-head";
        let (double, _held, seams) = double_over(storage).await;
        let script = Script::new();
        let store = script.wrap("deployment", double.lash_backend().session_store_factory());
        let backend =
            DecoratedBackend::over(double.lash_backend()).session_store_factory(move |_| store);
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("fixture core");
        crate::tests::create_catalog_session(&core, SESSION)
            .await
            .expect("create the session");
        crate::tests::harness::serve_processes_on(&double, &core);
        let durable = core
            .session(SESSION)
            .durable()
            .await
            .expect("durable handle");
        let hold = double.hold_session_drive(&SessionId::from(SESSION)).await;
        let sent = durable
            .send(TurnInput::text("first"))
            .id("unsupported-input")
            .await
            .expect("accept before the head changes");
        seams
            .stamp_session_state_version_for_testing(&SessionId::from(SESSION), 0)
            .await
            .expect("unsupported generation");
        let before = script.calls(StoreOp::read_session_state_version);
        hold.release();
        let error = tokio::time::timeout(ANSWERS_WITHIN, sent.output())
            .await
            .expect("the open answers")
            .expect_err("the generation is refused");
        let error = drive_refusal(SESSION, error);
        assert_nothing_paused(&double);
        assert_typed_refusal(
            error,
            serde_json::json!({
                "type": "session_state_version_unsupported", "found": 0,
                "current": lash_core::store::CURRENT_SESSION_STATE_VERSION,
            }),
        );
        assert_eq!(
            script.calls(StoreOp::read_session_state_version) - before,
            1
        );
    }

    /// The drive's admission meets an unreadable head before a runtime opens:
    /// the sender is refused once, as corrupt stored data.
    async fn undecodable_head_is_refused_corrupt(storage: Storage) {
        const ID: &str = "undecodable-head";
        let (double, _held, seams) = double_over(storage).await;
        let script = Script::new();
        let store = script.wrap("deployment", double.lash_backend().session_store_factory());
        let backend = DecoratedBackend::over(double.lash_backend()).session_store_factory({
            let store = Arc::clone(&store);
            move |_| store
        });
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("the core over the counting store");
        crate::tests::create_catalog_session(&core, ID)
            .await
            .expect("create the session");
        let durable = core
            .session(ID)
            .durable()
            .await
            .expect("acquire before the corruption");
        durable
            .pending_turn_inputs()
            .await
            .expect("resolve before the corruption");
        let id = SessionId::from(ID);
        // The input is accepted over a readable head and its drive is held,
        // so the first read of the corrupt head is the drive's admission.
        let hold = double.hold_session_drive(&id).await;
        let accepted = durable
            .send(TurnInput::text("accepted before the head is corrupt"))
            .await
            .expect("the input is accepted");
        let generation = store
            .inner()
            .read_session_state_version(&id)
            .await
            .expect("the session's generation");
        seams
            .stamp_session_state_version_and_corrupt_payload_for_testing(&id, generation)
            .await
            .expect("corrupt the stored head");
        let version_reads = script.calls(StoreOp::read_session_state_version);
        let window_reads = script.calls(StoreOp::load_session_window);
        hold.release();
        let answer = tokio::time::timeout(ANSWERS_WITHIN, accepted.output())
            .await
            .unwrap_or_else(|_| panic!("no answer, {:?}", invocations(&double)));
        tokio::time::timeout(ANSWERS_WITHIN, double.settle_session_drive(&id))
            .await
            .expect("the drive ends without a paused invocation");
        assert_nothing_paused(&double);
        let error = drive_refusal(ID, answer.expect_err("the drive is refused"));
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
    }

    /// A session-turn process whose child is committed under a generation
    /// outside this worker's window: the reopen's refusal ends the process
    /// with its own code on the first attempt.
    async fn committed_child_outside_the_window_ends_its_process(storage: Storage) {
        const CHILD: &str = "committed-child-outside-the-window";
        let (double, _held, seams) = double_over(storage).await;
        let script = Script::new();
        let store = script.wrap("deployment", double.lash_backend().session_store_factory());
        let backend = DecoratedBackend::over(double.lash_backend()).session_store_factory({
            let store = Arc::clone(&store);
            move |_| store
        });
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("the core over the counting store");
        crate::tests::harness::serve_processes_on(&double, &core);
        crate::tests::create_catalog_session(&core, CHILD)
            .await
            .expect("commit the child");
        let newer = lash_core::store::CURRENT_SESSION_STATE_VERSION + 1;
        seams
            .stamp_session_state_version_for_testing(&SessionId::from(CHILD), newer)
            .await
            .expect("stamp a generation this worker cannot read");
        let version_reads = script.calls(StoreOp::read_session_state_version);
        let handler = double
            .open_handler(lash_core::AdmittedScope::runtime_operation(
                "start-over-a-committed-child",
            ))
            .await
            .expect("open host handler");
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
                handler.scoped(),
            )
            .await
            .expect("the start is admitted");
        let output = tokio::time::timeout(
            ANSWERS_WITHIN,
            core.processes().await_output(&started.process_id),
        )
        .await
        .unwrap_or_else(|_| panic!("the process ends: {:?}", invocations(&double)))
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
        assert_eq!(
            failure.retry,
            lash_core::ToolRetryStatus::Never,
            "{failure:?}"
        );
        assert_nothing_paused(&double);
        assert_eq!(
            script.calls(StoreOp::read_session_state_version) - version_reads,
            1,
            "a permanent refusal is not retried: {:?}",
            invocations(&double)
        );
    }

    /// Corrupt stored data met by a root's owed scope close, after its
    /// answer was published (FIG-4777): the answer stands, the close's
    /// obligation stalls refused instead of retrying, and the session
    /// carries the typed fault, which refuses every later send until an
    /// operator clears it. `cleared` takes the operator's verbs instead of
    /// the refused send, whose input would hold the session's ingress claim.
    async fn closure_corruption_faults_the_session(storage: Storage, cleared: bool) {
        const SESSION: &str = "closure-corruption";
        let (double, _held, _seams) = double_over(storage).await;
        let script = Script::new();
        let store = script.wrap("deployment", double.lash_backend().session_store_factory());
        let backend =
            DecoratedBackend::over(double.lash_backend()).session_store_factory(move |_| store);
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("fixture core");
        crate::tests::create_catalog_session(&core, SESSION)
            .await
            .expect("create the session");
        crate::tests::harness::serve_processes_on(&double, &core);
        let durable = core
            .session(SESSION)
            .durable()
            .await
            .expect("durable handle");
        durable
            .pending_turn_inputs()
            .await
            .expect("resolve before arming");
        // One corrupt read: what refuses the later send is the recorded
        // fault, not the store.
        script
            .on(StoreOp::bound_turn_scopes)
            .nth(script.calls(StoreOp::bound_turn_scopes) + 1)
            .before()
            .fail(|| Kind::Corrupt.error());
        let id = SessionId::from(SESSION);
        let first = tokio::time::timeout(
            ANSWERS_WITHIN,
            durable.send(TurnInput::text("first")).output(),
        )
        .await
        .expect("the first send is answered")
        .expect("the published answer stands");
        assert_eq!(first.assistant_message(), Some("echo: first"));

        let fault = tokio::time::timeout(ANSWERS_WITHIN, async {
            loop {
                let faults = core
                    .session_faults(None, FAULTS)
                    .await
                    .expect("session faults");
                if let [fault] = faults.as_slice() {
                    return fault.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the close records the session's fault");
        let cause = serde_json::json!({
            "kind": "stored_data_corrupt", "record_kind": "store-fault-sweep",
            "message": "injected unreadable record",
        });
        assert_eq!(fault.session_id, id);
        assert_eq!(
            fault.record.code,
            lash_core::RuntimeErrorCode::RuntimeStoreCorrupt
        );
        assert_eq!(
            serde_json::to_value(&fault.record.cause).expect("typed cause"),
            cause
        );
        let lash_core::store::SessionFaultOrigin::ScopeClose { root } = &fault.record.origin else {
            panic!("the scope close met the fault: {fault:?}");
        };

        // The close's obligation stalled on its first attempt, for an
        // operator to re-arm: corrupt data is never retried.
        let stalled = tokio::time::timeout(ANSWERS_WITHIN, async {
            loop {
                let stalled = core
                    .stalled_obligations(lash_core::store::ObligationKind::ScopeClose, None, FAULTS)
                    .await
                    .expect("stalled scope closes");
                if let [stalled] = stalled.as_slice() {
                    return stalled.clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the close's obligation stalls");
        assert_eq!(
            stalled.key,
            Ok(lash_core::store::ObligationKey::ScopeClose {
                session_id: id.clone(),
                root: root.clone(),
            })
        );
        assert_eq!(
            (stalled.reason, stalled.attempts),
            (lash_core::store::StallReason::Refused, 1)
        );
        assert_eq!(
            stalled.last_error.as_ref().map(|error| error.code.clone()),
            Some(lash_core::RuntimeErrorCode::RuntimeStoreCorrupt)
        );
        tokio::time::timeout(ANSWERS_WITHIN, double.settle_session_drive(&id))
            .await
            .expect("the drive ends");
        assert_nothing_paused(&double);

        if !cleared {
            // Further work is refused with the fault's own code and cause.
            let refused = tokio::time::timeout(
                ANSWERS_WITHIN,
                durable.send(TurnInput::text("second")).output(),
            )
            .await
            .expect("the second send is answered")
            .expect_err("a faulted session admits nothing");
            let refused = drive_refusal(SESSION, refused);
            assert_eq!(
                refused.code,
                lash_core::RuntimeErrorCode::RuntimeStoreCorrupt,
                "{refused:?}"
            );
            assert_eq!(
                serde_json::to_value(&refused.cause).expect("typed cause"),
                cause
            );
            assert!(
                refused.is_terminal() && !refused.is_retryable(),
                "{refused:?}"
            );
            assert_eq!(
                script.calls(StoreOp::bound_turn_scopes),
                1,
                "the corrupt read is not retried"
            );
            return;
        }

        // The operator's verbs: the fault clears once, the stalled close is
        // due again, and the session admits.
        assert!(
            core.clear_session_fault(&id)
                .await
                .expect("clear the fault")
        );
        assert!(!core.clear_session_fault(&id).await.expect("clear again"));
        assert!(
            core.rearm_obligation(lash_core::store::ObligationKind::ScopeClose, &stalled.id)
                .await
                .expect("re-arm the close")
        );
        assert!(
            core.session_faults(None, FAULTS)
                .await
                .expect("session faults")
                .is_empty()
        );
        let second = tokio::time::timeout(
            ANSWERS_WITHIN,
            durable.send(TurnInput::text("second")).output(),
        )
        .await
        .unwrap_or_else(|_| panic!("the second send is answered: {:?}", invocations(&double)))
        .expect("a cleared session admits");
        assert_eq!(second.assistant_message(), Some("echo: second"));
    }

    macro_rules! laws {
        ($module:ident, $storage:expr $(, $ignore:meta)?) => {
            mod $module {
                use super::*;
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn open_and_send_fault_cells() { sweep($storage, Scenario::Open).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn runtime_assembly_fault_cells() { sweep($storage, Scenario::Assembly).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn single_and_batch_admission_fault_cells() { sweep($storage, Scenario::Admission).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn committed_child_reopen_fault_cells() { sweep($storage, Scenario::Child).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn wrong_session_refusal_reaches_the_sender() { foreign_window_is_refused($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn unsupported_generation_refusal_reaches_the_sender() { unsupported_generation_is_refused($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn an_undecodable_head_is_refused_corrupt_and_not_retried() { undecodable_head_is_refused_corrupt($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn a_committed_child_outside_the_window_ends_its_process_typed() { committed_child_outside_the_window_ends_its_process($storage).await; }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)] $(#[$ignore])?
                async fn closure_corruption_faults_the_session_until_an_operator_clears_it() {
                    closure_corruption_faults_the_session($storage, false).await;
                    closure_corruption_faults_the_session($storage, true).await;
                }
            }
        };
    }
    laws!(sqlite_memory, Storage::Memory);
    laws!(sqlite_file, Storage::File);
    laws!(
        postgres,
        Storage::Postgres,
        ignore = "requires the PostgreSQL gate"
    );
}
