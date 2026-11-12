//! Every transaction-scoped advisory lock the store takes, and the seal
//! that rules them after the artifact locks (FIG-7700).
//!
//! A transaction takes its waiting locks in the class order the artifact
//! lock plan states ([`crate::artifact_store::LockPlan`]): artifact locks
//! come last, once, through `lash_lock_artifacts`, which takes a sorted
//! artifact set and then seals the transaction (a transaction-local
//! setting). Every artifact lock is that function: the plan, a local end,
//! an environment acquisition, a hold and a publisher's sorted set. From
//! the seal on, [`function_ddl`]'s `lash_advisory_xact_lock` and
//! `lash_advisory_xact_lock_pair` never wait.
//! - It takes a lock with the try form.
//! - A lock this transaction already holds is granted at once, so a
//!   complete plan never notices the seal.
//! - A lock held elsewhere raises `55P03`, the typed contention every
//!   caller retries. A path the plan missed therefore costs a retry, never
//!   a deadlock.
//!
//! The seal also bounds every other wait the transaction makes from then
//! on: `lash_lock_artifacts` sets its `lock_timeout` to the sealed bound
//! ([`SEALED_LOCK_TIMEOUT`], default [`DEFAULT_SEALED_LOCK_TIMEOUT`]), so a
//! row, foreign-key or relation lock held elsewhere ends in `55P03` too,
//! and the operation retries. The bound is below the server's
//! `deadlock_timeout`, so in a cycle through a sealed transaction that
//! transaction times out, and retries, before a deadlock is detected.
//!
//! Every transaction-scoped advisory lock in the store's SQL is this
//! function: statements, step programs and server functions alike.
//! Only these raw locks remain:
//! - the writer fence's lock, which is every guarded transaction's first
//!   statement and so always comes before any seal;
//! - the schema bootstrap and migration locks, taken before this function
//!   exists;
//! - the session-scoped locks on dedicated connections (boot liveness, the
//!   constraint inspector), which no artifact-locking transaction takes.
//!
//! A test observes the plan's completeness with an unplanned-lock count:
//! it creates the sequence [`UNPLANNED_LOCKS`], and from then on each
//! request after a seal for a lock the transaction does not already hold
//! advances it, granted or contended. A sequence is not transactional, so
//! a contended attempt's request still counts after its rollback. A store
//! without the sequence pays one catalog lookup per sealed lock and
//! nothing else.

/// The transaction setting that carries the `lock_timeout` a transaction
/// runs under once sealed, in milliseconds
/// ([`crate::host::GuardPolicies::sealed_lock_timeout`]). The store's preludes
/// and its one-call commit set it; a transaction begun without either (an
/// unfenced store transaction) seals with
/// [`DEFAULT_SEALED_LOCK_TIMEOUT`].
pub(crate) const SEALED_LOCK_TIMEOUT: &str = "lash.sealed_lock_timeout";

/// The sealed `lock_timeout` a store installs unless configured otherwise.
pub(crate) const DEFAULT_SEALED_LOCK_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(250);

/// The handler that closes a sealing function's block around its
/// statements after its seal (FIG-7700). A lock wait there that expires is
/// raised again as the schema's own after-seal expiry
/// ([`crate::replayable::SEALED_WAIT_EXPIRED`]), so the retry loop knows
/// it expired after the seal; the advisory helper's refusal of an unplanned
/// lock, and an expiry an inner sealing function already named, pass
/// through unchanged. A block that holds only what follows the seal passes
/// `TRUE` as `sealed`; one that may begin before it (the owner commit's
/// steps) passes the variable that records whether a step has sealed, so a
/// wait before the seal is never renamed. Only `TRUE` is sealed: a `NULL`
/// record is not.
pub(crate) fn after_seal_handler(sealed: &str) -> String {
    format!(
        "EXCEPTION WHEN lock_not_available THEN
        IF ({sealed}) IS NOT TRUE OR SQLERRM = '{refused}' OR starts_with(SQLERRM, '{expired}') THEN
            RAISE;
        END IF;
        RAISE EXCEPTION '{expired}: %', SQLERRM USING ERRCODE = '55P03';",
        refused = crate::replayable::UNPLANNED_ADVISORY_REFUSED,
        expired = crate::replayable::SEALED_WAIT_EXPIRED,
    )
}

/// `ddl` with each `{AFTER_SEAL}` filled by [`after_seal_handler`] for a
/// block that holds only what follows the seal.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn with_after_seal(ddl: &str) -> String {
    ddl.replace("{AFTER_SEAL}", &after_seal_handler("TRUE"))
}

/// The sequence an unplanned lock advances, when a test created it.
#[cfg(any(test, feature = "testing"))]
pub const UNPLANNED_LOCKS: &str = "lash_unplanned_advisory_locks";

/// The advisory lock functions, exclusive or shared:
/// `lash_advisory_xact_lock` for the one-bigint key form and
/// `lash_advisory_xact_lock_pair` for the two-integer form. They have two
/// names rather than one overloaded name because teardown drops each
/// function by name alone.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn function_ddl() -> String {
    let refused = crate::replayable::UNPLANNED_ADVISORY_REFUSED;
    let body = |args: &str, held: &str| {
        format!(
            r#"
BEGIN
    IF current_setting('lash.artifacts_sealed', true) IS DISTINCT FROM 'on' THEN
        IF p_shared THEN PERFORM pg_advisory_xact_lock_shared({args}); ELSE PERFORM pg_advisory_xact_lock({args}); END IF;
        RETURN;
    END IF;
    IF to_regclass('lash_unplanned_advisory_locks') IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory'
            AND pid = pg_backend_pid() AND granted AND {held}
            AND (mode = 'ExclusiveLock' OR (p_shared AND mode = 'ShareLock'))) THEN
        PERFORM nextval('lash_unplanned_advisory_locks');
    END IF;
    IF NOT (CASE WHEN p_shared THEN pg_try_advisory_xact_lock_shared({args}) ELSE pg_try_advisory_xact_lock({args}) END) THEN
        RAISE EXCEPTION '{refused}'
            USING ERRCODE = '55P03';
    END IF;
END"#
        )
    };
    format!(
        r#"CREATE OR REPLACE FUNCTION lash_advisory_xact_lock(p_key bigint, p_shared boolean)
RETURNS void LANGUAGE plpgsql VOLATILE AS $lash_advisory_xact_lock$
{one}
$lash_advisory_xact_lock$;
CREATE OR REPLACE FUNCTION lash_advisory_xact_lock_pair(p_class integer, p_key integer, p_shared boolean)
RETURNS void LANGUAGE plpgsql VOLATILE AS $lash_advisory_xact_lock_pair$
{two}
$lash_advisory_xact_lock_pair$;
CREATE OR REPLACE FUNCTION lash_lock_artifacts(p_keys text[])
RETURNS void LANGUAGE plpgsql VOLATILE AS $lash_lock_artifacts$
DECLARE
    v_key text;
BEGIN
    FOREACH v_key IN ARRAY p_keys LOOP
        PERFORM lash_advisory_xact_lock(hashtextextended(v_key, 0), false);
    END LOOP;
    PERFORM set_config('lash.artifacts_sealed', 'on', true);
    -- Every later wait, row, foreign-key or advisory, ends in 55P03 after
    -- the sealed bound, so the transaction retries rather than closes a
    -- cycle (FIG-7700).
    PERFORM set_config('lock_timeout',
        COALESCE(NULLIF(current_setting('{sealed}', true), ''), '{default_ms}'), true);
END
$lash_lock_artifacts$;
"#,
        sealed = SEALED_LOCK_TIMEOUT,
        default_ms = DEFAULT_SEALED_LOCK_TIMEOUT.as_millis(),
        one = body(
            "p_key",
            "objsubid = 1 AND classid = ((p_key >> 32) & 4294967295)::oid \
             AND objid = (p_key & 4294967295)::oid",
        ),
        two = body(
            "p_class, p_key",
            "objsubid = 2 AND classid = (p_class::bigint & 4294967295)::oid \
             AND objid = (p_key::bigint & 4294967295)::oid",
        ),
    )
}
