//! Deleting an id that never materialized a session is a no-op (ADR 0049,
//! FIG-4705).
//!
//! A delete whose close finds no durable record of the session closed
//! nothing, so it owes nothing and cleans nothing: the id stays creatable,
//! and the session a later or concurrent create makes of it runs. Only an
//! accepted close leads to a physical delete.
//!
//! Over SQLite memory, SQLite file and PostgreSQL, on the Restate server
//! double. The PostgreSQL legs are ignored outside a PostgreSQL gate, which
//! selects them with `--include-ignored`.

use super::crashed_create_drain::{Storage, double_over};
use super::scope_support::delete_session_on;
use super::*;

fn core_over(double: &lash_restate_test::RestateTestBackend) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        double.lash_backend(),
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
}

/// Run one turn of live session `id` and require it to finish.
async fn runs_a_turn(
    double: &lash_restate_test::RestateTestBackend,
    core: &LashCore,
    id: &str,
) -> Result<()> {
    let turn = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        core.session(id)
            .open()
            .await?
            .send(TurnInput::text("a turn of the created session"))
            .output()
            .await
    })
    .await;
    let Ok(output) = turn else {
        panic!(
            "session `{id}` runs a turn: none finished in 60 s, invocations {:?}",
            invocations(double)
        );
    };
    match output {
        Ok(output) => assert!(
            matches!(output.result.outcome, TurnOutcome::Finished(_)),
            "session `{id}` runs a turn: {:?}",
            output.result.outcome
        ),
        Err(error) => panic!("session `{id}` runs a turn: {error:?}"),
    }
    double.settle_session_drive(&SessionId::from(id)).await;
    Ok(())
}

/// A delete of an id no session was ever created under answers `Absent` and
/// leaves the id as it found it: a session created under it afterwards runs
/// a turn, and its own delete closes and physically deletes it.
async fn a_delete_of_a_never_created_id_leaves_the_id_creatable_and_runnable(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "absent-delete-then-create";
    let Some((double, _held, _seams)) = double_over(storage).await else {
        return Ok(());
    };
    let core = core_over(&double)?;
    let session_id = SessionId::from(ID);

    let deletion = delete_session_on(&double, &core, ID).await?;
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

    core.session(ID)
        .create(crate::SessionCreation::default())
        .await?;
    runs_a_turn(&double, &core, ID).await?;

    let deletion = delete_session_on(&double, &core, ID).await?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Deleted(_)),
        "the created session's own delete closes and deletes it: {deletion:?}"
    );
    assert!(core.session(ID).durable().await?.was_deleted().await?);
    Ok(())
}

/// What [`CloseGate`] holds a close on: it signals that the close's store
/// half answered "no durable record", and waits to be released.
type Held = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

/// A session catalog that holds the first close whose store half found no
/// durable record of its session, after the store answered and before the
/// delete acts on the answer: the window a concurrent create lands in.
struct CloseGate {
    inner: Arc<dyn DeploymentStore>,
    held: StdMutex<Option<Held>>,
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for CloseGate {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> std::result::Result<Option<lash_core::store::ControlIntent>, StoreError> {
        let answer = self.inner.begin_session_close(session_id, at_ms).await?;
        if answer.is_none() {
            let held = self.held.lock_recover().take();
            if let Some((answered, release)) = held {
                let _ = answered.send(());
                let _ = release.await;
            }
        }
        Ok(answer)
    }
}

impl lash_core::DeploymentStoreDecorator for CloseGate {}

/// A delete whose close found no session, racing the create of its id: the
/// create lands after the close's store half answered and before the delete
/// returns. The delete closed nothing, so it cleans up nothing: it answers
/// `Absent`, and the created session is live, not closing, and runs a turn.
async fn a_delete_racing_a_create_cleans_up_nothing_without_an_accepted_close(
    storage: Storage,
) -> Result<()> {
    const ID: &str = "absent-delete-races-create";
    let Some((double, _held, _seams)) = double_over(storage).await else {
        return Ok(());
    };
    let (answered, closed) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let gate = Arc::new(CloseGate {
        inner: double.lash_backend().session_store_factory(),
        held: StdMutex::new(Some((answered, released))),
    });
    let backend: lash_core::Backend = DecoratedBackend::over(double.lash_backend())
        .session_store_factory(move |_| gate)
        .into();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = SessionId::from(ID);

    let create = async {
        closed.await.expect("the delete's close answers");
        let created = core
            .session(ID)
            .create(crate::SessionCreation::default())
            .await;
        release.send(()).expect("the delete waits on its close");
        created
    };
    let (deletion, created) = tokio::join!(delete_session_on(&double, &core, ID), create);
    created?;
    let deletion = deletion?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Absent { .. }),
        "the delete closed nothing, so it deletes nothing: {deletion:?}"
    );
    assert!(
        matches!(
            core.store_factory.lookup_session(&session_id).await?,
            lash_core::store::SessionLookup::Live(_)
        ),
        "the created session outlives a delete that never closed it"
    );
    assert!(
        core.store_factory
            .drive_epoch(&session_id)
            .await?
            .closing
            .is_none(),
        "no close of the created session was accepted"
    );
    runs_a_turn(&double, &core, ID).await?;

    let deletion = delete_session_on(&double, &core, ID).await?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Deleted(_)),
        "the created session's own delete closes and deletes it: {deletion:?}"
    );
    Ok(())
}

macro_rules! absent_session_delete_laws {
    ($($(#[$service:meta])* $storage:ident: $kind:expr;)*) => {
        $(
            mod $storage {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_delete_of_a_never_created_id_leaves_the_id_creatable_and_runnable()
                -> Result<()> {
                    super::a_delete_of_a_never_created_id_leaves_the_id_creatable_and_runnable(
                        $kind,
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_delete_racing_a_create_cleans_up_nothing_without_an_accepted_close()
                -> Result<()> {
                    super::a_delete_racing_a_create_cleans_up_nothing_without_an_accepted_close(
                        $kind,
                    )
                    .await
                }
            }
        )*
    };
}

absent_session_delete_laws! {
    sqlite_memory: Storage::SqliteMemory;
    sqlite_file: Storage::SqliteFile;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    postgres: Storage::Postgres;
}
