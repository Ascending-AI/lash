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
        RAISE EXCEPTION 'an advisory lock this transaction needs after its artifact locks is held elsewhere'
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
END
$lash_lock_artifacts$;
"#,
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
