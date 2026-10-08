//! A send to a store a newer release finalized answers its sender (FIG-4597).
//!
//! A finalize moves the store's fleet format past this build's writable
//! range, and every writer of this build is fenced: the store refuses the
//! write with the terminal `WriterFenced`. A send is the first write its
//! input makes, so the refusal reaches the sender at acceptance, typed and
//! never retried, and nothing of the input is written: no pending input, no
//! run, no model call.

use super::*;

/// How long a refused send may take to answer its sender. The bound turns a
/// sender left waiting on a refusal that is retried into a failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

const ID: &str = "writer-fenced-send";

/// Send to a session over `backend` after `finalize` fenced its writers;
/// the send is refused typed and writes nothing. `finalize` moves the fleet
/// format to the epoch it answers.
async fn a_send_to_a_finalized_store_is_refused_typed_and_writes_nothing(
    backend: lash_core::Backend,
    finalize: impl AsyncFnOnce() -> u32,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("writer-fenced-send")
        .complete({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(text_response("a fenced writer ran a turn")) }
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("the core");
    let session = core
        .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("create the session before the finalize");
    let recorded = finalize().await;

    let answer = tokio::time::timeout(
        ANSWERS_WITHIN,
        session.send(TurnInput::text("a send under a fenced writer")),
    )
    .await
    .expect("the sender receives the refusal, not a retry loop");
    let Err(error) = answer else {
        panic!("a fenced writer accepts nothing");
    };
    let fenced = match &error {
        EmbedError::Store(StoreError::WriterFenced { recorded, .. }) => Some(*recorded),
        EmbedError::Runtime(runtime) => match &runtime.cause {
            Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) => match &**refusal {
                lash_core::store::StoreRefusal::WriterFenced { recorded, .. } => Some(*recorded),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    };
    assert_eq!(
        fenced,
        Some(recorded),
        "the sender reads the typed fence naming the recorded format: {error:?}"
    );
    assert!(
        error.is_terminal() && !error.is_retryable(),
        "a fence is never retried: {error:?}"
    );
    let unfenced =
        lash_core::runtime::live_session_view(&core.store_factory, &lash_core::SessionId::from(ID))
            .await
            .expect("a reader is never fenced")
            .expect("the session exists");
    assert!(
        unfenced
            .list_pending_turn_inputs()
            .await
            .expect("pending inputs")
            .is_empty(),
        "the refused send wrote no input"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no turn ran");
    let _ = core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_finalized_store_is_refused_typed_and_writes_nothing_on_sqlite_file() {
    let files = tempfile::tempdir().expect("SQLite store directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(files.path().join("lash.db"))
            .await
            .expect("SQLite file stores"),
    );
    let location = stores.location().clone();
    a_send_to_a_finalized_store_is_refused_typed_and_writes_nothing(
        lash_conformance::backend_over(stores),
        async move || {
            let next = lash_core::store::FleetFormat::writable().max() + 1;
            lash_sqlite_store::testing::finalize_fleet_format(&location, next).expect("finalize");
            next
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the PostgreSQL service URL its gate sets"
)]
async fn a_send_to_a_finalized_store_is_refused_typed_and_writes_nothing_on_postgres() {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    ));
    let pool = storage.pool().clone();
    a_send_to_a_finalized_store_is_refused_typed_and_writes_nothing(
        lash_conformance::backend_over(stores),
        async move || {
            let next = lash_core::store::FleetFormat::writable().max() + 1;
            lash_postgres_store::testing::finalize_fleet_epoch(&pool, next)
                .await
                .expect("finalize");
            next
        },
    )
    .await;
}
