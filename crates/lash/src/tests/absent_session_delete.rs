//! Deleting an id that never materialized a session is a no-op (ADR 0049,
//! FIG-4705).
//!
//! A delete is the session's close request, mail to its actor (ADR 0132
//! §12). A delete that finds no session requests nothing, so it owes nothing
//! and cleans nothing: the id stays creatable, and the session a later or
//! concurrent create makes of it runs. Only a requested close ends in a
//! tombstone.
//!
//! Over SQLite memory and SQLite file; the PostgreSQL legs are ignored
//! outside a PostgreSQL gate, which selects them with `--include-ignored`.

use super::*;
use crate::{SessionId, TurnInput, TurnOutcome};

/// The stores a law's deployment runs over, and what must outlive them.
async fn stores(
    storage: &str,
) -> (
    Arc<dyn lash_core::StoreSet>,
    Vec<Box<dyn std::any::Any + Send>>,
) {
    match storage {
        "sqlite_memory" => (sqlite_memory_store_set().await, Vec::new()),
        "sqlite_file" => {
            let directory = tempfile::tempdir().expect("SQLite test directory");
            let stores = Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(
                    directory.path().join("lash.db"),
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await
                .expect("open SQLite store set"),
            );
            (stores, vec![Box::new(directory)])
        }
        "postgres" => {
            let (stores, database, attachments) = postgres_store_parts().await;
            (stores, vec![Box::new(database), Box::new(attachments)])
        }
        other => panic!("no storage {other}"),
    }
}

/// Run one turn of live session `id` and require it to finish.
async fn runs_a_turn(core: &LashCore, id: &str) -> Result<()> {
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        core.session(SessionId::fixture(id))
            .durable()
            .await?
            .send(TurnInput::text("a turn of the created session"))
            .output()
            .await
    })
    .await
    .unwrap_or_else(|_| panic!("session `{id}` runs a turn: none finished in 60 s"))?;
    assert!(
        matches!(output.result.outcome, TurnOutcome::Finished(_)),
        "session `{id}` runs a turn: {:?}",
        output.result.outcome
    );
    Ok(())
}

/// Delete `id` through the core's session administration.
async fn delete(core: &LashCore, id: &str) -> Result<crate::SessionDeletion> {
    let administration = core.session_administration().await;
    LashCore::delete_session(administration.delete_context(id)?).await
}

/// Whether `id`'s session has a close, requested or finished.
async fn closing(core: &LashCore, id: &str) -> Result<bool> {
    Ok(
        lash_core::runtime::durable::session_close::session_close_state(
            core.backend(),
            &SessionId::fixture(id),
        )
        .await
        .map_err(EmbedError::from)?
        .is_some(),
    )
}

/// The created session's own delete closes it and ends at its tombstone.
async fn its_own_delete_deletes_it(core: &LashCore, id: &str) -> Result<()> {
    let deletion = delete(core, id).await?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Requested { .. }),
        "the created session's own delete requests its close: {deletion:?}"
    );
    assert_eq!(
        core.await_session_deletion(&SessionId::fixture(id)).await?,
        crate::core::SessionDeleteCompletion::Deleted,
        "the close ends at its tombstone"
    );
    assert!(
        core.session(SessionId::fixture(id))
            .durable()
            .await?
            .was_deleted()
            .await?
    );
    Ok(())
}

/// A delete of an id no session was ever created under answers `Absent` and
/// leaves the id as it found it: a session created under it afterwards runs
/// a turn, and its own delete closes and deletes it.
async fn a_delete_of_a_never_created_id_leaves_the_id_creatable_and_runnable(
    storage: &str,
) -> Result<()> {
    const ID: &str = "absent-delete-then-create";
    let (stores, _held) = stores(storage).await;
    let core = standard_core_over(lash_conformance::backend_over(stores));
    let session_id = SessionId::from(ID);

    let deletion = delete(&core, ID).await?;
    assert!(
        matches!(
            &deletion,
            crate::SessionDeletion::Absent { session_id: absent } if *absent == session_id
        ),
        "nothing was ever created under the id, so nothing is deleted: {deletion:?}"
    );
    assert!(
        matches!(
            core.store_factory.lookup_session(&session_id).await?,
            lash_core::store::SessionLookup::Absent
        ),
        "the no-op delete left no tombstone"
    );
    assert!(
        !closing(&core, ID).await?,
        "the no-op delete requested no close"
    );

    core.session(SessionId::fixture(ID))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    runs_a_turn(&core, ID).await?;
    its_own_delete_deletes_it(&core, ID).await?;
    core.shutdown().await?;
    Ok(())
}

/// A delete racing the create of its id: whichever lands first, a delete
/// that found no session requested nothing, so it cleans up nothing. The
/// created session is live, not closing, and runs a turn; a delete that
/// came after the create requested the session's own close, which ends it.
async fn a_delete_racing_a_create_cleans_up_nothing_without_an_accepted_close(
    storage: &str,
) -> Result<()> {
    const ID: &str = "absent-delete-races-create";
    let (stores, _held) = stores(storage).await;
    let core = standard_core_over(lash_conformance::backend_over(stores));
    let session_id = SessionId::from(ID);

    let (deletion, created) = tokio::join!(
        delete(&core, ID),
        core.session(SessionId::fixture(ID))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec()
            )),
    );
    created?;
    match deletion? {
        crate::SessionDeletion::Absent { .. } => {
            assert!(
                matches!(
                    core.store_factory.lookup_session(&session_id).await?,
                    lash_core::store::SessionLookup::Live(_)
                ),
                "the created session outlives a delete that never closed it"
            );
            assert!(
                !closing(&core, ID).await?,
                "no close of the created session was requested"
            );
            runs_a_turn(&core, ID).await?;
            its_own_delete_deletes_it(&core, ID).await?;
        }
        crate::SessionDeletion::Requested { .. } => {
            assert_eq!(
                core.await_session_deletion(&session_id).await?,
                crate::core::SessionDeleteCompletion::Deleted,
                "a delete after the create closes the created session"
            );
        }
        other => panic!("a first delete of a fresh id: {other:?}"),
    }
    core.shutdown().await?;
    Ok(())
}

macro_rules! absent_session_delete_laws {
    ($($(#[$service:meta])* $storage:ident;)*) => {
        $(
            mod $storage {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_delete_of_a_never_created_id_leaves_the_id_creatable_and_runnable()
                -> Result<()> {
                    super::a_delete_of_a_never_created_id_leaves_the_id_creatable_and_runnable(
                        stringify!($storage),
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_delete_racing_a_create_cleans_up_nothing_without_an_accepted_close()
                -> Result<()> {
                    super::a_delete_racing_a_create_cleans_up_nothing_without_an_accepted_close(
                        stringify!($storage),
                    )
                    .await
                }
            }
        )*
    };
}

absent_session_delete_laws! {
    sqlite_memory;
    sqlite_file;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
    postgres;
}
