//! The one-call owner commit on PostgreSQL (FIG-5936).
//!
//! An owner commit whose every domain write is a step program
//! ([`super::steps`]) is one server call: `lash_owner_commit`, a PL/pgSQL
//! function generated from this build's statements by [`function_ddl`] and
//! declared in `schema.sql`. It runs the commit's envelope (the graph gate
//! and the writer fence shared, the owner fence and its turn-cancel read, the
//! database clock), installs the commit's graph, runs each step's statement
//! as static SQL dispatched by the statement's name, then the formats stamp,
//! the acknowledgement and the release, in one autocommit transaction. Every
//! statement keeps a cached plan: none is text the function executes.
//!
//! The function decides nothing a domain owns. A step that answers another
//! number of rows than it must, an envelope that does not admit the commit
//! or a graph the store does not accept raises, which rolls the call back,
//! and the commit runs again in the explicit-transaction form, whose
//! classifiers name the refusal. A commit with a write that is not a step
//! program takes that form from the start.
//!
//! A lost answer is reconciled by the call's token: the function records
//! its answer under the token as its last write, and the reconciler inserts
//! the same token. A call that committed keeps its row, and its answer is
//! read back; a call still running holds the token's key, and the insert
//! waits for its end; a call that has not reached its insert, or never ran,
//! meets the reconciler's row and rolls back. So the reconciler's verdict is
//! final: a commit is never applied twice.

use std::sync::LazyLock;

#[cfg(test)]
use lash_durable::domain::CANCEL_MAIL;
use lash_durable::domain::DomainWrite;
use lash_durable::{
    ActorCommit, ActorTx, CommitLabel, DurableError, Epoch, Release, StoreFailure, StoreFailureKind,
};
use lash_store_sql::Dialect;

use super::steps::{Entry, Step};
#[cfg(test)]
use super::steps::{Shape, Ty};
use super::{
    PostgresDurableStore, SQL, get, graph, park, prompts, replay, run_records, session_mail,
    snapshots, sqlx_failure, turns,
};
use crate::connection_sql::connection_sql;
use crate::replayable::Attempt;

lash_store_sql::statements! {
    /// The statements of the one-call owner commit's own mechanics.
    pub(crate) struct ExecutorStatements @ "durable_executor" {
        /// How many of the named blobs `?1` are not stored, or are among
        /// the roots `?2` and not stored as graph nodes.
        external_absent = "SELECT count(*) FROM unnest(?1::text[]) AS named(hash)
             LEFT JOIN blobs AS stored ON stored.hash = named.hash
             WHERE stored.hash IS NULL
                OR (named.hash = ANY(?2::text[]) AND stored.encoding <> 1)";

        /// How many staged nodes `?1` (raw digests) with bytes `?2` the
        /// install left out (not among `?3`) whose stored blob is not the
        /// same graph node.
        held_conflicts = "SELECT count(*)
             FROM unnest(?1::bytea[], ?2::bytea[]) AS staged(digest, content)
             LEFT JOIN blobs AS stored ON stored.hash = encode(staged.digest, 'hex')
             WHERE NOT (encode(staged.digest, 'hex') = ANY(?3::text[]))
               AND (stored.hash IS NULL OR stored.encoding <> 1
                    OR stored.content <> staged.content)";

        /// The bytes of the staged nodes `?1` (raw digests) with bytes `?2`
        /// the install inserted (among `?3`).
        installed_bytes = "SELECT COALESCE(sum(octet_length(staged.content)), 0)::bigint
             FROM unnest(?1::bytea[], ?2::bytea[]) AS staged(digest, content)
             WHERE encode(staged.digest, 'hex') = ANY(?3::text[])";

        /// Record the answer `?3` of call `?1` at `?2`: the call's last
        /// write.
        latch_record = "INSERT INTO owner_commit_latches (token, created_ms, answer)
             VALUES (?1, ?2, ?3)";

        /// Forget a few answers recorded before `?1`, skipping any another
        /// sweep holds: no commit waits on another's sweep.
        latch_sweep = "DELETE FROM owner_commit_latches WHERE token IN (
                 SELECT token FROM owner_commit_latches WHERE created_ms < ?1
                 ORDER BY created_ms
                 LIMIT 16
                 FOR UPDATE SKIP LOCKED
             )";

        /// Take call `?1`'s token unless the call recorded it: a row when
        /// the call never commits.
        latch_preempt = "INSERT INTO owner_commit_latches (token, created_ms, answer)
             VALUES (?1, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT, NULL)
             ON CONFLICT (token) DO NOTHING
             RETURNING token";

        /// The answer call `?1` recorded.
        latch_answer = "SELECT answer FROM owner_commit_latches WHERE token = ?1";
    }
}

pub(crate) static EXECUTOR_SQL: LazyLock<ExecutorStatements> =
    LazyLock::new(|| ExecutorStatements::render(Dialect::postgres()));

#[cfg(test)]
/// How long a recorded answer is kept: far past the reconciliation of a
/// lost call, which reads it within its operation's retries.
const LATCH_HORIZON_MS: i64 = 900_000;

/// How many prompt texts a storage remembers it holds.
const HELD_TEXTS: usize = 4096;

/// The prompt texts a storage's commits recorded, most recent last, which
/// a one-call commit sends by address alone. A hint, never a fact: a text
/// reclaimed since refuses the call, and the commit runs in the explicit
/// form, which reads what the store holds.
#[derive(Debug, Default)]
pub(crate) struct HeldTexts {
    known: std::sync::Mutex<(
        std::collections::HashSet<String>,
        std::collections::VecDeque<String>,
    )>,
}

impl HeldTexts {
    pub(super) fn contains(&self, hash: &str) -> bool {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .contains(hash)
    }

    /// Remember the texts `tx` recorded, once it committed.
    pub(super) fn note(&self, tx: &ActorTx) {
        let mut known = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (set, order) = &mut *known;
        for write in tx.domain() {
            let DomainWrite::Prompt(lash_durable::domain::PromptWrite::Record {
                snapshot_texts,
                request_texts,
                ..
            }) = write
            else {
                continue;
            };
            for text in snapshot_texts.iter().chain(request_texts) {
                if set.insert(text.hash.clone()) {
                    order.push_back(text.hash.clone());
                }
            }
        }
        while order.len() > HELD_TEXTS {
            if let Some(oldest) = order.pop_front() {
                set.remove(&oldest);
            }
        }
    }
}

/// The function's name.
pub(crate) const FUNCTION: &str = "lash_owner_commit";

/// Every statement a step may name: what the function dispatches on.
pub(super) fn entries() -> [&'static Entry; 25] {
    [
        &run_records::APPEND,
        &run_records::PRUNE,
        &snapshots::PUT_FIRST,
        &snapshots::PUT_NEXT,
        &snapshots::RETIRE,
        &snapshots::CLEAR,
        &turns::ADMIT_RUN,
        &turns::INSERT_PHASE,
        &turns::ADVANCE_PHASE,
        &turns::TRACE_EXPORTED,
        &turns::OPEN_NAMED_RUN,
        &turns::UPSERT_NAMESPACES,
        &turns::WORKING_SET,
        &session_mail::BIND_INPUTS,
        &session_mail::BIND_BATCHES,
        &prompts::INSERT_SNAPSHOT,
        &prompts::LOCK_TEXTS,
        &prompts::UNSENT_ABSENT,
        &prompts::INSERT_SENT_TEXTS,
        &prompts::INSERT_EDGES,
        &graph::ADMISSION_PUT,
        &graph::ADMISSION_REMOVE,
        &run_records::DELETE_OWNER,
        &graph::CLEAR_ADMISSIONS,
        &park::SET_PARK,
    ]
}

/// `write`'s step program at `epoch`: `None` when it has none, and its
/// commit takes the explicit-transaction form.
pub(super) fn program(
    actor: &lash_durable::ActorKey,
    epoch: Epoch,
    write: &DomainWrite,
    held: &HeldTexts,
) -> Result<Option<Vec<Step>>, DurableError> {
    match write {
        DomainWrite::Turn(write) => turns::steps(epoch, write),
        DomainWrite::RunFact(write) => run_records::steps(epoch, write),
        DomainWrite::ExecRoot(write) => snapshots::steps(epoch, write),
        DomainWrite::EffectAdmission(write) => graph::admission_steps(epoch, write).map(Some),
        DomainWrite::SessionMail(write) => Ok(session_mail::steps(write)),
        DomainWrite::Prompt(write) => Ok(prompts::steps(epoch, write, held)),
        DomainWrite::ActorPark { reason_json } => {
            Ok(Some(vec![park::step(actor.as_str(), reason_json)]))
        }
        DomainWrite::SessionCommit(_)
        | DomainWrite::ReleasePin(_)
        | DomainWrite::Wait(_)
        | DomainWrite::Process(_)
        | DomainWrite::SessionClose(_) => Ok(None),
    }
}

/// The digest of every statement the function runs, which a call names:
/// a function generated from other statements refuses it.
pub(crate) static REGISTRY: LazyLock<String> = LazyLock::new(|| {
    // FNV-1a over each name and text: a digest, not a defense.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |text: &str| {
        for byte in text.bytes().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for text in mechanics() {
        feed(text);
    }
    for entry in entries() {
        feed(entry.name());
        feed(entry.statement().sql());
    }
    format!("{hash:016x}")
});

/// The mechanics' statement texts, in the order the function runs them.
fn mechanics() -> Vec<&'static str> {
    let executor = &*EXECUTOR_SQL;
    vec![
        connection_sql().lock_xact_graph_gate_shared.sql(),
        connection_sql().lock_xact_fleet_fence_shared.sql(),
        SQL.postgres.owner_envelope.sql(),
        graph::lock_nodes().sql(),
        executor.external_absent.sql(),
        graph::insert_nodes().sql(),
        executor.held_conflicts.sql(),
        graph::insert_edges().sql(),
        executor.installed_bytes.sql(),
        SQL.actor.stamp_formats.sql(),
        SQL.postgres.ack_through.sql(),
        SQL.postgres.park_unless_pending.sql(),
        SQL.mail.delete_all.sql(),
        SQL.actor.end.sql(),
        SQL.actor.release.sql(),
        executor.latch_record.sql(),
        executor.latch_sweep.sql(),
    ]
}

#[cfg(test)]
/// `sql` with each placeholder `$N` replaced by `args[N - 1]`, outside
/// string literals and quoted identifiers.
fn inline(sql: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            out.push(c);
            if c == open {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '$' if chars.peek().is_some_and(char::is_ascii_digit) => {
                let mut digits = String::new();
                while let Some(digit) = chars.peek().copied().filter(char::is_ascii_digit) {
                    digits.push(digit);
                    chars.next();
                }
                let index: usize = digits.parse().unwrap_or(0);
                match index.checked_sub(1).and_then(|index| args.get(index)) {
                    Some(arg) => {
                        out.push('(');
                        out.push_str(arg);
                        out.push(')');
                    }
                    None => panic!("statement placeholder ${index} has no argument: {sql}"),
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
/// What the function reads parameter `index` of a step as, typed `ty`. An
/// array keeps its elements' order, which parallel arrays pair by.
fn param(index: usize, ty: Ty) -> String {
    let array = |cast: &str| {
        format!(
            "ARRAY(SELECT element{cast} FROM jsonb_array_elements_text(v_p->{index}) \
             WITH ORDINALITY AS listed(element, position) ORDER BY position)"
        )
    };
    match ty {
        Ty::Text => format!("v_p->>{index}"),
        Ty::Int => format!("(v_p->>{index})::BIGINT"),
        Ty::Bytes => format!("decode(v_p->>{index}, 'hex')"),
        Ty::Texts => array(""),
        Ty::Bools => array("::BOOLEAN"),
    }
}

#[cfg(test)]
/// One step's branch of the dispatch: its statement, with its rows counted.
fn branch(entry: &Entry) -> String {
    let args: Vec<String> = entry
        .params
        .iter()
        .enumerate()
        .map(|(index, ty)| param(index, *ty))
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let sql = inline(entry.statement().sql(), &args);
    let run = match entry.shape {
        Shape::Exec => format!("{sql};\n            GET DIAGNOSTICS v_rows = ROW_COUNT;"),
        Shape::Returning => {
            assert!(
                !sql.trim_start().to_ascii_uppercase().starts_with("WITH"),
                "step {} has its own WITH: it cannot be counted in one",
                entry.name()
            );
            format!(
                "WITH answered AS ({sql})\n            SELECT count(*) INTO v_rows FROM answered;"
            )
        }
        Shape::Select => {
            format!("SELECT count(*) INTO v_rows FROM ({sql}) AS answered;")
        }
    };
    format!("        WHEN '{}' THEN\n            {run}\n", entry.name())
}

#[cfg(test)]
/// `sql`, a query, run for its effect.
fn perform(sql: &str) -> String {
    let sql = sql.trim_start();
    match sql.get(..6) {
        Some(select) if select.eq_ignore_ascii_case("SELECT") => format!("PERFORM{}", &sql[6..]),
        _ => panic!("not a query: {sql}"),
    }
}

#[cfg(test)]
/// The function's DDL, as `schema.sql` declares it.
pub(crate) fn function_ddl() -> String {
    let executor = &*EXECUTOR_SQL;
    let envelope = inline(
        SQL.postgres.owner_envelope.sql(),
        &["p_actor", "p_epoch", "p_session"],
    );
    let lock_nodes = perform(&inline(
        graph::lock_nodes().sql(),
        &["p_named", "p_digests"],
    ));
    let external_absent = inline(executor.external_absent.sql(), &["p_named", "p_roots"]);
    let insert_nodes = inline(graph::insert_nodes().sql(), &["p_digests", "p_contents"]);
    let held_conflicts = inline(
        executor.held_conflicts.sql(),
        &["p_digests", "p_contents", "v_inserted"],
    );
    let insert_edges = inline(graph::insert_edges().sql(), &["v_parents", "v_children"]);
    let installed_bytes = inline(
        executor.installed_bytes.sql(),
        &["p_digests", "p_contents", "v_inserted"],
    );
    let stamp_formats = inline(SQL.actor.stamp_formats.sql(), &["p_actor", "p_formats"]);
    let ack_through = inline(SQL.postgres.ack_through.sql(), &["p_actor", "p_ack"]);
    let park = inline(
        SQL.postgres.park_unless_pending.sql(),
        &[
            "p_actor",
            "v_now",
            &format!("'{}'", CANCEL_MAIL.replace('\'', "''")),
        ],
    );
    let delete_mail = inline(SQL.mail.delete_all.sql(), &["p_actor"]);
    let end = inline(SQL.actor.end.sql(), &["p_actor"]);
    let release = inline(
        SQL.actor.release.sql(),
        &["p_actor", "p_release", "p_due", "v_now"],
    );
    let latch_record = inline(
        executor.latch_record.sql(),
        &[
            "p_token",
            "v_clock",
            "jsonb_build_object('revision', v_rev, 'state', v_state, 'cancel', \
             jsonb_build_array(v_cancel_turn, v_cancel_request, v_cancel_origin, \
             v_cancel_reason, v_cancel_disposition, v_cancel_mode))",
        ],
    );
    let latch_sweep = inline(
        executor.latch_sweep.sql(),
        &[&format!("v_clock - {LATCH_HORIZON_MS}")],
    );
    let branches: String = entries().iter().map(|entry| branch(entry)).collect();
    let registry = &*REGISTRY;
    let gate = perform(connection_sql().lock_xact_graph_gate_shared.sql());
    let fleet_fence = perform(connection_sql().lock_xact_fleet_fence_shared.sql());
    format!(
        "CREATE OR REPLACE FUNCTION {FUNCTION}(
    p_registry TEXT, p_actor TEXT, p_epoch BIGINT, p_session TEXT,
    p_fleet_low INTEGER, p_fleet_high INTEGER, p_token TEXT,
    p_lock_timeout TEXT, p_statement_ms BIGINT,
    p_named TEXT[], p_roots TEXT[], p_digests BYTEA[], p_contents BYTEA[],
    p_parents BYTEA[], p_children BYTEA[], p_steps JSONB,
    p_formats TEXT, p_ack BIGINT, p_release TEXT, p_due BIGINT, p_now BIGINT)
RETURNS TABLE (o_revision BIGINT, o_now BIGINT, o_fleet INTEGER, o_state TEXT,
    o_installed BIGINT, o_cancel_turn TEXT, o_cancel_request TEXT, o_cancel_origin TEXT,
    o_cancel_reason TEXT, o_cancel_disposition TEXT, o_cancel_mode TEXT)
LANGUAGE plpgsql
AS $lash_owner_commit$
#variable_conflict use_column
DECLARE
    v_fleet INTEGER;
    v_now BIGINT;
    v_xact TEXT;
    v_rev BIGINT;
    v_cancel_turn TEXT;
    v_cancel_request TEXT;
    v_cancel_origin TEXT;
    v_cancel_reason TEXT;
    v_cancel_disposition TEXT;
    v_cancel_mode TEXT;
    v_clock BIGINT;
    v_inserted TEXT[] := '{{}}';
    v_parents BYTEA[];
    v_children BYTEA[];
    v_installed BIGINT := 0;
    v_rows BIGINT;
    v_index INTEGER := 0;
    v_step JSONB;
    v_p JSONB;
    v_state TEXT := 'owned';
BEGIN
    -- Generated from this build's statements (registry {registry}); a call
    -- built from others is refused, as is any isolation but read committed.
    IF p_registry IS DISTINCT FROM '{registry}'
        OR current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'lash_owner_commit: registry or isolation refused' USING ERRCODE = 'LX000';
    END IF;
    IF p_lock_timeout IS NOT NULL THEN
        PERFORM set_config('lock_timeout', p_lock_timeout, true);
    END IF;
    {gate};
    {fleet_fence};
    {envelope}
    INTO v_fleet, v_clock, v_xact, v_rev, v_cancel_turn, v_cancel_request, v_cancel_origin,
        v_cancel_reason, v_cancel_disposition, v_cancel_mode;
    v_now := COALESCE(p_now, v_clock);
    IF v_fleet IS NULL OR v_fleet < p_fleet_low OR v_fleet > p_fleet_high THEN
        RAISE EXCEPTION 'lash_owner_commit: the writer fence refused' USING ERRCODE = 'LX002';
    END IF;
    IF v_rev IS NULL THEN
        RAISE EXCEPTION 'lash_owner_commit: the owner fence refused' USING ERRCODE = 'LX003';
    END IF;
    IF cardinality(p_named) > 0 THEN
        {lock_nodes};
        {external_absent} INTO v_rows;
        IF v_rows > 0 THEN
            RAISE EXCEPTION 'lash_owner_commit: the graph refused' USING ERRCODE = 'LX004';
        END IF;
    END IF;
    IF cardinality(p_digests) > 0 THEN
        WITH inserted AS ({insert_nodes})
        SELECT COALESCE(array_agg(hash), '{{}}') INTO v_inserted FROM inserted;
        {held_conflicts} INTO v_rows;
        IF v_rows > 0 THEN
            RAISE EXCEPTION 'lash_owner_commit: the graph refused' USING ERRCODE = 'LX004';
        END IF;
        SELECT COALESCE(array_agg(staged.parent), '{{}}'), COALESCE(array_agg(staged.child), '{{}}')
        INTO v_parents, v_children
        FROM unnest(p_parents, p_children) AS staged(parent, child)
        WHERE encode(staged.parent, 'hex') = ANY(v_inserted);
        IF cardinality(v_parents) > 0 THEN
            {insert_edges};
        END IF;
        {installed_bytes} INTO v_installed;
    END IF;
    FOR v_step IN SELECT value FROM jsonb_array_elements(p_steps) LOOP
        v_p := v_step->'p';
        CASE v_step->>'s'
{branches}        ELSE
            RAISE EXCEPTION 'lash_owner_commit: no step %', v_step->>'s' USING ERRCODE = 'LX000';
        END CASE;
        IF v_step ? 'n' AND v_rows <> (v_step->>'n')::BIGINT THEN
            RAISE EXCEPTION 'lash_owner_commit: step % refused', v_index USING ERRCODE = 'LX001';
        END IF;
        IF p_statement_ms IS NOT NULL
            AND clock_timestamp() - statement_timestamp() > p_statement_ms * INTERVAL '1 millisecond' THEN
            RAISE EXCEPTION 'canceling statement due to statement timeout' USING ERRCODE = '57014';
        END IF;
        v_index := v_index + 1;
    END LOOP;
    IF p_formats IS NOT NULL THEN
        {stamp_formats};
    END IF;
    IF p_ack IS NOT NULL THEN
        {ack_through};
    END IF;
    CASE p_release
    WHEN 'parked' THEN
        {park}
        INTO v_state;
    WHEN 'terminal' THEN
        {delete_mail};
        {end}
        INTO v_state;
    WHEN 'idle', 'waiting', 'ready' THEN
        {release}
        INTO v_state;
    ELSE
        NULL;
    END CASE;
    {latch_record};
    {latch_sweep};
    RETURN QUERY SELECT v_rev, v_now, v_fleet, v_state, v_installed, v_cancel_turn,
        v_cancel_request, v_cancel_origin, v_cancel_reason, v_cancel_disposition, v_cancel_mode;
END
$lash_owner_commit$;
"
    ) + &crate::artifact_store::local_end_function_ddl()
}

/// The call's arguments for `tx`, unless the commit takes the
/// explicit-transaction form: a write that is not a step program, or a
/// graph or budget the explicit form refuses.
pub(super) struct Call {
    pub(super) steps: serde_json::Value,
    pub(super) named: Vec<String>,
    pub(super) roots: Vec<String>,
    pub(super) digests: Vec<Vec<u8>>,
    pub(super) contents: Vec<Vec<u8>>,
    pub(super) parents: Vec<Vec<u8>>,
    pub(super) children: Vec<Vec<u8>>,
    pub(super) release: Option<&'static str>,
    pub(super) due: Option<i64>,
}

impl Call {
    pub(super) fn of(tx: &ActorTx, held: &HeldTexts) -> Option<Self> {
        let mut steps = Vec::new();
        for write in tx.domain() {
            for step in program(tx.actor(), tx.epoch(), write, held).ok()?? {
                let mut json = serde_json::json!({
                    "s": step.entry.name(),
                    "p": step.values.iter().map(super::steps::Value::json).collect::<Vec<_>>(),
                });
                if let Some(expect) = step.expect {
                    json["n"] = expect.into();
                }
                steps.push(json);
            }
        }
        let publications: Vec<_> = tx
            .domain()
            .iter()
            .flat_map(DomainWrite::publications)
            .collect();
        let mut call = Self {
            steps: serde_json::Value::Array(steps),
            named: Vec::new(),
            roots: Vec::new(),
            digests: Vec::new(),
            contents: Vec::new(),
            parents: Vec::new(),
            children: Vec::new(),
            release: None,
            due: None,
        };
        if !tx.graph().is_empty() || !tx.reads().is_empty() || !publications.is_empty() {
            let checked = lash_durable::domain::graph_commit::check_graph(
                tx.graph(),
                tx.reads(),
                &publications,
            )
            .ok()?;
            if let Some(budget) = tx.budget() {
                lash_durable::domain::graph_commit::measure_owner_commit(tx.graph(), tx.domain())
                    .and_then(|measured| budget.admit(&measured))
                    .ok()?;
            }
            let named: std::collections::BTreeSet<_> = checked
                .external
                .iter()
                .chain(checked.external_roots.iter())
                .collect();
            call.named = named.iter().map(|node| node.to_hex()).collect();
            call.roots = checked
                .external_roots
                .iter()
                .map(|node| node.to_hex())
                .collect();
            for node in &checked.nodes {
                for child in &node.children {
                    call.parents.push(node.id.0.to_vec());
                    call.children.push(child.0.to_vec());
                }
                call.digests.push(node.id.0.to_vec());
                call.contents.push(node.bytes.to_vec());
            }
        }
        (call.release, call.due) = match tx.release() {
            None => (None, None),
            Some(Release::Parked) => (Some("parked"), None),
            Some(Release::Terminal) => (Some("terminal"), None),
            Some(Release::Waiting { next_due }) => (Some("waiting"), next_due.map(|due| due.0)),
            Some(Release::Ready) => (Some("ready"), None),
            Some(_) => (Some("idle"), None),
        };
        Some(call)
    }
}

/// The call's SQL.
pub(super) static CALL: LazyLock<String> = LazyLock::new(|| {
    let params: Vec<String> = (1..=21).map(|index| format!("${index}")).collect();
    format!("SELECT * FROM {FUNCTION}({})", params.join(", "))
});

/// Whether the function refused the call: a refusal the
/// explicit-transaction form names, a call built from other statements,
/// or a store without the function. The call rolled back.
fn declined(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database.code().is_some_and(|code| code.starts_with("LX") || code == "42883")
    )
}

/// Whether the server answered the call itself, so it rolled back: any
/// error but a broken connection, the session ending under it (`57P`), or a
/// server fault (`58`, `XX`).
fn answered(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database) = error else {
        return false;
    };
    database.code().is_some_and(|code| {
        !(code.starts_with("08")
            || code.starts_with("57P")
            || code.starts_with("58")
            || code.starts_with("XX"))
    })
}

impl PostgresDurableStore {
    /// Commit `tx` as one call: its receipt, or `None` when the function
    /// refused it and the explicit-transaction form must run it. A lost
    /// answer is reconciled by the call's token.
    pub(super) async fn call_owner(
        &self,
        label: CommitLabel,
        tx: &ActorTx,
        call: &Call,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<ActorCommit>, Attempt<DurableError>> {
        let route = self.route(label.capacity());
        let mut connection = crate::observed_sql::checkout(route.pool)
            .await
            .map_err(|error| replay::attempt(sqlx_failure(error)))?;
        crate::observed_sql::transaction_started();
        let token = uuid::Uuid::new_v4().to_string();
        let injected = self
            .injected_instant()
            .transpose()
            .map_err(Attempt::Failed)?;
        let writable = self.fence.writable();
        let bound = |version: u32| i32::try_from(version).unwrap_or(i32::MAX);
        let session = match tx.actor().kind() {
            lash_durable::ActorKind::Session => Some(tx.actor().id()),
            lash_durable::ActorKind::Process => None,
        };
        let (lock_timeout, statement_ms) = route.prelude.call_guards();
        let query = crate::observed_sql::query(&CALL)
            .bind(REGISTRY.as_str())
            .bind(tx.actor().as_str())
            .bind(tx.epoch().0)
            .bind(session)
            .bind(bound(writable.min()))
            .bind(bound(writable.max()))
            .bind(&token)
            .bind(lock_timeout)
            .bind(statement_ms)
            .bind(&call.named)
            .bind(&call.roots)
            .bind(&call.digests)
            .bind(&call.contents)
            .bind(&call.parents)
            .bind(&call.children)
            .bind(&call.steps)
            .bind(tx.formats().map(|formats| formats.as_str().to_owned()))
            .bind(tx.ack().map(|through| through.0))
            .bind(call.release)
            .bind(call.due)
            .bind(injected.map(|now| now.0));
        #[cfg(any(test, feature = "testing"))]
        let loses_answer = match self.commit_fault.as_deref() {
            Some(fault) => fault.take_call(&mut connection).await,
            None => false,
        };
        let sent = query
            .fetch_one(crate::observed_sql::executor(&mut *connection))
            .await;
        #[cfg(any(test, feature = "testing"))]
        let sent = match sent {
            Ok(_) if loses_answer => Err(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "injected: the answer of the owner commit was lost",
            ))),
            sent => sent,
        };
        let row = match sent {
            Ok(row) => row,
            Err(error) if declined(&error) => return Ok(None),
            Err(error) if answered(&error) => return Err(replay::attempt(sqlx_failure(error))),
            Err(lost) => {
                drop(connection);
                return self
                    .reconcile_call(label, tx, &token, lost, deadline)
                    .await
                    .map(Some);
            }
        };
        let fleet: i32 = get(&row, 2).map_err(Attempt::Failed)?;
        if let Ok(fleet) = u32::try_from(fleet) {
            self.fence.observe(fleet);
        }
        let installed: i64 = get(&row, 4).map_err(Attempt::Failed)?;
        if let Some(window) = lash_core_execution::facade_support::sql::Window::current() {
            window.installed_graph(u64::try_from(installed).unwrap_or_default());
        }
        let cancel: Vec<Option<String>> = (5..=10)
            .map(|index| get(&row, index))
            .collect::<Result<_, _>>()
            .map_err(Attempt::Failed)?;
        answer(
            tx.actor(),
            &serde_json::json!({
                "revision": get::<i64>(&row, 0).map_err(Attempt::Failed)?,
                "state": get::<String>(&row, 3).map_err(Attempt::Failed)?,
                "cancel": cancel,
            }),
        )
        .map(Some)
        .map_err(Attempt::Failed)
    }

    /// Learn whether call `token`, whose answer was `lost`, committed: take
    /// its token unless it recorded it, waiting out a call still running.
    /// Taken: the call never commits, and it runs again. Recorded: its
    /// answer.
    async fn reconcile_call(
        &self,
        label: CommitLabel,
        tx: &ActorTx,
        token: &str,
        lost: sqlx::Error,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<ActorCommit, Attempt<DurableError>> {
        let pool = self.route(label.capacity()).pool;
        let retry = &self.pools.durable_retry;
        let mut last = format!("the connection was lost at the owner commit: {lost}");
        for read in 0..retry.attempts {
            if read > 0 {
                let Some(pause) = retry.pause_within(read - 1, deadline) else {
                    break;
                };
                tokio::time::sleep(pause).await;
            }
            let settled = self.settle_call(pool, token);
            let settled = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, settled).await {
                    Ok(settled) => settled,
                    Err(_) => break,
                },
                None => settled.await,
            };
            match settled {
                Ok(None) => {
                    return Err(Attempt::Aborted(DurableError::Store(StoreFailure {
                        kind: StoreFailureKind::Unavailable,
                        message: format!(
                            "the connection was lost at the owner commit, which never committed: \
                             {lost}"
                        ),
                    })));
                }
                Ok(Some(recorded)) => {
                    return answer(tx.actor(), &recorded).map_err(Attempt::Failed);
                }
                Err(error) => last = format!("its outcome could not be read: {error}"),
            }
        }
        Err(Attempt::Failed(DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Unavailable,
            message: format!("whether owner commit {token} committed is unknown: {last}"),
        })))
    }

    /// Take call `token`'s key, or read the answer it recorded.
    async fn settle_call(
        &self,
        pool: &sqlx::PgPool,
        token: &str,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(fault) = self.commit_fault.as_deref()
            && let Some(lost) = fault.outcome_connection(pool).await
        {
            return settle_on(&mut *lost?, token).await;
        }
        let mut connection = crate::observed_sql::checkout(pool).await?;
        settle_on(&mut connection, token).await
    }
}

async fn settle_on(
    connection: &mut sqlx::PgConnection,
    token: &str,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let taken = crate::observed_sql::query(EXECUTOR_SQL.latch_preempt.sql())
        .bind(token)
        .fetch_optional(crate::observed_sql::executor(&mut *connection))
        .await?;
    if taken.is_some() {
        return Ok(None);
    }
    let recorded: Option<serde_json::Value> =
        crate::observed_sql::query_scalar(EXECUTOR_SQL.latch_answer.sql())
            .bind(token)
            .fetch_one(crate::observed_sql::executor(&mut *connection))
            .await?;
    Ok(Some(recorded.unwrap_or_default()))
}

/// Decode a recorded answer: the commit's receipt.
pub(super) fn answer(
    actor: &lash_durable::ActorKey,
    recorded: &serde_json::Value,
) -> Result<ActorCommit, DurableError> {
    let corrupt = || super::corrupt("owner commit answer", &recorded.to_string());
    let revision = recorded["revision"].as_i64().ok_or_else(corrupt)?;
    let state = super::actor_state(recorded["state"].as_str().ok_or_else(corrupt)?)?;
    let cancel = recorded["cancel"].as_array().ok_or_else(corrupt)?;
    let text = |index: usize| {
        cancel
            .get(index)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    let turn_cancel = match (actor.kind(), text(0)) {
        (lash_durable::ActorKind::Session, Some(run)) => {
            let session = lash_sansio::SessionId::try_from(actor.id().to_owned())
                .map_err(|_| super::corrupt("session id", actor.id()))?;
            let run = lash_sansio::TurnId::try_from(run.clone())
                .map_err(|_| super::corrupt("turn id", &run))?;
            Some(turns::decode_cancel(
                &session,
                &run,
                (
                    text(1).unwrap_or_default(),
                    text(2),
                    text(3),
                    text(4).unwrap_or_default(),
                    text(5).unwrap_or_default(),
                ),
            )?)
        }
        _ => None,
    };
    Ok(ActorCommit {
        revision: lash_durable::StateRevision(revision),
        state,
        turn_cancel,
    })
}

#[cfg(test)]
mod tests {
    // Test tooling: reading and regenerating the committed artifact is
    // sanctioned here.
    #![allow(clippy::disallowed_methods)]

    const BEGIN: &str = "-- BEGIN GENERATED lash_owner_commit\n";
    const END: &str = "-- END GENERATED lash_owner_commit\n";

    /// `schema` with its generated block replaced by `block`.
    fn with_block(schema: &str, block: &str) -> String {
        let start = schema
            .find(BEGIN)
            .expect("schema.sql marks the generated function")
            + BEGIN.len();
        let end = schema
            .find(END)
            .expect("schema.sql ends the generated function");
        format!("{}{block}{}", &schema[..start], &schema[end..])
    }

    /// The function `schema.sql` declares is the one this build's statements
    /// generate: a statement edited without regenerating it fails here.
    #[test]
    fn the_declared_owner_commit_is_generated_from_this_builds_statements() {
        let schema = crate::schema::SCHEMA_DDL;
        assert!(
            schema == with_block(schema, &super::function_ddl()),
            "schema.sql's lash_owner_commit is stale: regenerate it with \
             LASH_REGENERATE=1 and the ignored regenerate_owner_commit test"
        );
    }

    #[test]
    #[ignore = "regenerates lash_owner_commit in crates/lash-postgres-store/schema.sql"]
    fn regenerate_owner_commit() {
        assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
        let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").expect("regeneration workspace");
        let path = std::path::PathBuf::from(root).join("crates/lash-postgres-store/schema.sql");
        let schema = std::fs::read_to_string(&path).expect("read schema.sql");
        std::fs::write(&path, with_block(&schema, &super::function_ddl()))
            .expect("rewrite schema.sql");
    }
}
