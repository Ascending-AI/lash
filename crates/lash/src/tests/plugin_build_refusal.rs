//! FIG-5246: a plugin factory that refuses to build its session plugin ends
//! the session's work in a typed failure, never a turn that does not settle.
//! A factory's retryable refusal is retried, and the turn completes once the
//! factory builds.

use super::*;

use lash_core::facade_support::{PluginSpec, PluginSpecFactory};
use lash_core::plugin::PluginDeclaration;

const REFUSING: &str = "build-refusal-probe";
const REFUSAL: &str = "the probe refuses this session";
/// Generous for a mock turn, and far short of a hang.
const SETTLES_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// A factory whose build answers `refusal(attempt)` for each 1-based attempt,
/// and an empty plugin once that answers `None`.
fn probe(
    builds: Arc<AtomicUsize>,
    refusal: impl Fn(usize) -> Option<lash_core::PluginError> + Send + Sync + 'static,
) -> Arc<dyn PluginFactory> {
    Arc::new(PluginSpecFactory::new(
        PluginDeclaration::initial(REFUSING),
        Arc::new(move |_ctx| {
            let attempt = builds.fetch_add(1, Ordering::SeqCst) + 1;
            refusal(attempt).map_or_else(|| Ok(PluginSpec::new()), Err)
        }),
    ))
}

fn core_with(backend: lash_core::Backend, plugin: Arc<dyn PluginFactory>) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(plugin)
        .build(crate::testing::runtime_lease_owner())
        .expect("standard core")
}

async fn postgres_backend() -> (
    lash_core::Backend,
    lash_postgres_store::testing::IsolatedDatabase,
    tempfile::TempDir,
) {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open(
            (attachments.path()).join("attachments.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("SQLite attachment store")
        .attachment_store(),
    ));
    (
        lash_conformance::backend_over(stores),
        database,
        attachments,
    )
}

/// `input`'s send answers within [`SETTLES_WITHIN`] with the factory's
/// refusal as a typed, terminal runtime error.
async fn assert_refused(session: &crate::DurableSession, input: &str) {
    let answer = tokio::time::timeout(
        SETTLES_WITHIN,
        session.send(crate::TurnInput::text(input)).output(),
    )
    .await
    .unwrap_or_else(|_| panic!("the `{input}` turn settles"));
    let Err(EmbedError::Runtime(error)) = answer else {
        panic!("the `{input}` turn answers with the refusal: {answer:?}");
    };
    assert_eq!(error.code, lash_core::RuntimeErrorCode::Plugin, "{error:?}");
    assert!(error.is_terminal(), "{error:?}");
    assert!(error.message.contains(REFUSAL), "{error:?}");
}

/// A refusal the session's recorded config makes permanent: each turn ends
/// refused with the factory's typed refusal, the session admits the next
/// send (so no turn is left in flight), and the core shuts down.
async fn a_refused_plugin_build_fails_the_turn_typed(backend: lash_core::Backend) {
    let builds = Arc::new(AtomicUsize::new(0));
    let core = core_with(
        backend,
        probe(Arc::clone(&builds), |_| {
            Some(lash_core::PluginError::Registration(REFUSAL.into()))
        }),
    );
    let session = core
        .session(lash_sansio::SessionId::try_from("refused-build".to_owned()).expect("id"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("creation records config and builds no plugin");
    assert_refused(&session, "first").await;
    assert_refused(&session, "second").await;
    assert!(builds.load(Ordering::SeqCst) >= 2, "each turn built");
    tokio::time::timeout(SETTLES_WITHIN, core.shutdown())
        .await
        .expect("nothing holds the core open")
        .expect("shutdown");
}

/// A retryable refusal is retried: the factory refuses its first build as an
/// unavailable store, builds on the next, and the turn answers.
async fn a_retryably_refused_plugin_build_completes_the_turn(backend: lash_core::Backend) {
    let builds = Arc::new(AtomicUsize::new(0));
    let core = core_with(
        backend,
        probe(Arc::clone(&builds), |attempt| {
            (attempt == 1).then_some(lash_core::PluginError::StoreUnavailable {
                fault: lash_core_store::store::StoreFault::Contended,
            })
        }),
    );
    let session = core
        .session(lash_sansio::SessionId::try_from("retried-build".to_owned()).expect("id"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    let output = tokio::time::timeout(
        SETTLES_WITHIN,
        session.send(crate::TurnInput::text("hello")).output(),
    )
    .await
    .expect("the turn settles")
    .expect("the turn answers its send");
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.assistant_message(), Some("echo: hello"));
    assert!(
        builds.load(Ordering::SeqCst) >= 2,
        "the refused build retried"
    );
    core.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_plugin_build_fails_the_turn_typed_on_sqlite() {
    a_refused_plugin_build_fails_the_turn_typed(sqlite_memory_store_backend().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the optional PostgreSQL service URL"
)]
async fn a_refused_plugin_build_fails_the_turn_typed_on_postgres() {
    let (backend, _database, _attachments) = postgres_backend().await;
    a_refused_plugin_build_fails_the_turn_typed(backend).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retryably_refused_plugin_build_completes_the_turn_on_sqlite() {
    a_retryably_refused_plugin_build_completes_the_turn(sqlite_memory_store_backend().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the optional PostgreSQL service URL"
)]
async fn a_retryably_refused_plugin_build_completes_the_turn_on_postgres() {
    let (backend, _database, _attachments) = postgres_backend().await;
    a_retryably_refused_plugin_build_completes_the_turn(backend).await;
}
