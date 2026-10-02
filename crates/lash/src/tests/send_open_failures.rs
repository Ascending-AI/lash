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
//! - a permanent catalog refusal without a typed carrier, or a plugin that
//!   refuses to build: `PluginSessionManager`, naming the refusal;
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
        core.session(id)
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
    let durable = core.session(id.as_str()).durable().await?;
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

mod typed_open {
    use super::*;
    use lash_core::store::{SessionWindowRead, WindowSelector};
    use std::sync::atomic::AtomicUsize;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Read {
        /// No read fails: the store only counts.
        Nothing,
        Catalog,
        Version,
        Window,
        /// The catalog lookup runtime assembly makes to bind the loaded
        /// state to its store: the first lookup after a window load.
        Assembly,
    }

    #[derive(Clone, Copy, Debug)]
    enum Fault {
        WrongSession,
        UnsupportedGeneration,
        NewerGeneration,
        StorageFailure,
        Backend,
    }

    struct OpenStore {
        inner: Arc<dyn DeploymentStore>,
        fault: Fault,
        read: Read,
        armed: AtomicBool,
        remaining: AtomicUsize,
        attempts: AtomicUsize,
        faults: AtomicUsize,
        /// A window was loaded since the last assembly lookup.
        window_loaded: AtomicBool,
        /// Window loads and generation reads since the store was armed.
        window_reads: AtomicUsize,
        version_reads: AtomicUsize,
    }

    impl OpenStore {
        fn over(inner: Arc<dyn DeploymentStore>, fault: Fault, read: Read) -> Arc<Self> {
            Arc::new(Self {
                inner,
                fault,
                read,
                armed: AtomicBool::new(false),
                remaining: AtomicUsize::new(2),
                attempts: AtomicUsize::new(0),
                faults: AtomicUsize::new(0),
                window_loaded: AtomicBool::new(false),
                window_reads: AtomicUsize::new(0),
                version_reads: AtomicUsize::new(0),
            })
        }

        fn armed(&self) -> bool {
            self.armed.load(Ordering::SeqCst)
        }

        fn fails(&self, read: Read) -> bool {
            if !self.armed.load(Ordering::SeqCst) || self.read != read {
                return false;
            }
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let fails = match self.fault {
                Fault::StorageFailure | Fault::Backend => self
                    .remaining
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok(),
                _ => true,
            };
            if fails {
                self.faults.fetch_add(1, Ordering::SeqCst);
            }
            fails
        }

        fn error(&self) -> StoreError {
            let current = lash_core::store::CURRENT_SESSION_STATE_VERSION;
            match self.fault {
                Fault::UnsupportedGeneration => lash_core::store::resolve_session_state_version(
                    Some(0),
                    self.inner.fleet_format(),
                )
                .expect_err("an unsupported session generation"),
                Fault::NewerGeneration => lash_core::store::resolve_session_state_version(
                    Some(current + 1),
                    self.inner.fleet_format(),
                )
                .expect_err("a session generation newer than this build"),
                Fault::StorageFailure => StoreError::StorageFailure {
                    backend: "open-law",
                    message: "the store is temporarily unavailable".into(),
                },
                Fault::Backend => {
                    StoreError::Backend("the store is temporarily unavailable".into())
                }
                Fault::WrongSession => unreachable!("the wrong session is a returned window"),
            }
        }
    }

    #[async_trait]
    impl lash_core::store::RuntimeStoreDecorator for OpenStore {
        type Inner = dyn DeploymentStore;

        fn inner(&self) -> &Self::Inner {
            self.inner.as_ref()
        }

        async fn lookup_session(
            &self,
            id: &SessionId,
        ) -> std::result::Result<lash_core::store::SessionLookup, StoreError> {
            let assembly = self.read == Read::Assembly
                && self.armed()
                && self.window_loaded.swap(false, Ordering::SeqCst);
            if self.fails(Read::Catalog) || (assembly && self.fails(Read::Assembly)) {
                return Err(self.error());
            }
            self.inner.lookup_session(id).await
        }

        async fn read_session_state_version(
            &self,
            id: &SessionId,
        ) -> std::result::Result<u32, StoreError> {
            if self.armed() {
                self.version_reads.fetch_add(1, Ordering::SeqCst);
            }
            if self.fails(Read::Version) {
                return Err(self.error());
            }
            self.inner.read_session_state_version(id).await
        }

        async fn load_session_window(
            &self,
            id: &SessionId,
            selector: WindowSelector,
        ) -> std::result::Result<Option<SessionWindowRead>, StoreError> {
            if self.armed() {
                self.window_reads.fetch_add(1, Ordering::SeqCst);
                self.window_loaded.store(true, Ordering::SeqCst);
            }
            let wrong_session = self.fails(Read::Window);
            let mut read = self.inner.load_session_window(id, selector).await?;
            if wrong_session {
                read.as_mut().expect("the session has a head").session_id =
                    SessionId::from("another-session");
            }
            Ok(read)
        }
    }

    impl lash_core::DeploymentStoreDecorator for OpenStore {}

    #[derive(Clone, Copy)]
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

    async fn send_with_fault(
        storage: Storage,
        fault: Fault,
        read: Read,
    ) -> (Result<crate::TurnOutput>, Arc<OpenStore>) {
        let (double, _held, _seams) = double_over(storage).await;
        let store = OpenStore::over(double.lash_backend().session_store_factory(), fault, read);
        let backend = DecoratedBackend::over(double.lash_backend()).session_store_factory({
            let store = Arc::clone(&store);
            move |_| store
        });
        let core = builder(backend.into())
            .build(crate::testing::runtime_lease_owner())
            .expect("the core over the decorated store");
        const ID: &str = "typed-open-fault";
        crate::tests::create_catalog_session(&core, ID)
            .await
            .expect("create the session");
        let durable = core
            .session(ID)
            .durable()
            .await
            .expect("acquire before the fault");
        durable
            .pending_turn_inputs()
            .await
            .expect("resolve before the fault");
        store.armed.store(true, Ordering::SeqCst);
        let answer = tokio::time::timeout(
            ANSWERS_WITHIN,
            durable
                .send(TurnInput::text("accepted before the open fault"))
                .output(),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{fault:?} at {read:?}: no answer, {:?}",
                invocations(&double)
            )
        });
        tokio::time::timeout(
            ANSWERS_WITHIN,
            double.settle_session_drive(&SessionId::from(ID)),
        )
        .await
        .expect("the drive ends without a paused invocation");
        assert_nothing_paused(&double);
        (answer, store)
    }

    async fn permanent_refusal(storage: Storage, fault: Fault) {
        let read = match fault {
            Fault::WrongSession => Read::Window,
            _ => Read::Version,
        };
        permanent_refusal_at(storage, fault, read).await;
    }

    async fn permanent_refusal_at(storage: Storage, fault: Fault, read: Read) {
        let expected = match fault {
            Fault::WrongSession => serde_json::json!({
                "type": "store_session_mismatch",
                "loaded": "another-session",
                "requested": "typed-open-fault",
            }),
            Fault::UnsupportedGeneration => serde_json::json!({
                "type": "session_state_version_unsupported",
                "found": 0,
                "current": lash_core::store::CURRENT_SESSION_STATE_VERSION,
            }),
            Fault::NewerGeneration => serde_json::json!({
                "type": "session_state_version_newer_than_runtime",
                "found": lash_core::store::CURRENT_SESSION_STATE_VERSION + 1,
                "current": lash_core::store::CURRENT_SESSION_STATE_VERSION,
            }),
            _ => unreachable!("a permanent refusal"),
        };
        let (answer, store) = send_with_fault(storage, fault, read).await;
        let error = drive_refusal("typed-open-fault", answer.expect_err("the open is refused"));
        assert_eq!(
            error.code.as_str(),
            expected["type"].as_str().unwrap(),
            "{error:?}"
        );
        let Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) = &error.cause else {
            panic!("the sender receives a typed store refusal: {error:?}");
        };
        assert_eq!(error.code, refusal.code());
        assert_eq!(serde_json::to_value(refusal).unwrap(), expected);
        assert!(error.is_terminal() && !error.is_retryable(), "{error:?}");
        assert_eq!(
            store.attempts.load(Ordering::SeqCst),
            1,
            "a permanent refusal is not retried"
        );

        let plugin = lash_core::PluginError::from(refusal.clone().into_store_error());
        let plugin: lash_core::PluginError = serde_json::from_value(
            serde_json::to_value(plugin).expect("journal the plugin refusal"),
        )
        .expect("replay the plugin refusal");
        let controller = lash_core::RuntimeEffectControllerError::from(plugin.clone());
        for plugin in [
            plugin,
            lash_core::PluginError::RuntimeEffectController(controller.clone()),
            lash_core::PluginError::Runtime(controller.into_runtime_error()),
        ] {
            let runtime = plugin.into_turn_failure(lash_core::RuntimeErrorCode::Plugin);
            assert_eq!(runtime.code, error.code);
            assert_eq!(runtime.cause, error.cause);
        }
    }

    async fn transient_fault_recovers(storage: Storage) {
        for fault in [Fault::StorageFailure, Fault::Backend] {
            for read in [Read::Catalog, Read::Version] {
                let (answer, store) = send_with_fault(storage, fault, read).await;
                let output =
                    answer.unwrap_or_else(|error| panic!("{fault:?} at {read:?}: {error:?}"));
                assert!(
                    output.is_success(),
                    "{fault:?} at {read:?}: {:?}",
                    output.result
                );
                assert_eq!(
                    store.faults.load(Ordering::SeqCst),
                    2,
                    "both faults were exercised"
                );
                assert!(
                    store.attempts.load(Ordering::SeqCst) >= 3,
                    "the store recovered on a retry"
                );
            }
        }
    }

    /// A fault only at the lookup runtime assembly makes, after the open's
    /// catalog and state reads succeeded, is retried like theirs.
    async fn transient_assembly_fault_recovers(storage: Storage) {
        for fault in [Fault::StorageFailure, Fault::Backend] {
            let (answer, store) = send_with_fault(storage, fault, Read::Assembly).await;
            let output = answer.unwrap_or_else(|error| panic!("{fault:?} at assembly: {error:?}"));
            assert!(output.is_success(), "{fault:?}: {:?}", output.result);
            assert_eq!(
                store.faults.load(Ordering::SeqCst),
                2,
                "both faults met the assembly lookup"
            );
            assert!(
                store.attempts.load(Ordering::SeqCst) >= 3,
                "the store recovered on a retry"
            );
        }
    }

    /// The session's head is bytes its decoder refuses: the drive is
    /// refused once, as corrupt stored data, and the sender is answered. The
    /// drive's admission reads the session's store before any runtime of it
    /// opens (FIG-4755), so its generation gate is the read that meets the
    /// head, and no open follows.
    async fn undecodable_head_is_refused_corrupt(storage: Storage) {
        const ID: &str = "undecodable-head";
        let (double, _held, seams) = double_over(storage).await;
        let store = OpenStore::over(
            double.lash_backend().session_store_factory(),
            Fault::StorageFailure,
            Read::Nothing,
        );
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
            .inner
            .read_session_state_version(&id)
            .await
            .expect("the session's generation");
        seams
            .stamp_session_state_version_and_corrupt_payload_for_testing(&id, generation)
            .await
            .expect("corrupt the stored head");
        store.armed.store(true, Ordering::SeqCst);
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
                store.version_reads.load(Ordering::SeqCst),
                store.window_reads.load(Ordering::SeqCst)
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
        let store = OpenStore::over(
            double.lash_backend().session_store_factory(),
            Fault::StorageFailure,
            Read::Nothing,
        );
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
        store.armed.store(true, Ordering::SeqCst);
        let handler: &'static lash_restate_test::OpenHandler = Box::leak(Box::new(
            double
                .open_handler(lash_core::AdmittedScope::runtime_operation(
                    "start-over-a-committed-child",
                ))
                .await
                .expect("open the host operation's handler"),
        ));
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
            store.version_reads.load(Ordering::SeqCst),
            1,
            "a permanent refusal is not retried: {:?}",
            invocations(&double)
        );
    }

    macro_rules! laws {
        ($module:ident, $storage:expr $(, $ignore:meta)?) => {
            mod $module {
                use super::*;
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn wrong_session_refusal_reaches_the_sender() {
                    permanent_refusal($storage, Fault::WrongSession).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn unsupported_generation_refusal_reaches_the_sender() {
                    permanent_refusal($storage, Fault::UnsupportedGeneration).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn newer_generation_refusal_reaches_the_sender() {
                    permanent_refusal($storage, Fault::NewerGeneration).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn transient_open_faults_retry_until_the_store_recovers() {
                    transient_fault_recovers($storage).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn a_transient_fault_at_the_assembly_lookup_is_retried() {
                    transient_assembly_fault_recovers($storage).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn a_typed_refusal_at_the_assembly_lookup_reaches_the_sender() {
                    permanent_refusal_at($storage, Fault::NewerGeneration, Read::Assembly).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn an_undecodable_head_is_refused_corrupt_and_not_retried() {
                    undecodable_head_is_refused_corrupt($storage).await;
                }
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn a_committed_child_outside_the_window_ends_its_process_typed() {
                    committed_child_outside_the_window_ends_its_process($storage).await;
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
