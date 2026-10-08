//! FIG-4346: a run the session's deletion meets ends with the typed
//! retirement ADR 0049 prescribes.
//!
//! A deletion is the session's close request, mail to its actor (ADR 0132
//! §12), drained at the actor's next pass, and the close's first step
//! cancels the turn it finds open. So a run in flight when its session is
//! deleted ends once, by its own commit or by the close, and its send
//! answers that end, or the typed `SessionDeleted` once the close deleted
//! the session's storage before the handle read it; nothing runs its turn
//! again; the close ends at the
//! tombstone; and a send after it is refused as the deleted session's. A
//! run resumed on another node after a crash meets the close first: its
//! activation drains the close request before it runs the turn again, at
//! every cut (`lash-durable-test`'s `deleted_session_crash_laws.rs`).

use super::*;
use crate::{SessionId, TurnInput};

/// A model whose first call signals [`Held::entered`] and holds until the
/// law releases it; every call is counted.
#[derive(Default)]
struct Held {
    calls: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Held {
    fn provider(self: &Arc<Self>) -> ProviderHandle {
        let held = Arc::clone(self);
        crate::testing::TestProvider::builder()
            .kind("deleted-session-held")
            .complete(move |_request| {
                let held = Arc::clone(&held);
                async move {
                    if held.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        held.entered.notify_one();
                        held.release.notified().await;
                    }
                    Ok(text_response("answered after the delete"))
                }
            })
            .build()
            .into_handle()
    }
}

/// A run held at its model call when its session is deleted ends once: its
/// send answers that end, the session's close
/// ends at its tombstone, the model is never asked again, and a send
/// after the close is refused as the deleted session's.
async fn a_run_in_flight_when_its_session_is_deleted_ends_typed(
    stores: Arc<dyn lash_core::StoreSet>,
) -> Result<()> {
    const ID: &str = "deleted-session-run";
    let held = Arc::new(Held::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(stores),
    ))
    .serve_test_llm_profile(held.provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::fixture(ID);
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;

    let running = session
        .send(TurnInput::text("held when the delete arrives"))
        .await?;
    held.entered.notified().await;
    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(administration.delete_context(ID)?).await?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Requested { .. }),
        "{deletion:?}"
    );
    held.release.notify_one();

    let ended = tokio::time::timeout(std::time::Duration::from_secs(60), running.outcome())
        .await
        .expect("the deleted session's run ends");
    match &ended {
        Ok(outcome) => assert!(
            matches!(
                outcome.status(),
                crate::TurnStatus::Answered | crate::TurnStatus::Cancelled
            ),
            "the run in flight ends once, by its commit or by the close's cancel: {outcome:?}"
        ),
        Err(EmbedError::Store(StoreError::SessionDeleted {
            session_id: deleted,
        })) if *deleted == session_id => {}
        Err(error) => panic!("the run's handle answers its end or the typed deletion: {error:?}"),
    }
    assert_eq!(
        core.await_session_deletion(&session_id).await?,
        crate::core::SessionDeleteCompletion::Deleted,
        "the close ends at its tombstone"
    );
    assert!(
        matches!(
            core.store_factory.lookup_session(&session_id).await?,
            lash_core::store::SessionLookup::Deleted
        ),
        "the deleted session is a tombstone"
    );
    assert_eq!(
        held.calls.load(Ordering::SeqCst),
        1,
        "nothing runs the deleted session's turn again"
    );
    let again = core.session(session_id.clone()).durable().await;
    let refused = match again {
        Err(error) => error,
        Ok(session) => session
            .send(TurnInput::text("after the delete"))
            .output()
            .await
            .expect_err("a deleted session takes no send"),
    };
    assert!(
        matches!(
            &refused,
            EmbedError::Store(StoreError::SessionDeleted { session_id: deleted })
                if *deleted == session_id
        ),
        "the deleted session refuses typed: {refused:?}"
    );
    core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_in_flight_when_its_session_is_deleted_ends_typed_on_sqlite_memory() -> Result<()> {
    a_run_in_flight_when_its_session_is_deleted_ends_typed(sqlite_memory_store_set().await).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn a_run_in_flight_when_its_session_is_deleted_ends_typed_on_postgres() -> Result<()> {
    let (stores, _database, _attachments) = postgres_store_set().await;
    a_run_in_flight_when_its_session_is_deleted_ends_typed(stores).await
}
