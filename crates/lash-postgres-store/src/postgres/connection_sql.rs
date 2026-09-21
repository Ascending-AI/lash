//! The PostgreSQL SQL that names no lash table.
//!
//! Advisory locks, transaction isolation, the server clock and the catalog
//! probes are statements like any other: they are issued against a production
//! database, they fork by dialect by definition, and before FIG-3387 there
//! were fourteen copies of six of them scattered across eleven modules —
//! `SELECT pg_advisory_xact_lock(hashtextextended($1, 0))` alone appeared six
//! times verbatim. They name no table, so the table-ownership rules cannot
//! reach them; this module is the home that makes them reachable instead.
//!
//! Everything here is declared once, named, and rendered once at startup, and
//! the ownership gate holds this module to the same "one statement, one name"
//! rule it holds a table module to. A statement that names a lash table does
//! **not** belong here — it belongs to that table's family — and the gate
//! refuses one that does.
//!
//! The lock *keys* stay with their callers: what a key is built from is the
//! caller's decision about what it is serialising, and two callers that share
//! this statement share only the lock's shape, never its key space. The seed
//! argument is part of that shape: seed `0` is the general key space, seed `1`
//! is session-history mutation, and the two never collide.

use std::sync::LazyLock;

use lash_store_sql::Dialect;

lash_store_sql::statements! {
    /// PostgreSQL statements over no lash table.
    pub(crate) struct ConnectionStatements @ "postgres_connection" {
        /// Take the transaction-scoped advisory lock for text key `?1` in the
        /// general (seed `0`) key space.
        ///
        /// The artifact owner and artifact byte locks, the process-definition
        /// registration lock, the parent-end plan lock, the trigger-store
        /// locks and the session-execution-lease lock are all this statement:
        /// one lock shape, six key spaces distinguished by what the caller
        /// binds, not by six copies of the text.
        lock_xact_by_text = "SELECT pg_advisory_xact_lock(hashtextextended(?1, 0))";

        /// The same lock with the caller choosing the seed, which is how the
        /// await-event waiters partition their key space per wait kind
        /// without hashing the kind into the key string.
        lock_xact_by_text_seeded = "SELECT pg_advisory_xact_lock(hashtextextended(?1, ?2))";

        /// The same lock in a caller-supplied classification (`?1`) keyed on
        /// text `?2`, which is `pg_advisory_xact_lock`'s two-integer form
        /// rather than its one-bigint form: the attachment GC wants its class
        /// visible in `pg_locks` as its own `classid`.
        lock_xact_by_class_and_text = "SELECT pg_advisory_xact_lock(?1, hashtext(?2))";

        /// Serialise one session's history mutations (seed `1`).
        ///
        /// A separate seed from [`Self::lock_xact_by_text`] on purpose: a
        /// session id locked for history mutation and the same id locked for
        /// anything else must not be the same lock, or two unrelated writers
        /// would queue behind each other.
        lock_xact_session_history = "SELECT pg_advisory_xact_lock(hashtextextended(?1, 1::bigint))";

        /// The batch form: every distinct session in `?1`, locked in one
        /// statement in `session_id` order.
        ///
        /// The ordering is the deadlock argument, and it is why this is one
        /// statement rather than a loop: two callers holding overlapping
        /// session sets acquire the shared members in the same order.
        lock_xact_session_history_batch =
            "SELECT pg_advisory_xact_lock(hashtextextended(ordered.session_id, 1::BIGINT))
             FROM (
                 SELECT DISTINCT session_id
                 FROM unnest(?1::TEXT[]) AS target(session_id)
                 ORDER BY session_id
             ) AS ordered";

        /// Lock the pair `(?1, ?2)` in the general key space, length-prefixing
        /// each part so that `("ab", "c")` and `("a", "bc")` are two locks
        /// rather than one.
        lock_xact_by_text_pair = "SELECT pg_advisory_xact_lock(
             hashtextextended(
                 length(?1)::TEXT || ':' || ?1 || length(?2)::TEXT || ':' || ?2,
                 0
             )
         )";

        /// The single fixed key the evidence-retention sweep serialises on.
        ///
        /// A literal key rather than a hashed one: there is exactly one sweep,
        /// so there is nothing to derive the key from.
        lock_xact_evidence_retention = "SELECT pg_advisory_xact_lock(715423, 0)";

        /// Take the session-scoped **shared** advisory lock `(?1, ?2)`.
        ///
        /// Shared and session-scoped, not exclusive and transaction-scoped:
        /// the constraint inspector wants concurrent inspectors to proceed
        /// together and a schema migration to wait for all of them.
        lock_shared_by_pair = "SELECT pg_advisory_lock_shared(?1, ?2)";

        /// Put this transaction on a repeatable-read snapshot.
        begin_repeatable_read = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ";

        /// Put this transaction on a repeatable-read snapshot the server
        /// itself refuses to let write.
        ///
        /// `READ ONLY` makes a module's read-only promise an engine-enforced
        /// invariant rather than a property of the statements it happens to
        /// send today.
        begin_repeatable_read_read_only =
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

        /// The transaction's own instant, in epoch milliseconds.
        ///
        /// Stable for the whole transaction, so every comparison and derived
        /// expiry inside it is based on one database-owned instant.
        select_transaction_epoch_ms =
            "SELECT floor(extract(epoch FROM transaction_timestamp()) * 1000)::bigint";

        /// The server's instant *now*, in epoch milliseconds.
        ///
        /// `clock_timestamp()`, not `transaction_timestamp()`: the process
        /// registry stamps each row as it writes it, and a long transaction
        /// stamping every row with its start would make lease expiry lie.
        select_statement_epoch_ms =
            "SELECT (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT";

        /// Lower this transaction's `lock_timeout` to ten seconds unless it is
        /// already lower, leaving an operator's stricter setting alone.
        ///
        /// `0` means "wait forever", which is why it is treated as the largest
        /// value rather than the smallest.
        clamp_lock_timeout = "SELECT set_config(
             'lock_timeout',
             CASE
                 WHEN current_setting('lock_timeout') = '0'
                   OR current_setting('lock_timeout')::interval > INTERVAL '10 seconds'
                 THEN '10s'
                 ELSE current_setting('lock_timeout')
             END,
             TRUE
         )";

        /// `set_config` rather than `SET`: PostgreSQL's `SET` takes no bound
        /// parameter, so a deployment's configured timeout would have to be
        /// interpolated into the statement text — a per-connection `format!`
        /// in place of one named statement, which is the shape this arc
        /// exists to delete.
        set_lock_timeout = "SELECT set_config('lock_timeout', ?1, false)";

        /// Set this connection's `statement_timeout` to `?1` milliseconds, for
        /// the same reason.
        set_statement_timeout = "SELECT set_config('statement_timeout', ?1, false)";

        /// The lease instant a test harness pinned on this session, or `NULL`
        /// when none is pinned.
        ///
        /// Compiled only under the `testing` feature, and it costs a round
        /// trip per clock read when it is: a `lash-perf` statement count taken
        /// against a `testing` build counts this probe too (SPEC-PRELUDE,
        /// 2026-09-20).
        select_injected_lease_epoch_ms =
            "SELECT NULLIF(current_setting('lash.test_lease_epoch_ms', true), '')";
    }
}

/// Every `CHECK` constraint on the catalog OIDs in `$1`, with whether it is
/// validated, whether it is enforced, and its expression.
///
/// `conenforced` arrived in PostgreSQL 18; reading it out of the row's `jsonb`
/// projection is what lets one statement serve every supported server rather
/// than branching on `server_version_num`.
///
/// A plain constant rather than a `statements!` declaration, and that is the
/// rule for a catalog probe: it reads `pg_catalog.pg_constraint` in a table
/// position, and the renderer refuses a relation `lash-store-sql` does not
/// own — correctly, since a system catalog is not a lash table and must never
/// acquire the `lash_` prefix. So this one spells `$1` itself.
pub(crate) const SELECT_CHECK_CONSTRAINTS: &str = "SELECT c.conrelid::bigint AS table_oid,
            c.conname::text AS name,
            c.convalidated AS validated,
            COALESCE(
                (pg_catalog.to_jsonb(c) ->> 'conenforced')::boolean,
                TRUE
            ) AS enforced,
            pg_catalog.pg_get_expr(
                c.conbin,
                c.conrelid,
                false
            ) AS expression
     FROM pg_catalog.pg_constraint AS c
     WHERE c.contype = 'c'
       AND c.conrelid::bigint = ANY($1::bigint[])";

/// Every connection-scoped statement, rendered once at first use.
static CONNECTION_SQL: LazyLock<ConnectionStatements> =
    LazyLock::new(|| ConnectionStatements::render(Dialect::postgres()));

/// The connection-scoped statements, rendered once at first use.
pub(crate) fn connection_sql() -> &'static ConnectionStatements {
    &CONNECTION_SQL
}
