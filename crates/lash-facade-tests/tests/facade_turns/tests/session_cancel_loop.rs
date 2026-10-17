//! CANCEL-CLAIM-PARKS-WITHOUT-STRANDING, the session's half (FIG-6099): a
//! session claimed only for its close (its claim requirements served by no
//! node) whose close fails on every claim parks `CancelLoop` at the
//! activation-loop budget instead of being claimed again without bound. Its
//! close stays recorded, and an operator's redrive, or a further close
//! request, runs it again to its tombstone.
//!
//! Over SQLite memory; the PostgreSQL legs are ignored outside a PostgreSQL
//! gate, which selects them with `--include-ignored`.

use super::session_cancel_without_provider::{ENDS_WITHIN, deploy_unserving, stranded_over};
use super::*;
use lash_core::durable_port::{
    ActorKey, ActorState, CommitLabel, DurableError, StoreFailure, StoreFailureKind,
};
use lash_core::testing::DurableCommitHook;
use std::sync::atomic::AtomicU32;

/// What runs the parked close again.
#[derive(Clone, Copy, Debug)]
enum Retry {
    /// An operator's redrive.
    Redrive,
    /// A further close request.
    CloseAgain,
}

/// The session actor's park once it parked, read within `within`.
async fn parked(
    backend: &lash_execution_contracts::backend::Backend,
    actor: &ActorKey,
    within: std::time::Duration,
) -> Option<lash::SessionParkReason> {
    tokio::time::timeout(within, async {
        loop {
            if let Ok(Some(snapshot)) = backend.durable().actor(actor).await
                && snapshot.state == ActorState::Parked
            {
                let park = snapshot.park.expect("a parked actor records its park");
                return serde_json::from_str(&park).expect("a session park");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .ok()
}

/// A close whose `cancel` step fails on every claim of a node that serves
/// none of the session's requirements parks the session `CancelLoop` after
/// exactly the activation-loop budget's claims that committed nothing, with
/// its close recorded and unfinished; a redrive, or a further close
/// request, then runs the close to its tombstone without calling a
/// provider.
async fn a_close_that_fails_every_claim_parks_cancel_loop_and_runs_again_when_asked(
    storage: &str,
) -> Result<()> {
    for retry in [Retry::Redrive, Retry::CloseAgain] {
        let id = format!("cancel-loop-{retry:?}").to_lowercase();
        let hook = Arc::new(std::sync::OnceLock::<Arc<DurableCommitHook>>::new());
        let (stranded, _held) = stranded_over(storage, &id, {
            let hook = Arc::clone(&hook);
            move |stores| {
                lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                    .map_durable_store(move |inner| {
                        let hooked = Arc::new(DurableCommitHook::over(inner));
                        let _ = hook.set(Arc::clone(&hooked));
                        hooked
                    })
                    .into_store_set()
            }
        })
        .await?;
        let hook = Arc::clone(hook.get().expect("the durable store is hooked"));
        let session = SessionId::fixture(&id);
        let actor = ActorKey::session(session.as_str()).expect("a session actor key");
        let attempts = Arc::new(AtomicU32::new(0));
        hook.arm({
            let (actor, attempts) = (actor.clone(), Arc::clone(&attempts));
            move |tx, label| {
                let fails = tx.actor() == &actor && label == CommitLabel::SESSION_CLOSE_CANCEL;
                if fails {
                    attempts.fetch_add(1, Ordering::SeqCst);
                }
                std::future::ready(if fails {
                    Err(DurableError::Store(StoreFailure {
                        kind: StoreFailureKind::Unavailable,
                        message: "cancel-loop: the close's cancel step fails".to_owned(),
                    }))
                } else {
                    Ok(())
                })
            }
        });
        let core = deploy_unserving(&stranded.backend, &stranded.calls)?;
        let budget = stranded.backend.config().settings().activation_loop_budget;
        let administration = core.session_administration().await;
        let deletion =
            LashCore::delete_session(administration.delete_context(session.as_str())?).await?;
        assert!(
            matches!(deletion, lash::SessionDeletion::Requested { .. }),
            "{deletion:?}"
        );
        let park = parked(&stranded.backend, &actor, ENDS_WITHIN)
            .await
            .unwrap_or_else(|| panic!("{retry:?}: the failing close never parked the session"));
        assert_eq!(
            park,
            lash::SessionParkReason::CancelLoop {
                failed_activations: budget
            },
            "{retry:?}: the session parks at its activation-loop budget"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            budget + 1,
            "{retry:?}: the claim that began the close and the budget's claims \
             that committed nothing each ran its cancel step once"
        );
        let close = stranded
            .backend
            .durable()
            .session_close(&session)
            .await?
            .expect("the parked close stays recorded");
        assert!(!close.is_tombstone(), "{retry:?}: the close did not finish");
        assert!(
            stranded.backend.durable().turn(&session).await?.is_some(),
            "{retry:?}: the turn the close cancels is still open"
        );

        hook.disarm();
        match retry {
            Retry::Redrive => assert!(
                stranded
                    .backend
                    .redrive_session(&session, "cancel-loop-law")
                    .await?,
                "the redrive found the session parked"
            ),
            Retry::CloseAgain => {
                let again =
                    LashCore::delete_session(administration.delete_context(session.as_str())?)
                        .await?;
                assert!(
                    matches!(again, lash::SessionDeletion::Requested { .. }),
                    "{again:?}"
                );
            }
        }
        let completion = tokio::time::timeout(ENDS_WITHIN, core.await_session_deletion(&session))
            .await
            .unwrap_or_else(|_| panic!("{retry:?}: the close ran again to its tombstone"))?;
        assert_eq!(completion, lash::SessionDeleteCompletion::Deleted);
        assert_eq!(
            stranded.calls.counts(),
            stranded.before,
            "{retry:?}: no tool body and no model call ran for the close"
        );
        core.shutdown().await?;
    }
    Ok(())
}

macro_rules! session_cancel_loop_laws {
    ($($(#[$service:meta])* $storage:ident;)*) => {
        $(
            mod $storage {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_close_that_fails_every_claim_parks_cancel_loop_and_runs_again_when_asked()
                -> Result<()> {
                    super::a_close_that_fails_every_claim_parks_cancel_loop_and_runs_again_when_asked(
                        stringify!($storage),
                    )
                    .await
                }
            }
        )*
    };
}

session_cancel_loop_laws! {
    sqlite_memory;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
    postgres;
}
