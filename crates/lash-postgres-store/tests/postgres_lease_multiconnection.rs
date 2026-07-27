//! Live, multi-connection process-lease clock/fencing evidence.
//!
//! The single-pool conformance suite (`tests/conformance.rs`) forces lease
//! expiry with `ttl_ms = 0` rather than by elapsed time, and the clock-contract
//! test only string-scans the source. Neither exercises two *independent*
//! connections competing for one lease under real time. This test does, proving
//! the property the DB-clock design exists to guarantee: worker wall-clock skew
//! can neither steal a live lease nor spuriously expire one, because every lease
//! decision reads `now` from the database clock (`clock_timestamp()` via
//! `process_lease_now_epoch_ms_tx`), not from any caller.
//!
//! ## App-wall-clock investigation (task step 1)
//!
//! The lease paths were grepped for `current_epoch_ms` and for client-supplied
//! timestamps flowing into the lease SQL. Finding: **lease validity is purely
//! database-time driven.** The client-facing API — `claim_process_lease`,
//! `reclaim_process_lease`, `renew_process_lease`, `complete_process_with_lease`
//! — never accepts an absolute wall-clock instant; it takes only a TTL
//! *duration* (`lease_ttl_ms`). Inside each method `now` comes from
//! `process_lease_now_epoch_ms_tx` (the DB clock), and `guard_lease` compares
//! the *DB-persisted* lease's `expires_at_epoch_ms` against DB-now — never the
//! caller's copy of the lease. The `expires_at_epoch_ms` / `claimed_at_epoch_ms`
//! fields carried inside a `ProcessLease` handed back to the client are outputs,
//! not validity inputs: on renew/complete only the `(owner, lease_token,
//! fencing_token)` triple is matched. (The `current_epoch_ms()` calls elsewhere
//! in the registry stamp `updated_at_ms` bookkeeping / `first_started` /
//! abandon-request metadata — none gate lease validity.)
//!
//! Because a client-supplied time genuinely cannot influence lease validity,
//! this test proves that fact directly with a deliberately skewed client value:
//! it forges a copy of the stale host's lease whose `expires_at_epoch_ms` is a
//! decade in the future and shows the database still rejects its renew/complete.
//! The two independent "hosts" whose local wall clocks are never consulted are
//! themselves the skew evidence: only database time decides.

use std::time::{Duration, Instant};

use lash_core::{
    LeaseOwnerIdentity, ProcessAwaitOutput, ProcessInput, ProcessLeaseClaimOutcome,
    ProcessProvenance, ProcessRegistration, ProcessRegistry, ProcessStarted, RecoveryDisposition,
};
use lash_postgres_store::PostgresStorage;

mod support;

use support::SharedDatabaseLock;

fn database_url() -> Option<String> {
    std::env::var("LASH_POSTGRES_DATABASE_URL").ok()
}

async fn connect() -> PostgresStorage {
    let url = database_url().expect("caller checked LASH_POSTGRES_DATABASE_URL is set");
    PostgresStorage::connect(&url)
        .await
        .expect("connect postgres")
}

/// An external placeholder row whose rerunnable disposition permits the two
/// simulated hosts to record and fence local execution attempts.
fn registration(id: &str) -> ProcessRegistration {
    ProcessRegistration::new(
        id,
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryDisposition::Rerunnable,
        ProcessProvenance::host(),
    )
}

fn owner(owner_id: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

/// A hermetic process id per run so this test never collides with conformance
/// fixtures or a prior run — no destructive TRUNCATE of a shared database.
fn unique_process_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    format!("proc-lease-multiconn-{}-{nanos}", std::process::id())
}

/// The authoritative clock: read epoch-ms straight from the database, exactly as
/// the lease code does. Tests must never trust the local wall clock for validity
/// — only the DB clock decides.
async fn db_now_ms(storage: &PostgresStorage) -> u64 {
    let ms: i64 =
        sqlx::query_scalar("SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT")
            .fetch_one(storage.pool())
            .await
            .expect("query database clock");
    ms.max(0) as u64
}

/// Count the terminal events persisted for `process_id`. Used to prove exactly
/// one terminal is ever written despite a fenced host also attempting to
/// complete.
async fn terminal_event_count(storage: &PostgresStorage, process_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM lash_process_events
         WHERE process_id = $1
           AND event_type IN (
               'process.completed', 'process.failed',
               'process.cancelled', 'process.abandoned'
           )",
    )
    .bind(process_id)
    .fetch_one(storage.pool())
    .await
    .expect("count terminal events")
}

/// Poll the database clock until it is strictly past `target_ms`. The bound is a
/// local safety timeout only (so a wedged DB fails fast rather than hanging);
/// the *fact of expiry* is decided by the DB clock, never the local one.
async fn wait_until_db_past(storage: &PostgresStorage, target_ms: u64, bound: Duration) {
    let deadline = Instant::now() + bound;
    loop {
        if db_now_ms(storage).await > target_ms {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "database clock did not advance past the lease expiry within {bound:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn success(tag: &str) -> ProcessAwaitOutput {
    ProcessAwaitOutput::Success {
        value: serde_json::json!({ "by": tag }),
        control: None,
    }
}

/// Two independent hosts (separate pools/connections) compete for one process
/// lease against one database. The database clock — not either host's wall clock
/// — decides the lease timeline, and the fencing token strictly increases across
/// a real-time expiry reclaim while the stale host is definitively fenced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_lease_clock_and_fencing_hold_across_independent_connections() {
    let Some(database_url) = database_url() else {
        eprintln!(
            "skipping Postgres multi-connection lease test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&database_url).await;

    // Three INDEPENDENT PostgresStorage instances => three separate sqlx pools,
    // hence three independent database connections. Host A and Host B are the
    // competing "hosts"; the observer is an out-of-band third connection used to
    // re-read state so that assertions never rely on a competitor's own view.
    let host_a = connect().await;
    let host_b = connect().await;
    let observer = connect().await;
    let reg_a = host_a.process_registry();
    let reg_b = host_b.process_registry();
    let reg_c = observer.process_registry();

    let process_id = unique_process_id();
    reg_a
        .register_process(registration(&process_id))
        .await
        .expect("host A registers the process");

    let owner_a = owner("host-a-owner");
    let owner_b = owner("host-b-owner");

    // A immutable fact recorded by A before it ever loses the lease. Its
    // immutability across connections is checked at the very end.
    let started = ProcessStarted {
        owner: owner_a.clone(),
        fencing_token: 0,
        attempt: 1,
        started_at_ms: 111,
    };
    reg_a
        .record_first_started(&process_id, started.clone())
        .await
        .expect("host A records first_started");

    // A short-but-real TTL for A's claim: it genuinely elapses (the bounded wait
    // below proves that via the DB clock). B reclaims with a generous TTL so the
    // downstream fencing/completion assertions test *ordering* facts, never race
    // a duration — the test asserts no exact timings.
    const A_TTL_MS: u64 = 750;
    const B_TTL_MS: u64 = 60_000;

    // (2) Host A claims the lease with a short-but-real TTL.
    let lease_a = reg_a
        .claim_process_lease(&process_id, &owner_a, A_TTL_MS)
        .await
        .expect("host A claim")
        .acquired()
        .expect("host A acquires the free lease");
    assert!(
        lease_a.expires_at_epoch_ms > lease_a.claimed_at_epoch_ms,
        "the acquired lease must carry a DB-derived expiry after its claim instant"
    );

    // Host B's competing plain claim while A's lease is live is refused, and the
    // refusal names A as the current holder (the `Busy { holder }` shape).
    match reg_b
        .claim_process_lease(&process_id, &owner_b, B_TTL_MS)
        .await
        .expect("host B claim while A is live")
    {
        ProcessLeaseClaimOutcome::Busy { holder } => {
            assert!(
                holder.owner.same_incarnation(&owner_a),
                "the busy refusal must report host A as the holder"
            );
            assert_eq!(
                holder.fencing_token, lease_a.fencing_token,
                "the observed holder must be exactly A's live lease"
            );
        }
        ProcessLeaseClaimOutcome::Acquired(_) => {
            panic!("host B must not acquire a lease held live by host A")
        }
    }

    // (3) Wait for genuine elapsed database time past A's expiry, polling the DB
    // clock (never the local wall clock). Bounded — no unbounded sleep.
    wait_until_db_past(
        &host_a,
        lease_a.expires_at_epoch_ms,
        Duration::from_secs(15),
    )
    .await;

    // Host B reclaims the now-expired lease. It observes the current holder
    // through its OWN connection, then reclaims. The fencing token strictly
    // increases across the reclaim.
    let observed = reg_b
        .get_process_lease(&process_id)
        .await
        .expect("host B reads the current holder")
        .expect("a holder is present to observe");
    let lease_b = reg_b
        .reclaim_process_lease(&process_id, &owner_b, &observed, B_TTL_MS)
        .await
        .expect("host B reclaim after real-time expiry")
        .acquired()
        .expect("an expired lease is reclaimable");
    assert!(
        lease_b.fencing_token > lease_a.fencing_token,
        "the fencing token must strictly increase across the reclaim: {} !> {}",
        lease_b.fencing_token,
        lease_a.fencing_token
    );
    assert!(
        lease_b.owner.same_incarnation(&owner_b),
        "host B now owns the reclaimed lease"
    );

    // (4) Host A is now stale/fenced. Its writes with the OLD lease must fail and
    // must NOT terminalize the row or disturb B's lease.
    //
    // Concrete client-clock skew proof: forge a copy of A's lease whose
    // `expires_at_epoch_ms` is a decade in the future — as if A's wall clock were
    // wildly ahead. The database must still reject it, because validity is
    // decided by the DB-persisted lease (whose token/fence A no longer matches),
    // not by the caller's expiry field.
    let mut skewed_a = lease_a.clone();
    skewed_a.expires_at_epoch_ms = db_now_ms(&host_a)
        .await
        .saturating_add(10 * 365 * 24 * 3600 * 1000);

    assert!(
        reg_a
            .renew_process_lease(&skewed_a, A_TTL_MS)
            .await
            .is_err(),
        "a fenced host cannot renew even with a wildly future client-supplied expiry"
    );
    assert!(
        reg_a
            .complete_process_with_lease(&skewed_a, success("stale-A-skewed"))
            .await
            .is_err(),
        "a fenced host cannot complete with a skewed client expiry"
    );
    assert!(
        reg_a
            .complete_process_with_lease(&lease_a, success("stale-A-original"))
            .await
            .is_err(),
        "a fenced host cannot complete with its genuine old lease either"
    );

    // Re-read via the THIRD connection: the row is NOT terminal, and B's lease is
    // untouched by A's fenced attempts.
    let mid_record = reg_c
        .get_process(&process_id)
        .await
        .expect("observer reads the record");
    assert!(
        !mid_record.is_terminal(),
        "a fenced host's failed writes must not terminalize the process"
    );
    let live_lease = reg_c
        .get_process_lease(&process_id)
        .await
        .expect("observer reads the lease")
        .expect("host B still holds a lease");
    assert_eq!(
        live_lease.fencing_token, lease_b.fencing_token,
        "host B's lease must be undisturbed by A's fenced attempts"
    );
    assert!(
        live_lease.owner.same_incarnation(&owner_b),
        "host B remains the holder after A's fenced attempts"
    );
    assert_eq!(
        terminal_event_count(&observer, &process_id).await,
        0,
        "no terminal event may exist after only a fenced host tried to complete"
    );

    // (5) Host B completes with its valid lease.
    let output = ProcessAwaitOutput::Success {
        value: serde_json::json!({ "by": "host-b", "n": 7 }),
        control: None,
    };
    let completed = reg_b
        .complete_process_with_lease(&lease_b, output)
        .await
        .expect("host B completes with its valid lease");
    assert!(
        completed.is_terminal(),
        "B's completion terminalizes the process"
    );

    // Exactly one terminal event exists (A's earlier attempts wrote none).
    assert_eq!(
        terminal_event_count(&observer, &process_id).await,
        1,
        "exactly one terminal event exists after B completes"
    );
    // The lease is released.
    assert!(
        reg_c
            .get_process_lease(&process_id)
            .await
            .expect("observer reads the released lease")
            .is_none(),
        "the lease is released after B completes"
    );
    // A's first_started fact is unchanged, observed across a third connection —
    // immutability holds regardless of which connection reads it.
    let final_record = reg_c
        .get_process(&process_id)
        .await
        .expect("observer final read");
    assert_eq!(
        final_record.first_started.as_deref(),
        Some(&started),
        "the first_started A recorded before losing the lease is immutable across connections"
    );
}
