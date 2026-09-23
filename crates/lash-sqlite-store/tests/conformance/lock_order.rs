//! Lock-order law: no SQLite transaction holds one database while it waits
//! for another in an order a concurrent writer takes the other way round.
//!
//! A `memdb` reader cannot take a shared lock while any writer holds a
//! database's write lock (ADR 0102), so a read that holds the effect journal
//! while it waits for the attached process registry deadlocks against a writer
//! that holds the registry and needs the journal to commit: both sides wait
//! out the busy timeout. On a WAL file the reader never waits, which is why
//! only the memory substrate could expose it. Every lock a transaction takes
//! across databases follows core → journal → registry.

use std::time::{Duration, Instant};

use lash_core_execution::{
    AwaitEventResolver as _, AwaitEventWaitIdentity, EffectHost as _, EffectJournalRetirement,
    ExecutionScope, Resolution, RuntimeErrorCode, SessionStoreFactory as _,
};

use super::SUBSTRATE;
use crate::deployment_fixture::TestDeployment;

const ROUNDS: usize = 200;

/// The longest a single operation may take. A lock cycle costs a full busy
/// timeout (15 s) before SQLite gives up; ordinary contention costs
/// milliseconds.
const OPERATION_BOUND: Duration = Duration::from_secs(5);

fn scope(round: usize) -> ExecutionScope {
    ExecutionScope::process(format!("lock-order-process-{round}"))
}

fn bounded(started: Instant, what: &str) {
    let elapsed = started.elapsed();
    assert!(
        elapsed < OPERATION_BOUND,
        "{what} took {elapsed:?}: a transaction waited out a lock cycle"
    );
}

async fn mint(
    host: &lash_sqlite_store::SqliteEffectHost,
    round: usize,
    wait: &AwaitEventWaitIdentity,
) -> Option<lash_core_execution::AwaitEventKey> {
    let started = Instant::now();
    let key = match host.await_event_key(&scope(round), wait.clone()).await {
        Ok(key) => Some(key),
        Err(error) if error.code == RuntimeErrorCode::AwaitEventUnknownOrRevoked => None,
        Err(error) => panic!("mint {round} failed: {error}"),
    };
    bounded(started, "a process-scope key mint");
    key
}

/// One host peeks process-scope promises while a second host on the same
/// deployment resolves them and fences their scopes, and the factory sweeps
/// retained evidence across the catalog, the journal and the registry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retention_sweeps_and_process_scope_promises_never_wait_on_each_other() {
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let reader = deployment.effect_host();
    let writer = deployment.reopen().await.effect_host();
    let factory = deployment.session_store_factory();
    let wait = AwaitEventWaitIdentity::tool_completion("lock-order-call");

    let peeks = tokio::spawn({
        let wait = wait.clone();
        async move {
            for round in 0..ROUNDS {
                // A scope the writer already fenced refuses the mint.
                let Some(key) = mint(&reader, round, &wait).await else {
                    continue;
                };
                let started = Instant::now();
                match reader.peek_await_event(&key).await {
                    Ok(_) => {}
                    Err(error) if error.code == RuntimeErrorCode::AwaitEventUnknownOrRevoked => {}
                    Err(error) => panic!("peek {round} failed: {error}"),
                }
                bounded(started, "a process-scope peek");
            }
        }
    });
    let resolves = tokio::spawn(async move {
        for round in 0..ROUNDS {
            let Some(key) = mint(&writer, round, &wait).await else {
                continue;
            };
            let started = Instant::now();
            writer
                .resolve_await_event(&key, Resolution::Ok(serde_json::json!(round)))
                .await
                .expect("resolve a process-scope promise");
            bounded(started, "a process-scope resolve");
            if round % 4 == 0 {
                let started = Instant::now();
                writer
                    .retire_effect_journal(EffectJournalRetirement::process(format!(
                        "lock-order-process-{round}"
                    )))
                    .await
                    .expect("fence a process scope");
                bounded(started, "a process-scope fence");
            }
        }
    });
    let sweeps = tokio::spawn(async move {
        for _ in 0..ROUNDS / 4 {
            let started = Instant::now();
            factory
                .reclaim_retained_evidence(lash_core_execution::store::RetentionBound {
                    committed_before_epoch_ms: 0,
                })
                .await
                .expect("sweep retained evidence");
            bounded(started, "a retention sweep");
        }
    });

    tokio::time::timeout(Duration::from_secs(120), async {
        peeks.await.expect("peeks");
        resolves.await.expect("resolves");
        sweeps.await.expect("sweeps");
    })
    .await
    .expect("the race finishes inside its deadline");
}
