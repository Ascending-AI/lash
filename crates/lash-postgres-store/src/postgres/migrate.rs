//! `lashctl migrate`: the operational steps that provision, advance and
//! contract the PostgreSQL component schema (FIG-3816, FIG-3817; ADR 0106
//! §5).
//!
//! Worker opens verify and never run DDL, so something else must own "make the
//! catalog this build expects exist". This module is that something. It runs
//! the three phases of the expand/contract discipline and records every step
//! in the `lash_migrations` ledger, so a rerun is a no-op and an interrupted
//! run resumes from the rows that committed:
//!
//! - **Expand** runs before the roll, under the schema advisory lock held
//!   exclusively. An expand run on an empty database provisions the whole
//!   schema (the bootstrap step is this build's `schema.sql`, idempotent by
//!   construction); on a stamped database it walks the catalog from the found
//!   stamp forward. Each step is one transaction: statements, the stamp, the
//!   ledger row and the release stamp commit or roll back together.
//! - **Backfill** rewrites rows the new release reads, and runs only once `F`
//!   has reached the epoch whose finalize releases it: nothing rewrites a row
//!   N reads while rollback to N is still promised. `lashctl finalize` runs
//!   every pending backfill as its last step. A backfill is a sequence of
//!   batches, each one guarded transaction that rewrites a bounded run of
//!   rows after the ledger row's cursor and moves the cursor in the same
//!   commit. So a crash loses at most the batch in flight, which rolls back
//!   whole, and a resumed run starts from the last committed cursor. Every
//!   batch statement rewrites only rows still in the old shape, so a batch
//!   that runs twice changes nothing the second time.
//! - **Contract** drops or tightens what only the old release needed, raises
//!   the component's reader floor, and runs only when `F` has reached its
//!   release's epoch **and** the ledger shows every backfill it names
//!   `applied` (ADR 0106 §5). A contract step is refused typed until then.
//!
//! A constraint the new release tightens follows the same order: the
//! backfill's first batch adds it `NOT VALID`, so new rows obey it at once,
//! the batches bring the old rows into line, and the contract step validates
//! it.

use crate::guarded_tx::WriterFence;
use crate::schema_shape::{Installation, read_search_path, resolve_installation};
use crate::*;

/// The DDL that creates `lash_migrations`, byte-for-byte the block
/// `schema.sql` carries: the bootstrap and the 133→134 expand step provision
/// the identical table, and a test asserts the bytes agree.
const MIGRATIONS_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS lash_migrations (
    phase TEXT NOT NULL
        CONSTRAINT ck_lash_migrations_phase
        CHECK (phase IN ('expand', 'backfill', 'contract')),
    migration TEXT NOT NULL,
    release TEXT NOT NULL,
    state TEXT NOT NULL
        CONSTRAINT ck_lash_migrations_state
        CHECK (state IN ('running', 'applied')),
    from_version INTEGER,
    to_version INTEGER NOT NULL,
    started_at_ms BIGINT NOT NULL,
    finished_at_ms BIGINT,
    backfill_cursor TEXT,
    backfill_rows BIGINT,
    PRIMARY KEY (phase, migration),
    CONSTRAINT ck_lash_migrations_backfill_progress
        CHECK ((phase = 'backfill' AND backfill_rows IS NOT NULL AND backfill_rows >= 0)
            OR (phase <> 'backfill' AND backfill_cursor IS NULL AND backfill_rows IS NULL))
);";

/// The DDL that creates `lash_fleet_format`, byte-for-byte the block
/// `schema.sql` carries: the bootstrap and the 135→136 expand step provision
/// the identical table, and a test asserts the bytes agree.
const FLEET_FORMAT_TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS lash_fleet_format (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE,
    format_version INTEGER NOT NULL,
    finalize_hold_reason TEXT,
    finalize_held_at_ms BIGINT,
    CONSTRAINT ck_fleet_format_singleton CHECK (singleton),
    CONSTRAINT ck_fleet_format_finalize_hold
        CHECK ((finalize_hold_reason IS NULL) = (finalize_held_at_ms IS NULL))
);";

/// Phase A's single post-cut expand. None of these objects constrains writes
/// made by N: the column is nullable, the table is new, and the index is not
/// unique. The compatibility stamp moves to 2 in the same transaction.
#[cfg(feature = "synthetic-next")]
const SYNTHETIC_NEXT_EXPAND_DDL: &str = "ALTER TABLE lash_sessions
    ADD COLUMN IF NOT EXISTS synthetic_next_note TEXT;
CREATE TABLE IF NOT EXISTS lash_synthetic_next (
    id BIGSERIAL PRIMARY KEY,
    note TEXT
);
CREATE INDEX IF NOT EXISTS idx_lash_synthetic_next_note
    ON lash_synthetic_next(note);";

/// Phase A's synthetic backfill constraint: the note the synthetic expand
/// added may be absent, never empty. The backfill's first batch adds it `NOT
/// VALID`, so every row written from then on obeys it, and the synthetic
/// contract validates it once the backfill has filled every old row.
#[cfg(feature = "synthetic-next")]
const SYNTHETIC_NEXT_NOTE_CONSTRAINT_DDL: &str = "ALTER TABLE lash_sessions
    ADD CONSTRAINT ck_lash_sessions_synthetic_next_note
    CHECK (synthetic_next_note IS NULL OR synthetic_next_note <> '') NOT VALID";

/// One batch of Phase A's synthetic backfill: the sessions after the cursor,
/// in key order, each given the note the synthetic release derives from its
/// id. A session that already carries a note keeps it, so a batch that runs
/// twice rewrites nothing the second time.
#[cfg(feature = "synthetic-next")]
const SYNTHETIC_NEXT_NOTE_BATCH: &str = "WITH batch AS (
    SELECT session_id FROM lash_sessions
    WHERE $1::TEXT IS NULL OR session_id > $1::TEXT
    ORDER BY session_id
    LIMIT $2
    FOR UPDATE
), rewritten AS (
    UPDATE lash_sessions AS sessions
    SET synthetic_next_note = 'backfilled:' || sessions.session_id
    FROM batch
    WHERE sessions.session_id = batch.session_id
      AND sessions.synthetic_next_note IS NULL
    RETURNING sessions.session_id
)
SELECT (SELECT max(session_id) FROM batch),
       (SELECT count(*) FROM batch),
       (SELECT count(*) FROM rewritten)";

/// Phase A's synthetic contract: validate the note's constraint.
#[cfg(feature = "synthetic-next")]
const SYNTHETIC_NEXT_CONTRACT_DDL: &str =
    "ALTER TABLE lash_sessions VALIDATE CONSTRAINT ck_lash_sessions_synthetic_next_note";

/// The 139→140 expand step (FIG-3600 S7): the logical-root family. The
/// session head gains its closing intent, a park its engine reference and
/// resume intent, a park event the `redrive_requested` kind, and the store
/// gains `lash_session_roots`, `lash_session_root_inputs` and
/// `lash_control_intents` with their indexes, each stated as `schema.sql`
/// states it. Every statement is guarded, so a replay after a crash is a
/// no-op.
const LOGICAL_ROOT_FAMILY_DDL: &str = "ALTER TABLE lash_session_meta ADD COLUMN IF NOT EXISTS closing_intent BIGINT;
ALTER TABLE lash_turn_parks ADD COLUMN IF NOT EXISTS engine_ref TEXT;
ALTER TABLE lash_turn_parks ADD COLUMN IF NOT EXISTS resume_intent BIGINT;
ALTER TABLE lash_turn_park_events DROP CONSTRAINT IF EXISTS ck_turn_park_events_kind;
ALTER TABLE lash_turn_park_events ADD CONSTRAINT ck_turn_park_events_kind CHECK (kind IN ('parked', 'unparked', 'cancelled', 'redrive_requested'));
CREATE TABLE IF NOT EXISTS lash_session_roots (
    session_id TEXT NOT NULL,
    root TEXT NOT NULL,
    admission_json TEXT,
    admitted_generation TEXT,
    terminal_kind TEXT,
    terminal_cause_json TEXT,
    terminal_head_revision BIGINT,
    terminal_at_ms BIGINT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_session_roots_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE),
    PRIMARY KEY (session_id, root),
    CONSTRAINT ck_session_roots_terminal CHECK ((terminal_kind IS NULL AND terminal_cause_json IS NULL AND terminal_head_revision IS NULL AND terminal_at_ms IS NULL) OR (terminal_kind IN ('answered', 'failed', 'cancelled') AND terminal_cause_json IS NOT NULL AND terminal_at_ms IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS lash_session_root_inputs (
    session_id TEXT NOT NULL,
    input_id TEXT NOT NULL,
    root TEXT NOT NULL,
    PRIMARY KEY (session_id, input_id)
);
CREATE TABLE IF NOT EXISTS lash_control_intents (
    intent_id BIGSERIAL PRIMARY KEY,
    session_id TEXT NOT NULL,
    format BIGINT NOT NULL,
    kind TEXT NOT NULL CONSTRAINT ck_control_intents_kind CHECK (kind IN ('redrive', 'cancel', 'fork', 'close_session')),
    kind_json TEXT NOT NULL,
    state TEXT NOT NULL CONSTRAINT ck_control_intents_state CHECK (state IN ('pending', 'acknowledged', 'superseded', 'failed_retryable', 'failed')),
    state_json TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    engine_ref TEXT,
    obligation_id TEXT,
    obligation_state TEXT,
    obligation_attempts INTEGER NOT NULL DEFAULT 0,
    obligation_due_at_ms BIGINT,
    obligation_claim_token TEXT,
    obligation_stall_reason TEXT,
    obligation_last_error TEXT,
    obligation_settled_at_ms BIGINT,
    CONSTRAINT ck_control_intents_obligation CHECK (((obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL) OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL) OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL AND obligation_claim_token IS NULL AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable') AND obligation_settled_at_ms IS NOT NULL)) IS TRUE)
);
CREATE INDEX IF NOT EXISTS idx_lash_control_intents_session
    ON lash_control_intents(session_id, kind);";

/// One expand-phase step this build's migrate runner can apply.
///
/// `from_version`/`to_version` chain steps together so planning can walk the
/// catalog from any stamped predecessor to `SCHEMA_VERSION`. `statements` are
/// all idempotent (`IF NOT EXISTS` or constraint guards) so a step can replay
/// after a crash without tripping on its own committed output.
struct ExpandMigration {
    /// The stable ledger id; recorded, never renamed.
    id: &'static str,
    /// The stamped component version this step starts from.
    from_version: i32,
    /// The stamped component version after it applies.
    to_version: i32,
    /// Its statements, run in the step's single transaction.
    statements: &'static str,
}

/// The expand catalog this build carries. Pre-1.0 the first step let
/// component 133 gain the ledger itself and become 134 (FIG-3816); the second
/// adds `lash_sessions.pending_follow_on_json`, the frame-handoff follow-on a
/// session head carries, and becomes 135 (FIG-3542); the third adds
/// `lash_fleet_format`, the deployment's fleet-format row, and becomes 136
/// (FIG-3796); the fourth restamps 136 to 137 on a vocabulary-only change
/// (FIG-3814); the fifth adds `lash_session_meta.drive_root_start`, the
/// admitted root's start marker, and becomes 138 (FIG-3815). Newer schema
/// generations append to this list; steps are never
/// removed or edited — the ledger names them permanently.
/// Component 139 (FIG-3607) re-keys the process relations, which is no
/// expand step, so nothing chains from 138: a 133–138 catalog plans to the
/// typed recreate refusal. The sixth step adds the logical-root family and
/// carries 139 to 140 (FIG-3600); the seventh restamps 140 to 141 when the
/// journaled effect envelope gains the session close (FIG-3600).
static EXPAND_MIGRATIONS: &[ExpandMigration] = &[
    ExpandMigration {
        id: "0134-migrations-ledger",
        from_version: 133,
        to_version: 134,
        statements: MIGRATIONS_TABLE_DDL,
    },
    ExpandMigration {
        id: "0135-pending-follow-on",
        from_version: 134,
        to_version: 135,
        statements: "ALTER TABLE lash_sessions ADD COLUMN IF NOT EXISTS pending_follow_on_json TEXT",
    },
    ExpandMigration {
        id: "0136-fleet-format",
        from_version: 135,
        to_version: 136,
        statements: FLEET_FORMAT_TABLE_DDL,
    },
    ExpandMigration {
        id: "0137-runtime-error-vocabulary",
        from_version: 136,
        to_version: 137,
        statements: "-- component 137 (FIG-3814): vocabulary-only change; nothing to apply.",
    },
    ExpandMigration {
        id: "0138-drive-root-start",
        from_version: 137,
        to_version: 138,
        statements: "ALTER TABLE lash_session_meta ADD COLUMN IF NOT EXISTS drive_root_start TEXT",
    },
    ExpandMigration {
        id: "0140-logical-root-family",
        from_version: 139,
        to_version: 140,
        statements: LOGICAL_ROOT_FAMILY_DDL,
    },
    ExpandMigration {
        id: "0141-begin-session-close",
        from_version: 140,
        to_version: 141,
        statements: "-- component 141 (FIG-3600): envelope-vocabulary-only change; nothing to apply.",
    },
];

/// One backfill this build's runner can run after finalize.
pub(crate) struct BackfillMigration {
    /// The stable ledger id; recorded, never renamed.
    id: &'static str,
    /// The fleet epoch whose finalize releases it. Until `F` reaches it the
    /// backfill is refused: its rewrites are the new release's shape, which
    /// the old release must never meet while rollback to it is promised.
    after_fleet: u32,
    /// DDL the first batch's transaction runs before any row is rewritten,
    /// under the schema advisory lock: a constraint the release tightens,
    /// added `NOT VALID` (ADR 0106 §5). Empty when there is none.
    prepare: &'static str,
    /// One batch. `$1` is the ledger's cursor (`NULL` before the first
    /// batch) and `$2` the batch size. It rewrites only rows still in the old
    /// shape, and answers the last key it scanned (`NULL` when it scanned
    /// none), the rows it scanned and the rows it rewrote.
    batch: &'static str,
}

/// The backfills this build carries, in the order a run takes them. Newer
/// releases append; steps are never removed or edited — the ledger names
/// them permanently. The 1.0 release carries none.
pub(crate) static BACKFILL_MIGRATIONS: &[BackfillMigration] = &[
    #[cfg(feature = "synthetic-next")]
    BackfillMigration {
        id: "synthetic-next-session-note",
        after_fleet: 2,
        prepare: SYNTHETIC_NEXT_NOTE_CONSTRAINT_DDL,
        batch: SYNTHETIC_NEXT_NOTE_BATCH,
    },
];

/// One contract step this build carries.
struct ContractMigration {
    /// The stable ledger id; recorded, never renamed.
    id: &'static str,
    /// The fleet epoch whose finalize retired every build that still reads
    /// what this step drops or tightens.
    after_fleet: u32,
    /// The backfills the ledger must show `applied` first.
    after_backfills: &'static [&'static str],
    /// Its statements, run in the step's single transaction.
    statements: &'static str,
    /// The reader floor the step raises the component stamp to: the oldest
    /// component version that can still read the contracted catalog.
    min_reader: i32,
}

/// The contract steps this build carries. The 1.0 release carries none.
static CONTRACT_MIGRATIONS: &[ContractMigration] = &[
    #[cfg(feature = "synthetic-next")]
    ContractMigration {
        id: "synthetic-next-contract",
        after_fleet: 2,
        after_backfills: &["synthetic-next-session-note"],
        statements: SYNTHETIC_NEXT_CONTRACT_DDL,
        min_reader: 2,
    },
];

/// The rows one backfill batch covers when the operator binary runs it.
pub(crate) const BACKFILL_BATCH_ROWS: i64 = 500;

/// The phase a migrate run is asked to execute (ADR 0106 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationPhase {
    /// Create and alter the objects the new generation reads; safe to run
    /// while old-build workers still serve the catalog.
    Expand,
    /// Rewrite rows into the new release's shape, in resumable batches; runs
    /// only after finalize, and `lashctl finalize` runs it as its last step.
    Backfill,
    /// Drop or tighten what only the old release needed, and raise the
    /// reader floor; runs only after finalize and once every backfill it
    /// names is complete.
    Contract,
}

impl MigrationPhase {
    /// The phase's ledger and CLI spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Expand => "expand",
            Self::Backfill => "backfill",
            Self::Contract => "contract",
        }
    }

    /// The phase names `lashctl migrate --phase` accepts.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "expand" => Some(Self::Expand),
            "backfill" => Some(Self::Backfill),
            "contract" => Some(Self::Contract),
            _ => None,
        }
    }
}

/// A backfill or contract step that must not run yet. Nothing changed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum MigrationRefusal {
    /// Backfill and contract act on an installed catalog only.
    Unprovisioned { phase: String },
    /// `F` has not reached the epoch whose finalize releases the backfill.
    BackfillBeforeFinalize {
        migration: String,
        recorded: u32,
        requires: u32,
    },
    /// `F` has not reached the epoch whose finalize retired the builds the
    /// contract step would exclude.
    ContractBeforeFinalize {
        migration: String,
        recorded: u32,
        requires: u32,
    },
    /// The ledger does not show every backfill the contract step names
    /// `applied`.
    ContractBeforeBackfills {
        migration: String,
        pending: Vec<String>,
    },
}

impl std::fmt::Display for MigrationRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unprovisioned { phase } => write!(
                formatter,
                "`lashctl migrate --phase {phase}` needs an installed catalog: run \
                 `lashctl migrate --phase expand` first"
            ),
            Self::BackfillBeforeFinalize {
                migration,
                recorded,
                requires,
            } => write!(
                formatter,
                "backfill {migration} runs only after finalize: the store records F={recorded} \
                 and the backfill needs F={requires}; run `lashctl finalize` first"
            ),
            Self::ContractBeforeFinalize {
                migration,
                recorded,
                requires,
            } => write!(
                formatter,
                "contract {migration} runs only after finalize: the store records F={recorded} \
                 and the step needs F={requires}; run `lashctl finalize` first"
            ),
            Self::ContractBeforeBackfills { migration, pending } => write!(
                formatter,
                "contract {migration} waits for its backfills: {} not yet applied; run \
                 `lashctl migrate --phase backfill` to finish them",
                pending.join(", ")
            ),
        }
    }
}

impl std::error::Error for MigrationRefusal {}

/// Why a migrate run did not complete.
#[derive(Debug)]
pub enum MigrateError {
    /// A step's precondition does not hold; the step changed nothing.
    Refused(MigrationRefusal),
    /// The store refused or failed.
    Store(StoreError),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refusal) => refusal.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for MigrateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            Self::Store(error) => Some(error),
        }
    }
}

impl From<StoreError> for MigrateError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<MigrationRefusal> for MigrateError {
    fn from(refusal: MigrationRefusal) -> Self {
        Self::Refused(refusal)
    }
}

/// One recorded or planned migrate step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationStep {
    /// `expand`, `backfill`, or `contract`.
    pub phase: String,
    /// The step's ledger id — `bootstrap-{version}` for a fresh provision.
    pub migration: String,
    /// The release that applied the step; empty while the step is only planned.
    pub release: String,
    /// `running` or `applied`. Expand and contract rows are always `applied`;
    /// a backfill is `running` from its first batch until the batch that
    /// finds nothing left.
    pub state: String,
    /// The stamped version it started from; `None` only for a bootstrap.
    pub from_version: Option<i32>,
    /// The stamped version after it applies.
    pub to_version: i32,
    /// Server-clock instant the step's transaction began; `None` while the step
    /// is only planned.
    pub started_at_ms: Option<i64>,
    /// Server-clock instant the step finished; `None` while it is planned or
    /// still running.
    pub finished_at_ms: Option<i64>,
    /// A backfill's cursor: the key its last committed batch ended at.
    pub backfill_cursor: Option<String>,
    /// The rows a backfill has rewritten so far; `None` for other phases.
    pub backfill_rows: Option<i64>,
}

/// What a plan or run found on the database and did about it.
#[derive(Clone, Debug)]
pub struct MigrationReport {
    /// The schema the lash installation resolved to; `None` on an
    /// unprovisioned database.
    pub namespace: Option<String>,
    /// The stamped component version before the run; `None` when the database
    /// had no installation or no readable stamp.
    pub found_version: Option<i32>,
    /// Ledger rows committed before this run, oldest first.
    pub applied: Vec<MigrationStep>,
    /// Steps this run committed, in order. Empty on a rerun and on a plan.
    pub executed: Vec<MigrationStep>,
    /// Steps a run would still commit, in order. On a plan this is the whole
    /// answer; after a run it is empty.
    pub planned: Vec<MigrationStep>,
}

/// What the catalog read says the database is.
struct MigrationState {
    /// The resolved installation, or `None` on an unprovisioned database.
    installation: Option<Installation>,
    /// The last DDL step recorded by the migration ledger. Compatibility
    /// versions are a separate sequence in `lash_schema_versions`.
    ddl_version: Option<i32>,
    /// Ledger rows already committed.
    applied: Vec<MigrationStep>,
    /// The writing release, when the release stamp could still be read.
    writing_release: Option<String>,
    /// The fleet epoch the store records, when it records one.
    fleet: Option<u32>,
}

impl MigrationState {
    /// Whether the ledger shows `migration` of `phase` applied.
    fn is_applied(&self, phase: MigrationPhase, migration: &str) -> bool {
        self.applied.iter().any(|step| {
            step.phase == phase.name() && step.migration == migration && step.state == "applied"
        })
    }
}

/// One step a plan produced.
enum PlannedStep<'a> {
    /// Provision the whole schema on an unprovisioned database.
    Bootstrap,
    /// Apply a catalog migration.
    Migration(&'a ExpandMigration),
}

/// The ledger's columns, in the order every read selects them.
const LEDGER_COLUMNS: &str = "phase, migration, release, state, from_version, to_version,
     started_at_ms, finished_at_ms, backfill_cursor, backfill_rows";

type LedgerRow = (
    String,
    String,
    String,
    String,
    Option<i32>,
    i32,
    i64,
    Option<i64>,
    Option<String>,
    Option<i64>,
);

fn ledger_step(row: LedgerRow) -> MigrationStep {
    let (
        phase,
        migration,
        release,
        state,
        from_version,
        to_version,
        started_at_ms,
        finished_at_ms,
        backfill_cursor,
        backfill_rows,
    ) = row;
    MigrationStep {
        phase,
        migration,
        release,
        state,
        from_version,
        to_version,
        started_at_ms: Some(started_at_ms),
        finished_at_ms,
        backfill_cursor,
        backfill_rows,
    }
}

/// Reads the stamp, the ledger, the fleet epoch and the release stamp inside
/// one transaction snapshot, so a plan describes one instant of the
/// database.
async fn read_state(
    tx: &mut sqlx::Transaction<'_, Postgres>,
) -> Result<MigrationState, StoreError> {
    let search_path = read_search_path(tx).await?;
    let Some(installation) = resolve_installation(tx, &search_path).await? else {
        return Ok(MigrationState {
            installation: None,
            ddl_version: None,
            applied: Vec::new(),
            writing_release: None,
            fleet: None,
        });
    };
    // Probed by OID against the anchored namespace, like every other object
    // read here: a `to_regclass` name lookup would resolve outside the
    // transaction's snapshot.
    let ledger_present: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM pg_catalog.pg_class
             WHERE relnamespace = $1 AND relname = 'lash_migrations'
               AND relkind IN ('r', 'p'))",
    )
    .bind(installation.namespace_oid())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let applied = if ledger_present {
        sqlx::query_as::<_, LedgerRow>(&format!(
            "SELECT {LEDGER_COLUMNS}
                 FROM {}.lash_migrations ORDER BY started_at_ms, migration",
            installation.quoted_namespace()
        ))
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?
        .into_iter()
        .map(ledger_step)
        .collect()
    } else {
        Vec::new()
    };
    let ddl_version = if !ledger_present {
        // A catalog older than the ledger (component 134) recorded its DDL
        // revision only in its pre-1.0 stamp, so that is the revision the
        // planner refuses by name.
        read_legacy_stamp_version(tx, &installation).await?
    } else if applied.is_empty() {
        // Hosts may install the published schema.sql directly. Its 1.0
        // compatibility stamp is not the pre-1.0 DDL migration counter.
        let report = verify_schema_shape(&mut *tx).await?;
        report.is_conformant().then_some(SCHEMA_VERSION)
    } else {
        applied.iter().map(|step| step.to_version).max()
    };
    let fleet = match crate::fleet_format::read_state_in_tx(tx)
        .await
        .map_err(store_sqlx_error)?
    {
        lash_core_execution::FleetFormatState::Recorded(fleet) => Some(fleet.version()),
        _ => None,
    };
    // Last: on a catalog that predates the release stamp the read fails,
    // which aborts the snapshot for any statement after it.
    let writing_release = crate::release_stamp::read_release_in_tx(tx).await;
    Ok(MigrationState {
        installation: Some(installation),
        ddl_version,
        applied,
        writing_release,
        fleet,
    })
}

/// The DDL revision a pre-1.0 stamp records: `lash_schema_versions.version`
/// on a stamp table without `min_reader`, the shape every catalog had before
/// the 1.0 compatibility stamp (ADR 0115 §1.2). A stamp that carries
/// `min_reader` is a compatibility stamp, whose version is no DDL revision,
/// and a catalog with no stamp table has none; both answer `None`.
async fn read_legacy_stamp_version(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    installation: &Installation,
) -> Result<Option<i32>, StoreError> {
    let (stamp_present, compat_stamp): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS (
                 SELECT 1 FROM pg_catalog.pg_class
                 WHERE relnamespace = $1 AND relname = 'lash_schema_versions'
                   AND relkind IN ('r', 'p')),
             EXISTS (
                 SELECT 1 FROM pg_catalog.pg_class AS relation
                 JOIN pg_catalog.pg_attribute AS attribute
                   ON attribute.attrelid = relation.oid
                 WHERE relation.relnamespace = $1
                   AND relation.relname = 'lash_schema_versions'
                   AND relation.relkind IN ('r', 'p')
                   AND attribute.attname = 'min_reader'
                   AND NOT attribute.attisdropped)",
    )
    .bind(installation.namespace_oid())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if !stamp_present || compat_stamp {
        return Ok(None);
    }
    sqlx::query_scalar(&format!(
        "SELECT version FROM {}.lash_schema_versions WHERE component = $1",
        installation.quoted_namespace()
    ))
    .bind(SCHEMA_COMPONENT)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)
}

/// Turns a read of the database into the ordered steps a run still owes it.
///
/// The walk is the whole planning algorithm: from the found stamp, chain
/// catalog steps whose ids are not yet in the ledger until `SCHEMA_VERSION` is
/// reached. A stamp with no chain is a recreate boundary, refused by the same
/// typed error an open would raise — "no applicable migration" is the honest
/// statement there too.
fn plan(state: &MigrationState) -> Result<Vec<PlannedStep<'_>>, StoreError> {
    let Some(installation) = &state.installation else {
        // Nothing provisioned: the bootstrap applies this build's schema.sql,
        // whose seed statements stamp the current component.
        return Ok(vec![PlannedStep::Bootstrap]);
    };
    let installed = Some(installation.namespace());
    let release = state.writing_release.as_deref();
    let Some(mut at) = state.ddl_version else {
        return Err(version_mismatch_error(installed, None, release));
    };
    let found = Some(at);
    if at > SCHEMA_VERSION {
        // A newer build's catalog: Lash never migrates backwards, and the
        // typed refusal says so.
        return Err(version_mismatch_error(installed, found, release));
    }
    let mut pending = Vec::new();
    while at < SCHEMA_VERSION {
        let Some(migration) = EXPAND_MIGRATIONS
            .iter()
            .find(|migration| migration.from_version == at)
        else {
            return Err(version_mismatch_error(installed, found, release));
        };
        // A committed ledger row means this step finished; skip it and keep
        // walking, which is what makes a resumed run pick up where it stopped.
        if !state
            .applied
            .iter()
            .any(|step| step.migration == migration.id)
        {
            pending.push(PlannedStep::Migration(migration));
        }
        at = migration.to_version;
    }
    Ok(pending)
}

/// The server clock's current instant in epoch milliseconds — the ledger's
/// timestamps are the database's, not the migrating host's, the same rule the
/// release stamp follows.
async fn server_clock_ms(tx: &mut sqlx::Transaction<'_, Postgres>) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)")
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

/// Records one applied step in the ledger and returns its committed row's
/// instants. `ON CONFLICT DO NOTHING` keeps the statement honest when a row
/// survived from a run whose planning missed it; the read-back then returns
/// the original row rather than rewriting history.
async fn record_step(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    ledger: &str,
    phase: &str,
    migration: &str,
    from_version: Option<i32>,
    to_version: i32,
    started_at_ms: i64,
) -> Result<(String, String, i64, Option<i64>), StoreError> {
    let recorded: Option<(String, String, i64, Option<i64>)> = sqlx::query_as(&format!(
        "INSERT INTO {ledger} (phase, migration, release, state,
                               from_version, to_version, started_at_ms, finished_at_ms)
         VALUES ($1, $2, $3, 'applied', $4, $5, $6,
                 CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT))
         ON CONFLICT (phase, migration) DO NOTHING
         RETURNING release, state, started_at_ms, finished_at_ms"
    ))
    .bind(phase)
    .bind(migration)
    .bind(crate::release_stamp::BUILD_RELEASE)
    .bind(from_version)
    .bind(to_version)
    .bind(started_at_ms)
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    match recorded {
        Some(row) => Ok(row),
        None => sqlx::query_as(&format!(
            "SELECT release, state, started_at_ms, finished_at_ms FROM {ledger}
             WHERE phase = $1 AND migration = $2"
        ))
        .bind(phase)
        .bind(migration)
        .fetch_one(&mut **tx)
        .await
        .map_err(store_sqlx_error),
    }
}

/// Applies one planned step in its own transaction: the step's statements, the
/// component-stamp move to `to_version`, the ledger row, and the release stamp
/// all commit or roll back together.
async fn apply_step(
    connection: &mut sqlx::PgConnection,
    fence: &WriterFence,
    installation: Option<&Installation>,
    step: &PlannedStep<'_>,
) -> Result<MigrationStep, StoreError> {
    let mut tx = crate::guarded_tx::begin_migration(connection, fence).await?;
    let started_at_ms = server_clock_ms(&mut tx).await?;
    let (migration, from_version, to_version, ledger) = match step {
        PlannedStep::Bootstrap => {
            sqlx::raw_sql(SCHEMA_DDL)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            // The bootstrap just created the ledger in the search_path's first
            // schema, so the unqualified name resolves to it.
            (
                format!("bootstrap-{SCHEMA_VERSION}"),
                None,
                SCHEMA_VERSION,
                "lash_migrations".to_string(),
            )
        }
        PlannedStep::Migration(migration) => {
            // plan only produces a catalog step for a resolved installation.
            let Some(installation) = installation else {
                return Err(StoreError::Backend(
                    "a catalog migration was planned for a database with no lash installation"
                        .to_string(),
                ));
            };
            sqlx::raw_sql(migration.statements)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            // Moving the stamp is part of the step: a later open sees either
            // the whole committed step or the version it started from.
            sqlx::query(&format!(
                "INSERT INTO {}.lash_schema_versions (component, version, min_reader)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (component) DO NOTHING",
                installation.quoted_namespace()
            ))
            .bind(SCHEMA_COMPONENT)
            .bind(1_i32)
            .bind(1_i32)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
            (
                migration.id.to_string(),
                Some(migration.from_version),
                migration.to_version,
                format!("{}.lash_migrations", installation.quoted_namespace()),
            )
        }
    };
    let (release, state, started_at_ms, finished_at_ms) = record_step(
        &mut tx,
        &ledger,
        MigrationPhase::Expand.name(),
        &migration,
        from_version,
        to_version,
        started_at_ms,
    )
    .await?;
    crate::release_stamp::write(&mut tx)
        .await
        .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(MigrationStep {
        phase: MigrationPhase::Expand.name().to_string(),
        migration,
        release,
        state,
        from_version,
        to_version,
        started_at_ms: Some(started_at_ms),
        finished_at_ms,
        backfill_cursor: None,
        backfill_rows: None,
    })
}

/// Phase A's synthetic expand: the one post-cut expand step, moving the
/// component stamp to `next`, the version the synthetic build writes.
#[cfg(feature = "synthetic-next")]
async fn apply_synthetic_expand(
    connection: &mut sqlx::PgConnection,
    fence: &WriterFence,
    next: i32,
) -> Result<Option<MigrationStep>, StoreError> {
    let mut tx = crate::guarded_tx::begin_migration(connection, fence).await?;
    let version: i32 =
        sqlx::query_scalar("SELECT version FROM lash_schema_versions WHERE component = $1")
            .bind(SCHEMA_COMPONENT)
            .fetch_one(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    if version == next {
        return Ok(None);
    }
    if version + 1 != next {
        let descriptor = lash_core_execution::compat::descriptor(
            lash_core_execution::compat::ComponentId::POSTGRES,
        )
        .ok_or_else(|| {
            StoreError::Backend("the build has no descriptor for the PostgreSQL store".into())
        })?;
        return Err(StoreError::Incompatible {
            refusal: lash_core_execution::compat::CompatRefusal::TooOld {
                component: descriptor.component.as_str().to_owned(),
                found: u32::try_from(version).unwrap_or_default(),
                reads: descriptor.reads,
                writing_release: None,
            },
        });
    }
    let started_at_ms = server_clock_ms(&mut tx).await?;
    sqlx::raw_sql(SYNTHETIC_NEXT_EXPAND_DDL)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query("UPDATE lash_schema_versions SET version = $1 WHERE component = $2")
        .bind(next)
        .bind(SCHEMA_COMPONENT)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let migration = "synthetic-next-expand";
    let (release, state, started_at_ms, finished_at_ms) = record_step(
        &mut tx,
        "lash_migrations",
        MigrationPhase::Expand.name(),
        migration,
        Some(SCHEMA_VERSION),
        SCHEMA_VERSION,
        started_at_ms,
    )
    .await?;
    crate::release_stamp::write(&mut tx)
        .await
        .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(Some(MigrationStep {
        phase: MigrationPhase::Expand.name().to_string(),
        migration: migration.to_string(),
        release,
        state,
        from_version: Some(SCHEMA_VERSION),
        to_version: SCHEMA_VERSION,
        started_at_ms: Some(started_at_ms),
        finished_at_ms,
        backfill_cursor: None,
        backfill_rows: None,
    }))
}

/// The component version the synthetic build's expand writes: its
/// descriptor's.
#[cfg(feature = "synthetic-next")]
fn synthetic_next_component_version() -> Result<i32, StoreError> {
    let descriptor =
        lash_core_execution::compat::descriptor(lash_core_execution::compat::ComponentId::POSTGRES)
            .ok_or_else(|| {
                StoreError::Backend("the build has no descriptor for the PostgreSQL store".into())
            })?;
    i32::try_from(descriptor.writes.max()).map_err(|error| StoreError::Backend(error.to_string()))
}

/// Run Phase A's synthetic expand to component `next` under the exclusive
/// schema lock, as a build whose fence is `fence`: the laws' way to stand in
/// N+1's `lashctl migrate` on a store both builds then open with explicit
/// writable ranges.
#[cfg(all(test, feature = "synthetic-next"))]
pub(crate) async fn expand_synthetic_next_for_testing(
    pool: &PgPool,
    fence: &WriterFence,
    next: i32,
) -> Result<(), StoreError> {
    let mut connection = lock_connection(pool, false).await?;
    let result = apply_synthetic_expand(&mut connection, fence, next).await;
    let _ = sqlx::Connection::close(connection).await;
    result.map(|_| ())
}

/// Seeds `F` at this build's [`FleetFormat::seed`] when the store records
/// none (ADR 0115 §2.1), still under the exclusive advisory lock.
///
/// The installer owns the fleet epoch and an open never records it: were the
/// first opener to decide it, an N+1 that opened a freshly migrated store
/// before any N would record its own epoch and skip the rollback window. The
/// bootstrap's seed rows already carry it; this also covers every run on an
/// installed catalog, including a rerun against one that a build predating
/// the seed migrated. A recorded epoch is left alone.
///
/// [`FleetFormat::seed`]: lash_core_execution::FleetFormat::seed
async fn seed_fleet_format(
    connection: &mut sqlx::PgConnection,
    fence: &WriterFence,
) -> Result<(), StoreError> {
    let mut tx = crate::guarded_tx::begin_migration(connection, fence).await?;
    crate::fleet_format::seed(
        &mut tx,
        lash_core_execution::FleetFormat::seed(fence.writable()),
    )
    .await?;
    tx.commit().await.map_err(store_sqlx_error)
}

fn report(state: &MigrationState, executed: Vec<MigrationStep>) -> MigrationReport {
    MigrationReport {
        namespace: state
            .installation
            .as_ref()
            .map(|installation| installation.namespace().to_string()),
        found_version: state.ddl_version,
        applied: state.applied.clone(),
        executed,
        planned: Vec::new(),
    }
}

/// Takes the advisory lock in the requested mode on a detached connection.
///
/// Session-scoped locks demand this shape — the connection is owned, so a
/// cancelled future or an error path still releases the lock when the session
/// closes rather than handing a locked connection back to the pool. See
/// [`verify_schema_under_advisory_lock`] for the full reasoning.
async fn lock_connection(pool: &PgPool, shared: bool) -> Result<sqlx::PgConnection, StoreError> {
    let (lock_namespace, lock_key) = SCHEMA_ADVISORY_LOCK_KEY;
    let mut connection = pool.acquire().await.map_err(store_sqlx_error)?.detach();
    let locked = sqlx::query(if shared {
        "SELECT pg_advisory_lock_shared($1, $2)"
    } else {
        "SELECT pg_advisory_lock($1, $2)"
    })
    .bind(lock_namespace)
    .bind(lock_key)
    .execute(&mut connection)
    .await
    .map_err(store_sqlx_error);
    match locked {
        Ok(_) => Ok(connection),
        Err(error) => {
            let _ = sqlx::Connection::close(connection).await;
            Err(error)
        }
    }
}

/// Reads the migrate state under a `REPEATABLE READ` snapshot: the lock the
/// caller holds is session-scoped, so the transaction must start only after
/// the lock landed — otherwise the snapshot would predate it.
async fn read_state_under_lock(
    connection: &mut sqlx::PgConnection,
) -> Result<MigrationState, StoreError> {
    let mut tx = sqlx::Connection::begin(&mut *connection)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
    let state = read_state(&mut tx).await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(state)
}

fn planned_step(step: &PlannedStep<'_>) -> MigrationStep {
    match step {
        PlannedStep::Bootstrap => MigrationStep {
            phase: MigrationPhase::Expand.name().to_string(),
            migration: format!("bootstrap-{SCHEMA_VERSION}"),
            release: String::new(),
            state: String::new(),
            from_version: None,
            to_version: SCHEMA_VERSION,
            started_at_ms: None,
            finished_at_ms: None,
            backfill_cursor: None,
            backfill_rows: None,
        },
        PlannedStep::Migration(migration) => MigrationStep {
            phase: MigrationPhase::Expand.name().to_string(),
            migration: migration.id.to_string(),
            release: String::new(),
            state: String::new(),
            from_version: Some(migration.from_version),
            to_version: migration.to_version,
            started_at_ms: None,
            finished_at_ms: None,
            backfill_cursor: None,
            backfill_rows: None,
        },
    }
}

/// A backfill or contract step not yet recorded, as a plan lists it.
fn planned_later_step(phase: MigrationPhase, migration: &str) -> MigrationStep {
    MigrationStep {
        phase: phase.name().to_string(),
        migration: migration.to_string(),
        release: String::new(),
        state: String::new(),
        from_version: Some(SCHEMA_VERSION),
        to_version: SCHEMA_VERSION,
        started_at_ms: None,
        finished_at_ms: None,
        backfill_cursor: None,
        backfill_rows: (phase == MigrationPhase::Backfill).then_some(0),
    }
}

/// The backfills a run still owes the store, refused typed when `F` has not
/// reached the epoch that releases one.
fn pending_backfills(
    state: &MigrationState,
) -> Result<Vec<&'static BackfillMigration>, MigrationRefusal> {
    let mut pending = Vec::new();
    for backfill in BACKFILL_MIGRATIONS {
        if state.is_applied(MigrationPhase::Backfill, backfill.id) {
            continue;
        }
        let recorded = state.fleet.unwrap_or_default();
        if recorded < backfill.after_fleet {
            return Err(MigrationRefusal::BackfillBeforeFinalize {
                migration: backfill.id.to_owned(),
                recorded,
                requires: backfill.after_fleet,
            });
        }
        pending.push(backfill);
    }
    Ok(pending)
}

/// The contract steps a run still owes the store, refused typed until `F`
/// has reached each one's epoch and the ledger shows its backfills applied.
fn pending_contracts(
    state: &MigrationState,
) -> Result<Vec<&'static ContractMigration>, MigrationRefusal> {
    let mut pending = Vec::new();
    for contract in CONTRACT_MIGRATIONS {
        if state.is_applied(MigrationPhase::Contract, contract.id) {
            continue;
        }
        contract_admitted(contract, state.fleet.unwrap_or_default(), |backfill| {
            state.is_applied(MigrationPhase::Backfill, backfill)
        })?;
        pending.push(contract);
    }
    Ok(pending)
}

/// A contract step's gate (ADR 0106 §5): `F` has reached its epoch, and
/// every backfill it names is applied.
fn contract_admitted(
    contract: &ContractMigration,
    recorded: u32,
    applied: impl Fn(&str) -> bool,
) -> Result<(), MigrationRefusal> {
    if recorded < contract.after_fleet {
        return Err(MigrationRefusal::ContractBeforeFinalize {
            migration: contract.id.to_owned(),
            recorded,
            requires: contract.after_fleet,
        });
    }
    let pending: Vec<String> = contract
        .after_backfills
        .iter()
        .filter(|backfill| !applied(backfill))
        .map(|backfill| (*backfill).to_owned())
        .collect();
    if !pending.is_empty() {
        return Err(MigrationRefusal::ContractBeforeBackfills {
            migration: contract.id.to_owned(),
            pending,
        });
    }
    Ok(())
}

/// Plans what a run of `phase` owes the database without changing it: the
/// shared advisory lock and the repeatable-read snapshot are exactly what a
/// verifying open takes, so the answer cannot describe a half-applied state.
/// A backfill or contract step whose gate is closed is refused, typed, as
/// the run would refuse it.
pub(crate) async fn plan_on(
    pool: &PgPool,
    phase: MigrationPhase,
) -> Result<MigrationReport, MigrateError> {
    let mut connection = lock_connection(pool, true).await?;
    let result = async {
        let state = read_state_under_lock(&mut connection).await?;
        let planned = match phase {
            MigrationPhase::Expand => plan(&state)?.iter().map(planned_step).collect(),
            MigrationPhase::Backfill | MigrationPhase::Contract if state.installation.is_none() => {
                return Err(MigrationRefusal::Unprovisioned {
                    phase: phase.name().to_owned(),
                }
                .into());
            }
            MigrationPhase::Backfill => pending_backfills(&state)?
                .into_iter()
                .map(|backfill| planned_later_step(phase, backfill.id))
                .collect(),
            MigrationPhase::Contract => pending_contracts(&state)?
                .into_iter()
                .map(|contract| planned_later_step(phase, contract.id))
                .collect(),
        };
        let mut planned_report = report(&state, Vec::new());
        planned_report.planned = planned;
        Ok(planned_report)
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    result
}

/// Runs `phase` as this build: expand under the exclusive advisory lock,
/// backfill in batches of [`BACKFILL_BATCH_ROWS`], contract under the
/// exclusive lock.
pub(crate) async fn migrate_on(
    pool: &PgPool,
    phase: MigrationPhase,
) -> Result<MigrationReport, MigrateError> {
    run_phase(
        pool,
        phase,
        &WriterFence::of_this_build(),
        BACKFILL_BATCH_ROWS,
    )
    .await
}

/// Runs `phase` as the build whose writer fence is `fence`.
pub(crate) async fn run_phase(
    pool: &PgPool,
    phase: MigrationPhase,
    fence: &WriterFence,
    batch_rows: i64,
) -> Result<MigrationReport, MigrateError> {
    match phase {
        MigrationPhase::Expand => Ok(expand_on(pool, fence).await?),
        MigrationPhase::Backfill => backfill_on(pool, fence, batch_rows).await,
        MigrationPhase::Contract => contract_on(pool, fence).await,
    }
}

/// Runs the pending expand migrations under the exclusive advisory lock.
///
/// The lock is the same key every verifying open holds while it reads the
/// catalog, so a worker can never verify against a half-applied batch and a
/// second `lashctl migrate` queues behind rather than racing this one.
async fn expand_on(pool: &PgPool, fence: &WriterFence) -> Result<MigrationReport, StoreError> {
    let mut connection = lock_connection(pool, false).await?;
    let result = async {
        let state = read_state_under_lock(&mut connection).await?;
        let pending = plan(&state)?;
        let mut executed = Vec::with_capacity(pending.len());
        for step in &pending {
            executed
                .push(apply_step(&mut connection, fence, state.installation.as_ref(), step).await?);
        }
        #[cfg(feature = "synthetic-next")]
        if let Some(step) =
            apply_synthetic_expand(&mut connection, fence, synthetic_next_component_version()?)
                .await?
        {
            executed.push(step);
        }
        seed_fleet_format(&mut connection, fence).await?;
        let mut result = report(&state, executed);
        if !result.executed.is_empty() {
            let namespace = verify_changed_catalog(&mut connection, result.executed.len()).await?;
            if result.namespace.is_none() {
                result.namespace = namespace;
            }
        }
        Ok(result)
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    result
}

/// A run that changed the catalog proves it before releasing the lock: the
/// structural check is the same one an open would run, so a migrated
/// database that cannot open fails here, not at the first worker's startup.
/// The verification also resolves the namespace a bootstrap just installed.
async fn verify_changed_catalog(
    connection: &mut sqlx::PgConnection,
    steps: usize,
) -> Result<Option<String>, StoreError> {
    let verification = verify_schema_shape(&mut *connection).await?;
    #[cfg(not(feature = "synthetic-next"))]
    let conformant = verification.is_conformant();
    #[cfg(feature = "synthetic-next")]
    let conformant = {
        let mut tx = sqlx::Connection::begin(&mut *connection)
            .await
            .map_err(store_sqlx_error)?;
        let findings = crate::schema_shape::synthetic_next_findings(&mut tx, &verification).await?;
        tx.rollback().await.map_err(store_sqlx_error)?;
        findings.is_empty()
    };
    if !conformant {
        return Err(StoreError::Backend(format!(
            "`lashctl migrate` applied {steps} step(s) but the resulting schema is not \
             conformant — do not start workers against it: {verification}"
        )));
    }
    Ok(verification.schema)
}

/// What one backfill batch did.
pub(crate) enum BatchOutcome {
    /// It rewrote a run of rows; more may follow.
    Progressed,
    /// It found nothing left, and recorded the backfill `applied`.
    Completed,
}

/// Start `backfill` if its ledger row does not exist yet: the prepare DDL and
/// the `running` row commit together, under the exclusive schema lock, in a
/// migration transaction fenced like every other.
///
/// The lock comes before the fence here, as it does for every migrate step,
/// so a backfill that starts never waits on the schema lock while it holds
/// the fence row.
pub(crate) async fn start_backfill(
    pool: &PgPool,
    fence: &WriterFence,
    backfill: &BackfillMigration,
) -> Result<(), MigrateError> {
    let started: Option<String> = sqlx::query_scalar(
        "SELECT state FROM lash_migrations WHERE phase = 'backfill' AND migration = $1",
    )
    .bind(backfill.id)
    .fetch_optional(pool)
    .await
    .map_err(store_sqlx_error)?;
    if started.is_some() {
        return Ok(());
    }
    let mut connection = lock_connection(pool, false).await?;
    let result = async {
        let mut tx = crate::guarded_tx::begin_migration(&mut connection, fence).await?;
        let recorded = tx.fleet().version();
        if recorded < backfill.after_fleet {
            return Err(MigrationRefusal::BackfillBeforeFinalize {
                migration: backfill.id.to_owned(),
                recorded,
                requires: backfill.after_fleet,
            }
            .into());
        }
        // Another run may have started it while this one waited for the lock.
        let started: Option<String> = sqlx::query_scalar(
            "SELECT state FROM lash_migrations
             WHERE phase = 'backfill' AND migration = $1
             FOR UPDATE",
        )
        .bind(backfill.id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if started.is_some() {
            return Ok(());
        }
        let started_at_ms = server_clock_ms(&mut tx).await?;
        if !backfill.prepare.is_empty() {
            sqlx::raw_sql(backfill.prepare)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
        sqlx::query(
            "INSERT INTO lash_migrations (phase, migration, release, state,
                 from_version, to_version, started_at_ms, finished_at_ms,
                 backfill_cursor, backfill_rows)
             VALUES ('backfill', $1, $2, 'running', $3, $3, $4, NULL, NULL, 0)",
        )
        .bind(backfill.id)
        .bind(crate::release_stamp::BUILD_RELEASE)
        .bind(SCHEMA_VERSION)
        .bind(started_at_ms)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(())
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    result
}

/// One batch of `backfill` in one guarded transaction: the rows after the
/// ledger's cursor rewritten, and the cursor, the row count and — when the
/// batch found fewer rows than it asked for — the `applied` state moved in
/// the same commit. A batch that does not commit changed nothing.
pub(crate) async fn backfill_batch(
    pool: &PgPool,
    fence: &WriterFence,
    backfill: &BackfillMigration,
    batch_rows: i64,
) -> Result<BatchOutcome, MigrateError> {
    let mut tx = crate::guarded_tx::begin_guarded(pool, fence).await?;
    let outcome = backfill_batch_in(&mut tx, backfill, batch_rows).await?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(outcome)
}

/// [`backfill_batch`]'s statements, inside the guarded transaction `tx`.
pub(crate) async fn backfill_batch_in(
    tx: &mut crate::guarded_tx::GuardedTx<'_>,
    backfill: &BackfillMigration,
    batch_rows: i64,
) -> Result<BatchOutcome, MigrateError> {
    let recorded = tx.fleet().version();
    if recorded < backfill.after_fleet {
        return Err(MigrationRefusal::BackfillBeforeFinalize {
            migration: backfill.id.to_owned(),
            recorded,
            requires: backfill.after_fleet,
        }
        .into());
    }
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT state, backfill_cursor FROM lash_migrations
         WHERE phase = 'backfill' AND migration = $1
         FOR UPDATE",
    )
    .bind(backfill.id)
    .fetch_optional(&mut ***tx)
    .await
    .map_err(store_sqlx_error)?;
    let cursor = match row {
        None => {
            return Err(StoreError::Backend(format!(
                "backfill {} has no ledger row to resume from",
                backfill.id
            ))
            .into());
        }
        Some((state, _)) if state == "applied" => return Ok(BatchOutcome::Completed),
        Some((_, cursor)) => cursor,
    };
    let (last_key, scanned, rewritten): (Option<String>, i64, i64) = sqlx::query_as(backfill.batch)
        .bind(cursor.as_deref())
        .bind(batch_rows)
        .fetch_one(&mut ***tx)
        .await
        .map_err(store_sqlx_error)?;
    let completed = scanned < batch_rows;
    sqlx::query(
        "UPDATE lash_migrations
         SET backfill_cursor = COALESCE($2, backfill_cursor),
             backfill_rows = backfill_rows + $3,
             state = CASE WHEN $4 THEN 'applied' ELSE 'running' END,
             finished_at_ms = CASE WHEN $4
                 THEN CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)
                 ELSE NULL END
         WHERE phase = 'backfill' AND migration = $1",
    )
    .bind(backfill.id)
    .bind(last_key)
    .bind(rewritten)
    .bind(completed)
    .execute(&mut ***tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(if completed {
        BatchOutcome::Completed
    } else {
        BatchOutcome::Progressed
    })
}

/// The ledger row of one step.
async fn ledger_row(
    pool: &PgPool,
    phase: MigrationPhase,
    migration: &str,
) -> Result<MigrationStep, StoreError> {
    sqlx::query_as::<_, LedgerRow>(&format!(
        "SELECT {LEDGER_COLUMNS} FROM lash_migrations WHERE phase = $1 AND migration = $2"
    ))
    .bind(phase.name())
    .bind(migration)
    .fetch_one(pool)
    .await
    .map(ledger_step)
    .map_err(store_sqlx_error)
}

/// Run every backfill this build carries that the ledger does not show
/// `applied`, each to completion, in batches of `batch_rows`: what
/// `lashctl finalize` runs as its last step. Each returned step is the
/// backfill's final ledger row.
pub(crate) async fn run_backfills(
    pool: &PgPool,
    fence: &WriterFence,
    batch_rows: i64,
) -> Result<Vec<MigrationStep>, MigrateError> {
    let mut executed = Vec::new();
    for backfill in BACKFILL_MIGRATIONS {
        let recorded: Option<String> = sqlx::query_scalar(
            "SELECT state FROM lash_migrations WHERE phase = 'backfill' AND migration = $1",
        )
        .bind(backfill.id)
        .fetch_optional(pool)
        .await
        .map_err(store_sqlx_error)?;
        if recorded.as_deref() == Some("applied") {
            continue;
        }
        start_backfill(pool, fence, backfill).await?;
        while let BatchOutcome::Progressed =
            backfill_batch(pool, fence, backfill, batch_rows).await?
        {}
        executed.push(ledger_row(pool, MigrationPhase::Backfill, backfill.id).await?);
    }
    Ok(executed)
}

/// `lashctl migrate --phase backfill`: every pending backfill, resumed from
/// its cursor and run to completion.
async fn backfill_on(
    pool: &PgPool,
    fence: &WriterFence,
    batch_rows: i64,
) -> Result<MigrationReport, MigrateError> {
    let state = {
        let mut connection = lock_connection(pool, true).await?;
        let state = read_state_under_lock(&mut connection).await;
        let _ = sqlx::Connection::close(connection).await;
        state?
    };
    if state.installation.is_none() {
        return Err(MigrationRefusal::Unprovisioned {
            phase: MigrationPhase::Backfill.name().to_owned(),
        }
        .into());
    }
    let executed = run_backfills(pool, fence, batch_rows).await?;
    Ok(report(&state, executed))
}

/// `lashctl migrate --phase contract`: every pending contract step, each in
/// one fenced transaction under the exclusive schema lock, refused typed
/// until `F` has reached its epoch and the ledger shows its backfills
/// applied. The step raises the component's reader floor in the same commit.
async fn contract_on(pool: &PgPool, fence: &WriterFence) -> Result<MigrationReport, MigrateError> {
    let mut connection = lock_connection(pool, false).await?;
    let result = async {
        let state = read_state_under_lock(&mut connection).await?;
        if state.installation.is_none() {
            return Err(MigrationRefusal::Unprovisioned {
                phase: MigrationPhase::Contract.name().to_owned(),
            }
            .into());
        }
        let mut executed = Vec::new();
        for contract in CONTRACT_MIGRATIONS {
            if state.is_applied(MigrationPhase::Contract, contract.id) {
                continue;
            }
            executed.push(apply_contract(&mut connection, fence, contract).await?);
        }
        let result = report(&state, executed);
        if !result.executed.is_empty() {
            verify_changed_catalog(&mut connection, result.executed.len()).await?;
        }
        Ok(result)
    }
    .await;
    let _ = sqlx::Connection::close(connection).await;
    result
}

/// One contract step, gated inside its own fenced transaction: the `F` the
/// fence read and the ledger rows it locks are the ones the step commits
/// against.
async fn apply_contract(
    connection: &mut sqlx::PgConnection,
    fence: &WriterFence,
    contract: &ContractMigration,
) -> Result<MigrationStep, MigrateError> {
    let mut tx = crate::guarded_tx::begin_migration(connection, fence).await?;
    let applied: Vec<String> = sqlx::query_scalar(
        "SELECT migration FROM lash_migrations
         WHERE phase = 'backfill' AND state = 'applied' AND migration = ANY($1)
         FOR SHARE",
    )
    .bind(
        contract
            .after_backfills
            .iter()
            .map(|backfill| (*backfill).to_owned())
            .collect::<Vec<_>>(),
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    contract_admitted(contract, tx.fleet().version(), |backfill| {
        applied.iter().any(|name| name == backfill)
    })?;
    let started_at_ms = server_clock_ms(&mut tx).await?;
    sqlx::raw_sql(contract.statements)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(
        "UPDATE lash_schema_versions SET min_reader = GREATEST(min_reader, $1)
         WHERE component = $2",
    )
    .bind(contract.min_reader)
    .bind(SCHEMA_COMPONENT)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let (release, state, started_at_ms, finished_at_ms) = record_step(
        &mut tx,
        "lash_migrations",
        MigrationPhase::Contract.name(),
        contract.id,
        Some(SCHEMA_VERSION),
        SCHEMA_VERSION,
        started_at_ms,
    )
    .await?;
    crate::release_stamp::write(&mut tx)
        .await
        .map_err(store_sqlx_error)?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(MigrationStep {
        phase: MigrationPhase::Contract.name().to_string(),
        migration: contract.id.to_string(),
        release,
        state,
        from_version: Some(SCHEMA_VERSION),
        to_version: SCHEMA_VERSION,
        started_at_ms: Some(started_at_ms),
        finished_at_ms,
        backfill_cursor: None,
        backfill_rows: None,
    })
}

/// Runs `phase` for `database_url` with default pool settings — what
/// `lashctl migrate` invokes.
pub async fn migrate(
    database_url: &str,
    phase: MigrationPhase,
) -> Result<MigrationReport, MigrateError> {
    let pool = migrate_pool(database_url).await?;
    let result = migrate_on(&pool, phase).await;
    pool.close().await;
    result
}

/// Plans what [`migrate`] would apply without changing the database.
pub async fn plan_migrations(
    database_url: &str,
    phase: MigrationPhase,
) -> Result<MigrationReport, MigrateError> {
    let pool = migrate_pool(database_url).await?;
    let result = plan_on(&pool, phase).await;
    pool.close().await;
    result
}

/// The migrate runner needs two connections at most — one holds the advisory
/// lock while it works — and the usual statement timeouts, not a worker's
/// pool shape.
async fn migrate_pool(database_url: &str) -> Result<PgPool, StoreError> {
    postgres_pool_options(&PostgresStoreConfig {
        max_connections: 2,
        ..PostgresStoreConfig::default()
    })
    .connect(database_url)
    .await
    .map_err(store_sqlx_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The upgrade path and the bootstrap provision identical objects: a
    /// catalog step that creates an object states it exactly the way
    /// `schema.sql` does, so the two paths can never disagree about that
    /// object's shape. A step that alters an existing table carries no byte
    /// contract with the artifact — `schema.sql` folds the added column into
    /// its `CREATE TABLE` body — so the structural suites own that
    /// equivalence.
    #[test]
    fn created_objects_match_the_schema_artifact() {
        for migration in EXPAND_MIGRATIONS {
            if migration.statements.starts_with("CREATE") {
                assert!(
                    SCHEMA_DDL.contains(migration.statements),
                    "schema.sql does not contain {}'s DDL verbatim",
                    migration.id
                );
            }
        }
    }

    /// The bootstrap and a host applying `schema.sql` seed the same `F` that
    /// [`seed_fleet_format`] seeds on an installed catalog: the migrating
    /// build's writable floor (ADR 0115 §2.1).
    #[test]
    fn the_schema_artifact_seeds_the_migrating_build_s_fleet_epoch() {
        let seed =
            lash_core_execution::FleetFormat::seed(lash_core_execution::FleetFormat::writable());
        let statement = format!(
            "INSERT INTO lash_fleet_format (singleton, format_version)\nVALUES (TRUE, {seed})\nON CONFLICT (singleton) DO NOTHING;"
        );
        assert!(
            SCHEMA_DDL.contains(&statement),
            "schema.sql must seed lash_fleet_format at F={seed}"
        );
    }

    /// A step that alters tables before it creates any still creates each
    /// object exactly as `schema.sql` states it: every `CREATE` statement of
    /// the logical-root step appears verbatim in the artifact.
    #[test]
    fn the_logical_root_step_creates_what_the_schema_artifact_states() {
        let created: Vec<&str> = LOGICAL_ROOT_FAMILY_DDL
            .split(";\n")
            .map(|statement| statement.trim_end_matches(';'))
            .filter(|statement| statement.starts_with("CREATE"))
            .collect();
        assert_eq!(created.len(), 4, "three tables and an index");
        for statement in created {
            assert!(
                SCHEMA_DDL.contains(statement),
                "schema.sql does not contain the logical-root step's DDL verbatim: {statement}"
            );
        }
    }

    /// The catalog must chain to the current component: a step targeting a
    /// version the build no longer stamps would leave planning stuck.
    #[test]
    fn the_expand_catalog_chains_to_the_current_component() {
        let mut at = SCHEMA_VERSION;
        while let Some(migration) = EXPAND_MIGRATIONS
            .iter()
            .find(|migration| migration.to_version == at)
        {
            assert_eq!(
                migration.from_version,
                at - 1,
                "{} does not chain from the previous component",
                migration.id
            );
            at = migration.from_version;
        }
    }

    #[test]
    fn every_expand_step_passes_the_previous_tolerant_check() {
        // The catalog below predates the 1.0 compatibility stamp. It is the
        // pre-cut DDL chain, not a compatibility expand from version 1.
        // There are no post-cut expand steps yet. When one is registered, this
        // test must apply it to the previous catalog and call the tolerant
        // checker before admitting the step.
        assert!(
            EXPAND_MIGRATIONS
                .iter()
                .all(|step| step.from_version < SCHEMA_VERSION),
            "a post-cut expand needs a previous-catalog tolerant check"
        );
    }

    #[test]
    fn phases_round_trip_through_their_names() {
        for phase in [
            MigrationPhase::Expand,
            MigrationPhase::Backfill,
            MigrationPhase::Contract,
        ] {
            assert_eq!(MigrationPhase::parse(phase.name()), Some(phase));
        }
        assert_eq!(MigrationPhase::parse("sideways"), None);
    }

    /// Every backfill a contract step waits for is one this build carries,
    /// and every step's epoch is one a finalize can reach: a contract that
    /// named a backfill no build runs would be refused forever.
    #[test]
    fn contract_steps_wait_only_for_backfills_this_build_carries() {
        for contract in CONTRACT_MIGRATIONS {
            for backfill in contract.after_backfills {
                let carried = BACKFILL_MIGRATIONS
                    .iter()
                    .find(|carried| carried.id == *backfill)
                    .unwrap_or_else(|| {
                        panic!("{} waits for unknown backfill {backfill}", contract.id)
                    });
                assert!(
                    carried.after_fleet <= contract.after_fleet,
                    "{} could contract before {backfill} may run",
                    contract.id
                );
            }
            assert!(contract.min_reader >= 1, "{}", contract.id);
        }
        // `F` is 1 at the cut and moves at every compatibility release's
        // finalize, so a backfill released at 1 would run before any.
        for backfill in BACKFILL_MIGRATIONS {
            assert!(
                backfill.after_fleet > 1,
                "{} would run before any finalize",
                backfill.id
            );
        }
    }

    /// A contract step is refused until `F` reaches its epoch, then until
    /// every backfill it names is applied, and each refusal names its remedy.
    #[test]
    fn a_contract_gate_refuses_before_finalize_and_before_its_backfills() {
        let contract = ContractMigration {
            id: "gate-contract",
            after_fleet: 2,
            after_backfills: &["gate-backfill"],
            statements: "",
            min_reader: 2,
        };
        let before_finalize = contract_admitted(&contract, 1, |_| true).unwrap_err();
        assert_eq!(
            before_finalize,
            MigrationRefusal::ContractBeforeFinalize {
                migration: "gate-contract".to_owned(),
                recorded: 1,
                requires: 2,
            }
        );
        assert!(before_finalize.to_string().contains("lashctl finalize"));
        let before_backfills = contract_admitted(&contract, 2, |_| false).unwrap_err();
        assert_eq!(
            before_backfills,
            MigrationRefusal::ContractBeforeBackfills {
                migration: "gate-contract".to_owned(),
                pending: vec!["gate-backfill".to_owned()],
            }
        );
        assert!(
            before_backfills
                .to_string()
                .contains("lashctl migrate --phase backfill")
        );
        contract_admitted(&contract, 2, |backfill| backfill == "gate-backfill")
            .expect("finalized and backfilled");
    }

    #[test]
    fn migration_refusals_serialize_tagged() {
        assert_eq!(
            serde_json::to_value(MigrationRefusal::BackfillBeforeFinalize {
                migration: "b".to_owned(),
                recorded: 1,
                requires: 2,
            })
            .expect("serialize"),
            serde_json::json!({"refusal":"backfill_before_finalize","migration":"b","recorded":1,"requires":2})
        );
    }
}
