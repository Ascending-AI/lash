//! The generator of `lash_owner_commit` (FIG-5936, FIG-5942): the one-call
//! owner commit's PL/pgSQL, written from this build's statements and
//! declared in `schema.sql`. Every statement it runs is static SQL; it
//! executes no text.
//!
//! The function runs the commit's envelope, puts the checkpoint bodies a
//! head commit supplies, installs the commit's graph, then runs each step:
//! its statement, dispatched by name, with what the step's rows name for
//! the commit's mechanics gathered as they are answered. A step may be
//! skipped by what an earlier step captured, and may capture its own first
//! row. After a step, the ancestry it retired is walked and the actors it
//! woke are woken. Then the commit's retired roots are held, and the
//! formats stamp, the acknowledgement and the release end it; a `ready`
//! release that writes nothing else keeps the claim's no-progress mark.

use super::executor::{EXECUTOR_SQL, FUNCTION, LATCH_HORIZON_MS, REGISTRY, entries};
use super::prompts::HELD_TEXTS_FUNCTION;
use super::renewal::FUNCTION as RENEW_FUNCTION;
use super::steps::{Collect, END_LOCAL_PARAMS, Entry, Shape, Ty};
use super::{SQL, graph, pages, prompts};
use crate::connection_sql::connection_sql;
use crate::session_sql::session_sql;

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

/// What the function reads parameter `index` of a step as, typed `ty`. An
/// array keeps its elements' order, which parallel arrays pair by.
fn param(index: usize, ty: Ty) -> String {
    let array = |element: &str| {
        format!(
            "ARRAY(SELECT {element} FROM jsonb_array_elements_text(v_p->{index}) \
             WITH ORDINALITY AS listed(element, position) ORDER BY position)"
        )
    };
    match ty {
        Ty::Key => format!("decode(v_p->>{index}, 'hex')"),
        Ty::Keys => array("decode(element, 'hex')"),
        Ty::Text => format!("v_p->>{index}"),
        Ty::Int => format!("(v_p->>{index})::BIGINT"),
        Ty::Bool => format!("(v_p->>{index})::BOOLEAN"),
        Ty::Bytes => format!("decode(v_p->>{index}, 'hex')"),
        Ty::Texts => array("element"),
        Ty::Bools => array("element::BOOLEAN"),
        Ty::Ints => array("element::BIGINT"),
        Ty::Now => "v_now".to_owned(),
    }
}

/// `entry`'s statement with its parameters read from the step.
fn statement(entry: &Entry) -> String {
    let args: Vec<String> = entry
        .params
        .iter()
        .enumerate()
        .map(|(index, ty)| param(index, *ty))
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    inline(entry.statement().sql(), &args)
}

/// What the function gathers from an answered row for `collect`.
fn gather(collect: Collect) -> String {
    let column = format!("v_row.\"{}\"", collect.column());
    let into = match collect {
        Collect::Closure(_) => "v_retired",
        Collect::Closures(_) => {
            return format!("v_retired := v_retired || COALESCE({column}, '{{}}'::BYTEA[]);");
        }
        Collect::PhaseEnd(_) => "v_phase_ends",
        Collect::Ancestry(_) => "v_leaves",
        Collect::Wake(_) => "v_wakes",
        Collect::Text(_) => "v_texts",
        Collect::Protect(_) => "v_protect",
    };
    let ty = match collect {
        Collect::Ancestry(_) | Collect::Wake(_) => "TEXT",
        _ => "BYTEA",
    };
    format!("IF {column} IS NOT NULL THEN {into} := {into} || {column}::{ty}; END IF;")
}

/// The reclaim of the prompt texts the steps released (`v_texts`), keeping
/// those they protected (`v_protect`): the closure is gathered and locked,
/// then each level deletes what no root references any more, and the
/// children of what it deleted are the next level.
fn text_reclaim() -> String {
    let reclaim = inline(
        prompts::RECLAIM.statement().sql(),
        &["v_texts", "v_protect", "4096"],
    );
    format!(
        "PERFORM * FROM ({reclaim}) AS retired; v_texts := '{{}}'; v_protect := '{{}}'; v_rows := 0;"
    )
}

/// Shared SQL inlining for domain-owned generated functions.
pub(super) fn inline_statement(sql: &str, values: &[&str]) -> String {
    inline(sql, values)
}

/// The artifact local end with the function's vouching for its answer
/// (FIG-5971, FIG-5942). The explicit form validates the answer's stored
/// pairs and plans with the build's own decoders. The function vouches
/// only for what needs no decoder: every plan a frame of the committing
/// session, its id spelled canonically, with exactly the gated body the
/// client computed; and every checked pair the ended referrer itself or
/// one of those plans. Anything else declines the call, and the explicit
/// form decides it.
fn end_local(entry: &Entry) -> String {
    let args: Vec<String> = entry
        .params
        .iter()
        .take(END_LOCAL_PARAMS)
        .enumerate()
        .map(|(index, ty)| param(index, *ty))
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let call = inline(entry.statement().sql(), &args);
    let body = param(END_LOCAL_PARAMS, Ty::Text);
    format!(
        "BEGIN
                {call} INTO v_answer;
            EXCEPTION WHEN invalid_text_representation THEN
                RAISE EXCEPTION 'lash_owner_commit: an end read a stored cleanup that is not JSON'
                    USING ERRCODE = 'LX006';
            END;
            v_frame := '[' || to_json(p_session)::TEXT || ',\"';
            IF EXISTS (
                SELECT 1 FROM jsonb_array_elements(v_answer->'plans') AS planned(item)
                WHERE NOT COALESCE(
                    planned.item->>0 = 'frame_environment'
                    AND planned.item->>2 = {body}
                    AND starts_with(planned.item->>1, v_frame)
                    AND right(planned.item->>1, 2) = '\"]'
                    AND substr(planned.item->>1, length(v_frame) + 1,
                        length(planned.item->>1) - length(v_frame) - 2) ~ '^[^\"\\\\[:cntrl:]]+$',
                    FALSE)
            ) OR EXISTS (
                SELECT 1 FROM jsonb_array_elements(v_answer->'checks') AS checked(item)
                WHERE NOT COALESCE(
                    (checked.item->>0 = {kind} AND checked.item->>1 = {id})
                    OR EXISTS (
                        SELECT 1 FROM jsonb_array_elements(v_answer->'plans') AS planned(item)
                        WHERE planned.item->>0 = checked.item->>0
                          AND planned.item->>1 = checked.item->>1),
                    FALSE)
            ) THEN
                RAISE EXCEPTION 'lash_owner_commit: an end answered what the call cannot vouch for'
                    USING ERRCODE = 'LX006';
            END IF;
            v_rows := 1;",
        kind = args[0],
        id = args[1],
    )
}

/// One step's branch of the dispatch: its statement, its rows counted (the
/// true rows of a flagged entry), its first row kept for a capture, and
/// what its rows name gathered.
fn branch(entry: &Entry) -> String {
    let run = match entry.shape {
        Shape::Exec => format!(
            "{};
            GET DIAGNOSTICS v_rows = ROW_COUNT;
            IF v_rows > 0 THEN v_first := '{{}}'::JSONB; END IF;",
            statement(entry)
        ),
        Shape::Returning | Shape::Select => {
            let counted = match entry.flag {
                None => "v_rows := v_rows + 1;".to_owned(),
                Some(flag) => {
                    format!("IF v_row.\"{flag}\" THEN v_rows := v_rows + 1; END IF;")
                }
            };
            let gathered: String = entry
                .collects()
                .map(|collect| format!("\n                {}", gather(collect)))
                .collect();
            format!(
                "v_rows := 0;
            FOR v_row IN {} LOOP
                IF v_step ? 'c' AND v_first IS NULL THEN
                    v_first := to_jsonb(v_row);
                END IF;
                {counted}{gathered}
            END LOOP;",
                statement(entry)
            )
        }
        Shape::EndLocal => end_local(entry),
        Shape::TextReclaim => text_reclaim(),
    };
    format!("        WHEN '{}' THEN\n            {run}\n", entry.name())
}

/// `sql`, a query, run for its effect.
fn perform(sql: &str) -> String {
    let sql = sql.trim_start();
    match sql.get(..6) {
        Some(select) if select.eq_ignore_ascii_case("SELECT") => format!("PERFORM{}", &sql[6..]),
        _ => panic!("not a query: {sql}"),
    }
}

/// A function's peel of `v_frontier` (FIG-5949): each level locks what of
/// it no other transaction holds, then deletes what of that no root names
/// and no edge reaches, and the children those reached are the next level.
/// It needs `v_frontier` and `v_locked` (`BYTEA[]`) and `v_rows` (`BIGINT`).
pub(crate) fn peel_loop() -> String {
    let peel_lock = inline(graph::peel_lock().sql(), &["v_frontier"]);
    let peel_level = inline(graph::peel_level().sql(), &["v_locked"]);
    format!(
        "WHILE cardinality(v_frontier) > 0 LOOP
        {peel_lock}
        INTO v_locked;
        EXIT WHEN cardinality(v_locked) = 0;
        {peel_level}
        INTO v_rows, v_frontier;
    END LOOP;"
    )
}

/// The function's DDL, as `schema.sql` declares it.
pub(crate) fn function_ddl() -> String {
    let executor = &*EXECUTOR_SQL;
    let envelope = inline(
        SQL.postgres.owner_envelope.sql(),
        &["p_actor", "p_epoch", "p_session"],
    );
    let put_bodies = inline(
        crate::blobs::blob_sql().postgres.insert_chunk.sql(),
        &["p_body_hashes", "p_bodies"],
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
    let lock_held = perform(&inline(
        graph::stored_nodes().sql(),
        &[
            "ARRAY(SELECT staged.digest FROM unnest(p_digests) AS staged(digest) \
           WHERE NOT (staged.digest = ANY(v_inserted)))",
        ],
    ));
    let (hold_lock, hold) = pages::hold_statements();
    let hold_lock = inline(hold_lock.sql(), &["v_retired", "v_phase_ends"]);
    let hold = inline(
        hold.sql(),
        &["v_retired", "v_phase_ends", "v_now", "v_held"],
    );
    let installed_bytes = inline(
        executor.installed_bytes.sql(),
        &["p_digests", "p_contents", "v_inserted"],
    );
    let ancestor_lock = inline(
        session_sql().graph_postgres.select_parent_for_update.sql(),
        &["v_node"],
    );
    let ancestor_reachable = inline(
        session_sql().graph_postgres.exists_reachable.sql(),
        &["v_node"],
    );
    let ancestor_retire = inline(session_sql().graph_postgres.retire.sql(), &["v_node"]);
    let wake = inline(SQL.actor.wake.sql(), &["v_owner", "v_now"]);
    let stamp_formats = inline(SQL.actor.stamp_formats.sql(), &["p_actor", "p_formats"]);
    let stamp_requires = inline(SQL.actor.stamp_requires.sql(), &["p_actor", "p_requires"]);
    let ack_through = inline(SQL.postgres.ack_through.sql(), &["p_actor", "p_ack"]);
    let delete_mail = inline(SQL.mail.delete_all.sql(), &["p_actor"]);
    let end = inline(SQL.actor.end.sql(), &["p_actor"]);
    let release = inline(
        SQL.actor.release.sql(),
        &["p_actor", "p_release", "p_due", "v_now"],
    );
    let release_unadvanced = inline(
        SQL.actor.release.sql(),
        &["p_actor", "'ready'", "p_due", "v_now"],
    );
    let unadvanced = inline(SQL.actor.unadvanced.sql(), &["p_actor"]);
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
    let fleet_fence = perform(connection_sql().lock_xact_fleet_fence_shared.sql());
    format!(
        "CREATE OR REPLACE FUNCTION {FUNCTION}(
    p_registry TEXT, p_actor TEXT, p_epoch BIGINT, p_session TEXT,
    p_fleet_low INTEGER, p_fleet_high INTEGER, p_token TEXT,
    p_lock_timeout TEXT, p_statement_ms BIGINT,
    p_body_hashes BYTEA[], p_bodies BYTEA[],
    p_named BYTEA[], p_roots BYTEA[], p_digests BYTEA[], p_contents BYTEA[],
    p_parents BYTEA[], p_children BYTEA[], p_steps JSONB,
    p_formats TEXT, p_ack BIGINT, p_release TEXT, p_due BIGINT, p_now BIGINT, p_requires TEXT)
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
    v_inserted BYTEA[] := '{{}}';
    v_parents BYTEA[];
    v_children BYTEA[];
    v_installed BIGINT := 0;
    v_rows BIGINT;
    v_index INTEGER := 0;
    v_step JSONB;
    v_p JSONB;
    v_state TEXT := 'owned';
    v_retired BYTEA[] := '{{}}';
    v_phase_ends BYTEA[] := '{{}}';
    v_held BYTEA[];
    v_row RECORD;
    v_first JSONB;
    v_slots JSONB := '{{}}';
    v_when JSONB;
    v_leaves TEXT[] := '{{}}';
    v_node TEXT;
    v_parent TEXT;
    v_reached BOOLEAN;
    v_wakes TEXT[] := '{{}}';
    v_owner TEXT;
    v_woken BIGINT;
    v_texts BYTEA[] := '{{}}';
    v_protect BYTEA[] := '{{}}';
    v_tfront BYTEA[];
    v_closure BYTEA[];
    v_tparents BYTEA[];
    v_tchildren BYTEA[];
    v_answer JSONB;
    v_frame TEXT;
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
    -- The checkpoint bodies a head commit supplies are stored before the
    -- graph that names them (FIG-5942).
    IF cardinality(p_body_hashes) > 0 THEN
        {put_bodies};
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
        -- A staged node another writer stored is held against a peel; one a
        -- peel took first is a conflict, which the explicit form installs
        -- again (FIG-5949).
        {lock_held};
        {held_conflicts} INTO v_rows;
        IF v_rows > 0 THEN
            RAISE EXCEPTION 'lash_owner_commit: the graph refused' USING ERRCODE = 'LX004';
        END IF;
        SELECT COALESCE(array_agg(staged.parent), '{{}}'), COALESCE(array_agg(staged.child), '{{}}')
        INTO v_parents, v_children
        FROM unnest(p_parents, p_children) AS staged(parent, child)
        WHERE staged.parent = ANY(v_inserted);
        IF cardinality(v_parents) > 0 THEN
            {insert_edges};
        END IF;
        {installed_bytes} INTO v_installed;
    END IF;
    FOR v_step IN SELECT value FROM jsonb_array_elements(p_steps) LOOP
        v_index := v_index + 1;
        -- A step runs only when the slot it names holds what it names: a
        -- row, or a row whose column is true (FIG-5942).
        IF v_step ? 'w' THEN
            v_when := v_slots->(v_step->'w'->>0);
            IF (CASE
                    WHEN v_when IS NULL OR v_when = 'null'::JSONB THEN FALSE
                    WHEN v_step->'w'->>1 IS NULL THEN TRUE
                    ELSE COALESCE((v_when->>(v_step->'w'->>1))::BOOLEAN, FALSE)
                END) IS DISTINCT FROM (v_step->'w'->>2)::BOOLEAN THEN
                CONTINUE;
            END IF;
        END IF;
        v_p := v_step->'p';
        v_first := NULL;
        CASE v_step->>'s'
{branches}        ELSE
            RAISE EXCEPTION 'lash_owner_commit: no step %', v_step->>'s' USING ERRCODE = 'LX000';
        END CASE;
        IF v_step ? 'c' THEN
            v_slots := v_slots || jsonb_build_object(v_step->>'c', COALESCE(v_first, 'null'::JSONB));
        END IF;
        IF v_step ? 'n' AND v_rows <> (v_step->>'n')::BIGINT THEN
            RAISE EXCEPTION 'lash_owner_commit: step % refused', v_index - 1 USING ERRCODE = 'LX001';
        END IF;
        IF p_statement_ms IS NOT NULL
            AND clock_timestamp() - statement_timestamp() > p_statement_ms * INTERVAL '1 millisecond' THEN
            RAISE EXCEPTION 'canceling statement due to statement timeout' USING ERRCODE = '57014';
        END IF;
        IF v_step ? 'r' THEN
            v_retired := v_retired || ARRAY(SELECT decode(jsonb_array_elements_text(v_step->'r'), 'hex'));
        END IF;
        IF v_step ? 'a' THEN
            v_leaves := v_leaves || ARRAY(SELECT jsonb_array_elements_text(v_step->'a'));
        END IF;
        -- Retire the ancestry only the step's leaves kept alive, each leaf
        -- in id order: a node goes while no live child, revision or
        -- terminal window names it.
        IF cardinality(v_leaves) > 0 THEN
            FOR v_node IN SELECT DISTINCT leaf FROM unnest(v_leaves) AS named(leaf) ORDER BY leaf LOOP
                LOOP
                    {ancestor_lock}
                    INTO v_parent;
                    EXIT WHEN NOT FOUND;
                    {ancestor_reachable}
                    INTO v_reached;
                    EXIT WHEN v_reached;
                    {ancestor_retire};
                    EXIT WHEN v_parent IS NULL;
                    v_node := v_parent;
                END LOOP;
            END LOOP;
            v_leaves := '{{}}';
        END IF;
        -- Wake each owner the step settled but the committing actor; one
        -- that ended or is gone has nothing to wake.
        IF cardinality(v_wakes) > 0 THEN
            FOR v_owner IN SELECT DISTINCT owner FROM unnest(v_wakes) AS woke(owner) ORDER BY owner LOOP
                IF v_owner <> p_actor THEN
                    WITH woken AS ({wake})
                    SELECT count(*) INTO v_woken FROM woken;
                END IF;
            END LOOP;
            v_wakes := '{{}}';
        END IF;
    END LOOP;
    -- Hold what the steps retired for the bounded retirement steps
    -- (FIG-6191, D-RET): each retired root's closure, and an ended turn's
    -- PhaseRoot alone, every node locked before any hold row (FIG-6193).
    IF cardinality(v_retired) > 0 OR cardinality(v_phase_ends) > 0 THEN
        {hold_lock}
        INTO v_held;
        {hold};
    END IF;
    IF p_formats IS NOT NULL THEN
        {stamp_formats};
    END IF;
    IF p_requires IS NOT NULL THEN
        {stamp_requires};
    END IF;
    IF p_ack IS NOT NULL THEN
        {ack_through};
    END IF;
    CASE p_release
    WHEN 'terminal' THEN
        {delete_mail};
        {end}
        INTO v_state;
    WHEN 'idle', 'waiting', 'ready' THEN
        {release}
        INTO v_state;
    WHEN 'ready_unadvanced' THEN
        {release_unadvanced}
        INTO v_state;
        {unadvanced};
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
        + &renewal_ddl()
        + &held_texts_ddl()
        + &prompts::retirement_function_ddl()
        + &prompt_record_ddl()
}

/// The same frontier the explicit record used: share-lock a level, read it
/// after the locks, stop at held immutable nodes, and install only the absent
/// reachable texts and their edges. A prefix is replaced after publication,
/// then its former closure is reclaimed with the held frontier protected.
fn prompt_record_ddl() -> String {
    let sql = &*prompts::SQL;
    let pg = &*prompts::POSTGRES_SQL;
    let snapshot = inline(
        sql.insert_snapshot.sql(),
        &[
            "p_session",
            "p_owner",
            "p_call",
            "p_snapshot",
            "p_request",
            "p_epoch",
        ],
    );
    let held = inline(
        pg.held_after_lock.sql(),
        &["v_frontier", &prompts::TEXT_LOCK_SEED.to_string()],
    );
    let insert = inline(pg.insert_texts.sql(), &["v_hashes", "v_bodies"]);
    let index = inline(pg.insert_index_edges.sql(), &["v_parents", "v_children"]);
    let edges = inline(
        pg.insert_edges.sql(),
        &["p_session", "p_owner", "p_call", "p_parts", "p_edges"],
    );
    let release = inline(sql.release_prefix.sql(), &["p_session"]);
    let prefix = inline(sql.set_prefix.sql(), &["p_session", "p_prefix"]);
    let reclaim = text_reclaim();
    format!(
        "CREATE OR REPLACE FUNCTION lash_prompt_record(
    p_session TEXT, p_owner TEXT, p_call TEXT, p_snapshot TEXT, p_request TEXT, p_epoch BIGINT,
    p_hashes BYTEA[], p_bodies TEXT[], p_parents BYTEA[], p_children BYTEA[], p_roots BYTEA[],
    p_parts TEXT[], p_edges BYTEA[], p_prefix BYTEA, p_turn BOOLEAN)
RETURNS TABLE (accepted BOOLEAN, missing BYTEA)
LANGUAGE plpgsql VOLATILE
AS $lash_prompt_record$
DECLARE
    v_rows BIGINT;
    v_frontier BYTEA[] := p_roots;
    v_held BYTEA[];
    v_new BYTEA[];
    v_unheld BYTEA[] := '{{}}';
    v_hash BYTEA;
    v_position INTEGER;
    v_hashes BYTEA[];
    v_bodies TEXT[];
    v_parents BYTEA[];
    v_children BYTEA[];
    v_texts BYTEA[] := '{{}}';
    v_protect BYTEA[] := '{{}}';
    v_tfront BYTEA[];
    v_closure BYTEA[];
    v_tparents BYTEA[];
    v_tchildren BYTEA[];
BEGIN
    WITH recorded AS ({snapshot}) SELECT count(*) INTO v_rows FROM recorded;
    IF v_rows <> 1 THEN
        RETURN QUERY SELECT FALSE, NULL::BYTEA;
        RETURN;
    END IF;
    WHILE cardinality(v_frontier) > 0 LOOP
        v_held := ARRAY({held});
        v_protect := v_protect || v_held;
        v_new := '{{}}';
        FOREACH v_hash IN ARRAY v_frontier LOOP
            IF v_hash = ANY(v_held) OR v_hash = ANY(v_unheld) THEN CONTINUE; END IF;
            v_position := array_position(p_hashes, v_hash);
            IF v_position IS NULL OR p_bodies[v_position] IS NULL THEN
                RETURN QUERY SELECT FALSE, v_hash;
                RETURN;
            END IF;
            v_new := v_new || v_hash;
            v_unheld := v_unheld || v_hash;
        END LOOP;
        v_frontier := ARRAY(SELECT DISTINCT edge.child
            FROM unnest(p_parents, p_children) AS edge(parent, child)
            WHERE edge.parent = ANY(v_new) ORDER BY edge.child);
    END LOOP;
    IF cardinality(v_unheld) > 0 THEN
        SELECT array_agg(supplied.hash ORDER BY supplied.hash), array_agg(supplied.body ORDER BY supplied.hash)
        INTO v_hashes, v_bodies FROM unnest(p_hashes, p_bodies) AS supplied(hash, body)
        WHERE supplied.hash = ANY(v_unheld);
        {insert};
        SELECT COALESCE(array_agg(edge.parent ORDER BY edge.parent, edge.child), '{{}}'),
               COALESCE(array_agg(edge.child ORDER BY edge.parent, edge.child), '{{}}')
        INTO v_parents, v_children FROM unnest(p_parents, p_children) AS edge(parent, child)
        WHERE edge.parent = ANY(v_unheld);
        {index};
    END IF;
    {edges};
    IF p_turn THEN
        WITH released AS ({release})
        SELECT COALESCE(array_agg(hash), '{{}}') INTO v_texts FROM released;
        IF p_prefix IS NOT NULL THEN {prefix}; END IF;
        {reclaim}
    END IF;
    RETURN QUERY SELECT TRUE, NULL::BYTEA;
END
$lash_prompt_record$;
"
    )
}

/// The guarded renewal of a node's lease as one server request (FIG-7049):
/// the writer fence's shared lock under the renewal role's lock limit, then
/// the recorded epoch and the database clock, then the renewal of the node's
/// boot. An epoch outside `[p_fleet_low, p_fleet_high]`, or none, renews
/// nothing and answers the epoch it read, which the caller refuses; a boot
/// that holds no lease any more renews nothing and answers no expiry.
fn renewal_ddl() -> String {
    let fleet_fence = perform(connection_sql().lock_xact_fleet_fence_shared.sql());
    let fence_and_clock = SQL.postgres.fence_and_clock.sql();
    let renew = inline(
        SQL.node.renew.sql(),
        &[
            "p_node",
            "p_boot",
            "GREATEST(LEAST(v_now::NUMERIC + p_ttl, 9223372036854775807), \
             -9223372036854775808)::BIGINT",
        ],
    );
    format!(
        "CREATE OR REPLACE FUNCTION {RENEW_FUNCTION}(
    p_node TEXT, p_boot TEXT, p_ttl BIGINT, p_fleet_low INTEGER, p_fleet_high INTEGER,
    p_lock_timeout TEXT, p_statement_ms BIGINT, p_now BIGINT)
RETURNS TABLE (o_fleet INTEGER, o_expires BIGINT)
LANGUAGE plpgsql
AS ${RENEW_FUNCTION}$
DECLARE
    v_fleet INTEGER;
    v_clock BIGINT;
    v_now BIGINT;
    v_expires BIGINT;
BEGIN
    IF current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION 'lash_node_renew: requires read committed' USING ERRCODE = '25000';
    END IF;
    IF p_lock_timeout IS NOT NULL THEN
        PERFORM set_config('lock_timeout', p_lock_timeout, true);
    END IF;
    {fleet_fence};
    {fence_and_clock}
    INTO v_fleet, v_clock;
    IF v_fleet IS NULL OR v_fleet < p_fleet_low OR v_fleet > p_fleet_high THEN
        RETURN QUERY SELECT v_fleet, NULL::BIGINT;
        RETURN;
    END IF;
    v_now := COALESCE(p_now, v_clock);
    {renew}
    INTO v_expires;
    -- As in the owner routine, an imported pool's statement budget is
    -- checked before success: raising rolls the renewal back as a whole.
    IF p_statement_ms IS NOT NULL
        AND clock_timestamp() - statement_timestamp() > p_statement_ms * INTERVAL '1 millisecond' THEN
        RAISE EXCEPTION 'canceling statement due to statement timeout' USING ERRCODE = '57014';
    END IF;
    RETURN QUERY SELECT v_fleet, v_expires;
END
${RENEW_FUNCTION}$;
"
    )
}

/// One level of a prompt record's text frontier as one server request
/// (FIG-7049): the texts `p_keys` share-locked in key order, then which of
/// them are stored, read by its own statement once every lock is held. A
/// volatile function's statements each read a fresh snapshot under read
/// committed, so a text a concurrent reclaim deleted before this lock is
/// not answered held.
fn held_texts_ddl() -> String {
    let lock = perform(&inline(
        connection_sql().lock_xact_shared_by_keys_seeded.sql(),
        &["p_keys", "p_seed"],
    ));
    let held = inline(prompts::held_texts().sql(), &["p_keys"]);
    format!(
        "CREATE OR REPLACE FUNCTION {HELD_TEXTS_FUNCTION}(p_keys BYTEA[], p_seed BIGINT)
RETURNS SETOF BYTEA
LANGUAGE plpgsql VOLATILE
AS ${HELD_TEXTS_FUNCTION}$
BEGIN
    {lock};
    RETURN QUERY {held};
END
${HELD_TEXTS_FUNCTION}$;
"
    )
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
