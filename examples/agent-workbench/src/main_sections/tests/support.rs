use super::*;

/// Read the durable ancestry in bounded pages, including earlier frames that
/// are deliberately absent from the resident read view.
pub(crate) async fn durable_history_messages(
    state: &AppState,
    session_id: &lash::SessionId,
) -> Vec<lash::messages::Message> {
    let runtime_store: Arc<dyn lash::persistence::RuntimeStore> =
        state.session_store_factory.clone();
    let store = lash::persistence::SessionStore::new(runtime_store, session_id.clone())
        .expect("valid session id");
    let mut anchor = lash::persistence::HistoryAnchor::Head;
    let mut messages = Vec::new();
    loop {
        let page = store
            .load_ancestors(
                anchor,
                lash::persistence::HistoryBudget {
                    max_nodes: std::num::NonZeroU32::new(128).expect("positive node budget"),
                    max_bytes: std::num::NonZeroU64::new(32 * 1024 * 1024)
                        .expect("positive byte budget"),
                },
            )
            .await
            .expect("read durable history page");
        messages.extend(
            page.nodes
                .into_iter()
                .filter_map(|node| match node.record.payload {
                    lash::persistence::SessionNodePayload::Event {
                        event: lash::persistence::SessionHistoryRecord::Conversation(message),
                    } => Some(message.to_message()),
                    _ => None,
                }),
        );
        match page.next {
            Some(next) => anchor = lash::persistence::HistoryAnchor::Cursor(next),
            None => break,
        }
    }
    messages
}

/// The sessions root of a test data directory, created if absent: the root a
/// store the test opens beside its core keeps its files under.
pub(crate) fn sessions_root(data_dir: &std::path::Path) -> std::path::PathBuf {
    let root = data_dir.join("lash-sessions");
    std::fs::create_dir_all(&root).expect("create the test sessions root");
    root
}

/// The trigger store of a fresh SQLite memory store set.
pub(crate) fn memory_trigger_store() -> Arc<lash_sqlite_store::SqliteTriggerStore> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set")
            .trigger_store()
    })
}

/// The Restate double a workbench test runs on (FIG-3600 S5c): lash-restate's
/// engine and services over a fresh SQLite memory store set, connected to an
/// in-process server double. Its engine drives every accepted input through
/// its `LashSession` service, as a deployment's does.
///
/// Keep the returned double alive to the end of the test (FIG-3723); hand
/// `double.lash_backend()` to the core. Every core a test builds over one
/// double shares its stores, so a later core reopens what an earlier wrote.
pub(crate) async fn test_double_backend(seed: u64) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate double")
}

/// Serve `core`'s processes on `double`: the double's `LashProcessWorkflow`
/// runs each process segment on this worker, as a deployment's endpoint runs
/// them on the worker it was built with.
pub(crate) fn install_test_process_worker(
    double: &lash_restate_test::RestateTestBackend,
    core: &lash::LashCore,
) {
    double.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the test core's process worker config"),
        )
        .expect("a valid test process worker"),
    );
}

/// Open `session_id` once the engine's drive of it released the session: a
/// drive that just settled a root may still hold the session's store for a
/// moment, and an open meanwhile is refused as contended.
pub(crate) async fn open_session_once_released(
    core: &lash::LashCore,
    session_id: &str,
) -> lash::LashSession {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match core.session(session_id).open().await {
                Ok(session) => return session,
                Err(error) if crate::session_open_is_contended(&error) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("open session `{session_id}`: {error:?}"),
            }
        }
    })
    .await
    .expect("the engine's drive releases the session")
}

/// Work a test runs under a handler's scoped controller.
pub(crate) type HandlerWork<T> = Arc<
    dyn for<'a> Fn(
            lash::runtime::ScopedEffectController<'a>,
        ) -> std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>
        + Send
        + Sync,
>;

/// Run `work` under `admitted` inside a handler of `double`'s deployment and
/// return what its last execution produced: a trigger emission, like a
/// session close, journals its effects, so on the Restate engine it runs in
/// a handler, as the workbench's own workflows run it.
pub(crate) async fn run_in_test_handler<T: Send + 'static>(
    double: &lash_restate_test::RestateTestBackend,
    admitted: lash::runtime::AdmittedScope,
    work: HandlerWork<T>,
) -> T {
    let outcome = Arc::new(Mutex::new(None));
    double
        .run_in_handler(
            admitted,
            Arc::new({
                let outcome = Arc::clone(&outcome);
                move |scoped| {
                    let outcome = Arc::clone(&outcome);
                    let work = Arc::clone(&work);
                    Box::pin(async move {
                        let value = work(scoped).await;
                        *outcome.lock_recover() = Some(value);
                    })
                }
            }),
        )
        .await
        .expect("the test handler completed");
    outcome
        .lock_recover()
        .take()
        .expect("the test handler produced its outcome")
}

/// One session deletion issued from a handler: `attempt` receives the
/// deletion's context and reports its outcome.
pub(crate) type SessionDeleteAttempt = Arc<
    dyn for<'a> Fn(
            lash::SessionDeleteContext<'a>,
        )
            -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>
        + Send
        + Sync,
>;

pub(crate) use super::session_delete_workflow::run_session_delete_in_handler;

/// Delete `session_id` through `core` inside a handler of `double`'s
/// deployment until the delete is physical. A session whose drive still
/// runs is only closing (ADR 0109 §4), and the delete owes another attempt
/// once that work ends; the test retries its step, as the workbench's delete
/// workflow retries its own.
pub(crate) async fn delete_session_in_handler(
    double: &lash_restate_test::RestateTestBackend,
    core: &lash::LashCore,
    session_id: &SessionId,
) -> Result<(), String> {
    // Each call is one attempt of the delete's obligation, which stalls
    // after its attempt ceiling: a few spaced retries, never a hot loop.
    let mut attempts = 0;
    loop {
        attempts += 1;
        let closing = Arc::new(Mutex::new(None));
        run_session_delete_in_handler(
            double,
            core,
            session_id,
            Arc::new({
                let closing = Arc::clone(&closing);
                move |context| {
                    let closing = Arc::clone(&closing);
                    Box::pin(async move {
                        let deletion = lash::LashCore::delete_session(context)
                            .await
                            .map_err(|error| error.to_string())?;
                        if let lash::SessionDeletion::Closing(state) = deletion {
                            *closing.lock_recover() = Some(format!("{:?}", state.waiting));
                        }
                        Ok(())
                    })
                }
            }),
        )
        .await?;
        let Some(waiting) = closing.lock_recover().take() else {
            return Ok(());
        };
        if attempts >= 5 {
            return Err(format!(
                "session `{session_id}` stayed closing; its delete waits on {waiting}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A backend with its catalog, trigger store or process work replaced, every
/// other port its own, for a test that decorates a store or observes the
/// process-event sink.
pub(crate) struct DecoratedBackend {
    layered: lash::testing::LayeredBackend,
}

impl DecoratedBackend {
    pub(crate) fn over(inner: lash::Backend) -> Self {
        Self {
            layered: lash::testing::LayeredBackend::over(inner),
        }
    }

    /// Serve sessions from `catalog`, a test's recording or fault-injecting
    /// decorator over (or stand-in for) the backend's own catalog.
    pub(crate) fn with_catalog(self, catalog: Arc<dyn lash::persistence::DeploymentStore>) -> Self {
        Self {
            layered: self.layered.map_session_store_factory(|_| catalog),
        }
    }

    /// Keep triggers in `trigger_store`, a test's decorated trigger store.
    pub(crate) fn with_trigger_store(
        self,
        trigger_store: Arc<dyn lash::triggers::TriggerStore>,
    ) -> Self {
        Self {
            layered: self.layered.map_trigger_store(|_| trigger_store),
        }
    }

    /// Drive processes through `wiring`, built over the backend's (possibly
    /// decorated) registry so the two stay one registry.
    pub(crate) fn with_process_work(self, wiring: lash::process::ProcessWorkWiring) -> Self {
        Self {
            layered: self.layered.wire_process_work(|_| wiring),
        }
    }
}

impl From<DecoratedBackend> for lash::Backend {
    fn from(decorated: DecoratedBackend) -> Self {
        decorated.layered.into_backend()
    }
}

/// A process registry of its own under `data_dir`'s sessions root, on
/// `clock` and with `wake_delivery`, for a test that drives registrations and
/// wake deliveries directly rather than through the core.
pub(crate) async fn standalone_process_registry(
    data_dir: &std::path::Path,
    clock: Arc<dyn lash::runtime::Clock>,
    wake_delivery: Option<lash::process::WakeDeliveryConfig>,
) -> Arc<dyn lash::process::ProcessRegistry> {
    let sessions = data_dir.join("lash-sessions");
    std::fs::create_dir_all(&sessions).expect("create the sessions root");
    let registry = lash_sqlite_store::SqliteProcessRegistry::open_with_clock(
        &sessions.join(format!("standalone-registry-{}.db", uuid::Uuid::new_v4())),
        clock,
        sessions.clone(),
    )
    .await
    .expect("open a standalone process registry");
    Arc::new(match wake_delivery {
        Some(config) => registry.with_wake_delivery_config(config),
        None => registry,
    })
}

/// The session catalog of a fresh SQLite memory store set.
pub(crate) fn memory_session_store_factory() -> Arc<lash_sqlite_store::SqliteStore> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set")
            .session_store_factory()
    })
}

pub(crate) fn detached_trigger_store() -> Arc<dyn lash::triggers::TriggerStore> {
    sync_await(async {
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set")
            .trigger_store() as Arc<dyn lash::triggers::TriggerStore>
    })
}

pub(crate) fn run_async_test_on_stack_budget<F, Fut, T>(name: &str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget test thread")
        .join()
        .expect("stack-budget test thread")
}

/// The value `test` returns crosses the join after the law's runtime has
/// dropped: ambient tasks it spawned are gone with the runtime, so fixture
/// data directories a live law hands back can be removed without racing the
/// detached writers that would otherwise recreate them.
pub(crate) fn run_async_test_on_stack_budget_multi_thread<F, Fut, T>(
    name: &str,
    worker_threads: usize,
    test: F,
) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(move || {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .thread_stack_size(STACK_BUDGET_BYTES)
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget multi-thread test thread")
        .join()
        .expect("stack-budget multi-thread test thread")
}
