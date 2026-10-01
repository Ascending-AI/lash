//! A commit under a superseded drive fence ends its run typed, with no retry
//! (FIG-4512).
//!
//! A later admission that seals a newer drive epoch owns the session: a
//! commit presenting the older fence is refused `StaleDriveFence`, whole, on
//! every attempt (ADR 0105 §2, §9). The refusal is the permanent
//! `StoreCommitSuperseded`, which no engine retries, so the run that met it
//! ends in that attempt instead of repeating the commit until the engine's
//! retries are exhausted and the run is paused.
//!
//! The law is the conformance suite's
//! `a_root_whose_admission_a_successor_sealed_commits_nothing`: a successor
//! seals while the admitted root's one model call runs, and the root's first
//! commit, which no replay repeats, presents the superseded fence. It runs
//! here inside a handler of the server double, over SQLite memory, SQLite
//! file and PostgreSQL.

use super::process_root_recovery::{Harness, Storage};
use super::*;

/// The law's turns, each run in a handler of the double's deployment.
struct HandlerTurns(Harness);

#[async_trait::async_trait]
impl lash_conformance::ConformanceTurnRunner for HandlerTurns {
    async fn run_turn(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) {
        let Harness::Double(backend) = &self.0 else {
            unreachable!("the law runs on the server double");
        };
        let attempt: HandlerAttempt = Arc::new(move |scoped| {
            let attempt = Arc::clone(&attempt);
            Box::pin(async move {
                attempt(scoped).await;
            })
        });
        tokio::time::timeout(BOUND, backend.run_in_handler(admitted, attempt))
            .await
            .expect("the law's turn finishes")
            .expect("the law's turn runs in its handler");
    }

    async fn run_crashed_then_redriven_turn(
        &self,
        _admitted: lash_core::AdmittedScope,
        _crashing: lash_conformance::ConformanceTurnAttempt,
        _redrive: lash_conformance::ConformanceTurnAttempt,
    ) {
        unreachable!("the law crashes no turn");
    }
}

async fn superseded_fence_commit_law(storage: Storage) {
    let (harness, _stores) = Harness::new(storage, false).await;
    let backend = harness.backend();
    let prefix = run_tag("superseded-fence-commit");
    lash_conformance::registration_macro_support::a_root_whose_admission_a_successor_sealed_commits_nothing(
        &prefix,
        backend.effect_host(),
        backend.stores(),
        Arc::new(HandlerTurns(harness)),
    )
    .await;
}

macro_rules! laws {
    ($module:ident, $storage:expr $(, $service:literal)?) => {
        mod $module {
            use super::*;
            $(#[ignore = $service])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_commit_under_a_superseded_drive_fence_ends_its_run_typed_with_no_retry() {
                superseded_fence_commit_law($storage).await;
            }
        }
    };
}

laws!(sqlite_memory, Storage::Memory);
laws!(sqlite_file, Storage::File);
laws!(
    postgres,
    Storage::Postgres,
    "requires PostgreSQL; run through the pg16 service gate"
);
