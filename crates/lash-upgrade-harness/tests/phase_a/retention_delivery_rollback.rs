//! `retention_delivery_rollback` (ADR 0115 §6): what N+1 writes before
//! finalize survives N's rollback, N's retention and GC, and a return to
//! N+1, with nothing lost and nothing delivered twice.
//!
//! On PostgreSQL and on SQLite, N+1 answers a turn that stores and roots an
//! attachment, then, as a host, publishes two modules under host pins:
//! one module under a pin it keeps and a pin it releases, one under the
//! released pin alone. Releasing arms the pin's cleanup obligation and its
//! fence. It also puts attachment bytes no session roots. Then N rolls
//! back: one relay pass delivers the released pin's cleanup, the retention
//! sweep and the attachment GC run, and N answers a turn on the session
//! N+1 wrote. N+1 returns, relays, sweeps and answers a turn of its own.
//!
//! The leg counts rows and deliveries, never time: the session's graph
//! nodes, turn commits and attachment roots only grow; the kept pin's edge,
//! the shared module and the fence survive every step; the released pin's
//! cleanup is delivered exactly once, severing its edges and reclaiming the
//! module only it held; the only attachment any GC deletes is the one no
//! session roots; and every turn's model call is made once.
//!
//! On PostgreSQL the ledger also holds a cleanup row whose referrer kind no
//! build of this window knows, as a later build writes it after widening
//! the column's check. N's relay stalls it `undecodable`, typed, and it
//! stays outstanding and listed under N and again under N+1. It does not
//! hold N's drain: `lashctl drain-status` reads drained and lists the row,
//! its identity and its typed reason, for the operator to settle. On SQLite the
//! kind check is inline in the table, so no build can stall such a row
//! there without rebuilding the table; the unknown kind runs on PostgreSQL
//! only.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use lash_sqlite_store::SqliteDatabase;
use lash_upgrade_harness::harness::{Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, Operator, block_on};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::retention::{
    InspectReport, MaintainReport, Orphaned, Published, RelayReport, Released,
};
use lash_upgrade_harness::node::served_by;
use serde::Serialize;
use sqlx::{Connection, PgConnection, Row};

use crate::support::{Leg, quiesce, record};

/// The referrer kind a later build writes, which no build of this window
/// knows.
const UNKNOWN_KIND: &str = "synthetic_next";
/// The obligation id of the unknown-kind row.
const UNKNOWN_OBLIGATION: &str = "obligation-written-by-a-later-build";

/// Where the leg reads a case's rows: its PostgreSQL database, or its
/// SQLite durable core.
enum Rows {
    Postgres(String),
    Sqlite(PathBuf),
}

/// The rows the leg counts, as one read sees them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Snapshot {
    graph_nodes: i64,
    turn_commits: i64,
    attachment_roots: i64,
    /// `(module_ref, referrer_kind, referrer_id)`, sorted.
    edges: Vec<(String, String, String)>,
    /// Every module reference whose bytes are stored, sorted.
    modules: Vec<String>,
    /// `(referrer_kind, referrer_id)`, sorted.
    fences: Vec<(String, String)>,
    /// `(referrer_kind, referrer_id, state, stall_reason)`, sorted.
    cleanups: Vec<(String, String, String, Option<String>)>,
}

impl Rows {
    fn of(case: &Case) -> Result<Self> {
        match (case.postgres_url(), case.sqlite_dir()) {
            (Some(url), _) => Ok(Self::Postgres(url.to_owned())),
            (None, Some(dir)) => Ok(Self::Sqlite(
                dir.join(SqliteDatabase::DurableCore.file_name()),
            )),
            (None, None) => bail!("{} has no store", case.name),
        }
    }

    fn snapshot(&self, session: &str) -> Result<Snapshot> {
        match self {
            Self::Postgres(url) => block_on(postgres_snapshot(url, session)),
            Self::Sqlite(path) => sqlite_snapshot(path, session),
        }
    }

    /// Write a cleanup row as a later build would: its expand widens the
    /// kind check to the kind it adds, then it arms the row due.
    fn inject_unknown_cleanup(&self) -> Result<()> {
        let Self::Postgres(url) = self else {
            bail!("the unknown kind runs on PostgreSQL only");
        };
        let sql = format!(
            "ALTER TABLE lash_artifact_cleanup_obligations
                 DROP CONSTRAINT ck_artifact_cleanup_obligations_kind;
             ALTER TABLE lash_artifact_cleanup_obligations
                 ADD CONSTRAINT ck_artifact_cleanup_obligations_kind CHECK (referrer_kind IN
                 ('frame_environment', 'process_record', 'subscription_revision', 'start',
                  'execution', 'host_pin', 'definition_revision', '{UNKNOWN_KIND}'));
             INSERT INTO lash_artifact_cleanup_obligations
                 (referrer_kind, referrer_id, cleanup_json, obligation_id,
                  obligation_state, obligation_due_at_ms)
             VALUES ('{UNKNOWN_KIND}', 'a-later-referrer', '{{}}', '{UNKNOWN_OBLIGATION}',
                     'due', 0);"
        );
        let url = url.clone();
        block_on(async move {
            let mut connection = PgConnection::connect(&url).await?;
            sqlx::raw_sql(&sql).execute(&mut connection).await?;
            connection.close().await.ok();
            Ok(())
        })
    }
}

async fn postgres_snapshot(url: &str, session: &str) -> Result<Snapshot> {
    let mut pg = PgConnection::connect(url).await?;
    let count = |table: &'static str| format!("SELECT COUNT(*) FROM {table} WHERE session_id = $1");
    let graph_nodes: i64 = sqlx::query_scalar(&count("lash_graph_nodes"))
        .bind(session)
        .fetch_one(&mut pg)
        .await?;
    let turn_commits: i64 = sqlx::query_scalar(&count("lash_runtime_turn_commits"))
        .bind(session)
        .fetch_one(&mut pg)
        .await?;
    let attachment_roots: i64 = sqlx::query_scalar(&count("lash_attachment_manifest"))
        .bind(session)
        .fetch_one(&mut pg)
        .await?;
    let edges = sqlx::query(
        "SELECT artifact_ref, referrer_kind, referrer_id FROM lash_artifact_referrer_edges
         ORDER BY 1, 2, 3",
    )
    .fetch_all(&mut pg)
    .await?
    .iter()
    .map(|row| Ok((row.try_get(0)?, row.try_get(1)?, row.try_get(2)?)))
    .collect::<Result<_, sqlx::Error>>()?;
    let modules = sqlx::query_scalar("SELECT artifact_ref FROM lash_lashlang_artifacts ORDER BY 1")
        .fetch_all(&mut pg)
        .await?;
    let fences =
        sqlx::query("SELECT referrer_kind, referrer_id FROM lash_referrer_fences ORDER BY 1, 2")
            .fetch_all(&mut pg)
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<_, sqlx::Error>>()?;
    let cleanups = sqlx::query(
        "SELECT referrer_kind, referrer_id, obligation_state, obligation_stall_reason
         FROM lash_artifact_cleanup_obligations ORDER BY 1, 2",
    )
    .fetch_all(&mut pg)
    .await?
    .iter()
    .map(|row| {
        Ok((
            row.try_get(0)?,
            row.try_get(1)?,
            row.try_get(2)?,
            row.try_get(3)?,
        ))
    })
    .collect::<Result<_, sqlx::Error>>()?;
    pg.close().await.ok();
    Ok(Snapshot {
        graph_nodes,
        turn_commits,
        attachment_roots,
        edges,
        modules,
        fences,
        cleanups,
    })
}

fn sqlite_snapshot(path: &Path, session: &str) -> Result<Snapshot> {
    let db =
        rusqlite::Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    let count = |table: &str| -> Result<i64> {
        Ok(db.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
            [session],
            |row| row.get(0),
        )?)
    };
    let edges = db
        .prepare(
            "SELECT artifact_ref, referrer_kind, referrer_id FROM artifact_referrer_edges
             ORDER BY 1, 2, 3",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let modules = db
        .prepare("SELECT artifact_ref FROM artifact_refs ORDER BY 1")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let fences = db
        .prepare("SELECT referrer_kind, referrer_id FROM referrer_fences ORDER BY 1, 2")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let cleanups = db
        .prepare(
            "SELECT referrer_kind, referrer_id, obligation_state, obligation_stall_reason
             FROM artifact_cleanup_obligations ORDER BY 1, 2",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(Snapshot {
        graph_nodes: count("graph_nodes")?,
        turn_commits: count("runtime_turn_commits")?,
        attachment_roots: count("attachment_manifest")?,
        edges,
        modules,
        fences,
        cleanups,
    })
}

/// The evidence one case leaves beside the leg.
#[derive(Serialize)]
struct Evidence {
    before_rollback: Snapshot,
    after_n: Snapshot,
    after_return: Snapshot,
    relay_n: RelayReport,
    relay_next: RelayReport,
    maintain_n: MaintainReport,
    maintain_next: MaintainReport,
    inspect_n: InspectReport,
    inspect_next: InspectReport,
}

/// The cleanup rows a snapshot holds for the unknown kind, stalled or not.
fn unknown_rows(snapshot: &Snapshot) -> Vec<&(String, String, String, Option<String>)> {
    snapshot
        .cleanups
        .iter()
        .filter(|row| row.0 == UNKNOWN_KIND)
        .collect()
}

/// One roll over `case`: N+1 writes, N rolls back and maintains, N+1
/// returns.
///
/// `operator` is N+1's `lashctl` over a PostgreSQL case, which drains N's
/// generation on the return; SQLite has no drain.
fn roll(leg: &Leg, case: &Case, operator: Option<&Operator>) -> Result<Evidence> {
    let unknown_kind = operator.is_some();
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let rows = Rows::of(case)?;
    let session = case.session_id("retained");
    let marker = |step: &str| format!("retention {step} {}", case.name);

    // N+1, before finalize: a turn that roots an attachment.
    let next_node = next.serve(case)?;
    let next_generation = next_node.generation()?.to_owned();
    let answered = next.turn_with_attachment(
        case,
        &session,
        &marker("next-writes"),
        "an attachment N+1 stored",
    )?;
    ensure!(
        answered.reply.as_deref() == Some(served_by(BuildLabel::Next, &next_generation).as_str()),
        "N+1 did not answer the first turn: {answered:?}"
    );
    quiesce(case, &session)?;
    next_node.stop()?;
    let rooted: InspectReport = next.retention(case, &["inspect"])?;
    ensure!(
        !rooted.attachments.is_empty(),
        "N+1's turn stored no attachment: {rooted:?}"
    );

    // N+1, as a host: two pins over two modules, one pin released.
    let kept = lash_core::HostArtifactPin::mint().to_string();
    let ended = lash_core::HostArtifactPin::mint().to_string();
    let shared: Published =
        next.retention(case, &["publish", "--pin", &kept, "--module", "shared"])?;
    next.retention::<Published>(case, &["publish", "--pin", &ended, "--module", "shared"])?;
    let sole: Published =
        next.retention(case, &["publish", "--pin", &ended, "--module", "sole"])?;
    let released: Released = next.retention(case, &["release", "--pin", &ended])?;
    let orphan: Orphaned = next.retention(case, &["orphan", "--text", &marker("orphan")])?;
    if unknown_kind {
        rows.inject_unknown_cleanup()?;
    }

    let before = rows.snapshot(&session)?;
    let mut expected_edges = vec![
        (
            shared.module_ref.clone(),
            "host_pin".to_owned(),
            kept.clone(),
        ),
        (
            shared.module_ref.clone(),
            "host_pin".to_owned(),
            ended.clone(),
        ),
        (
            sole.module_ref.clone(),
            "host_pin".to_owned(),
            ended.clone(),
        ),
    ];
    expected_edges.sort();
    ensure!(
        before.edges == expected_edges,
        "N+1 wrote edges {:?}",
        before.edges
    );
    let fences = vec![("host_pin".to_owned(), ended.clone())];
    ensure!(
        before.fences == fences,
        "N+1 wrote fences {:?}",
        before.fences
    );
    ensure!(
        before
            .cleanups
            .iter()
            .filter(|row| row.0 == "host_pin")
            .map(|row| (row.1.as_str(), row.2.as_str()))
            .collect::<Vec<_>>()
            == [(ended.as_str(), "due")],
        "the released pin's cleanup is not due: {:?} ({released:?})",
        before.cleanups
    );
    ensure!(
        before.graph_nodes > 0 && before.turn_commits == 1 && before.attachment_roots >= 1,
        "N+1's turn left {before:?}"
    );
    let written: InspectReport =
        next.retention(case, &["inspect", "--module", "shared", "--module", "sole"])?;
    ensure!(
        written
            .modules
            .iter()
            .all(|module| module.verified == Some(true)),
        "N+1 did not read its own modules: {written:?}"
    );
    ensure!(
        written.attachments.contains(&orphan.attachment_id),
        "the orphan is not stored: {written:?}"
    );

    // Rollback: N relays, sweeps and collects garbage, with no deployment up.
    let relay_n: RelayReport = n.retention(case, &["relay"])?;
    let (claimed, stalled) = if unknown_kind { (2, 1) } else { (1, 0) };
    ensure!(
        relay_n.claimed == claimed
            && relay_n.delivered == 1
            && relay_n.stalled == stalled
            && relay_n.retried == 0
            && relay_n.claim_lost == 0,
        "N's relay pass: {relay_n:?}"
    );
    let maintain_n: MaintainReport = n.retention(case, &["maintain", "--session", &session])?;
    ensure!(
        maintain_n.attachments_reclaimed == 1,
        "N's GC reclaimed {} attachments, not the one orphan: {maintain_n:?}",
        maintain_n.attachments_reclaimed
    );
    let inspect_n: InspectReport =
        n.retention(case, &["inspect", "--module", "shared", "--module", "sole"])?;
    ensure!(
        inspect_n.attachments == rooted.attachments,
        "N's GC left {:?}, not the rooted {:?}",
        inspect_n.attachments,
        rooted.attachments
    );
    ensure!(
        inspect_n.modules[0].verified == Some(true) && inspect_n.modules[1].verified.is_none(),
        "N read the modules as {:?}",
        inspect_n.modules
    );
    let after_n = rows.snapshot(&session)?;
    let kept_edge = vec![(
        shared.module_ref.clone(),
        "host_pin".to_owned(),
        kept.clone(),
    )];
    ensure!(
        after_n.edges == kept_edge,
        "N's cleanup left edges {:?}",
        after_n.edges
    );
    ensure!(
        after_n.modules == [shared.module_ref.clone()],
        "N's cleanup left modules {:?}, not the shared one",
        after_n.modules
    );
    ensure!(
        after_n.fences == fences,
        "N's cleanup left fences {:?}",
        after_n.fences
    );
    ensure!(
        (
            after_n.graph_nodes,
            after_n.turn_commits,
            after_n.attachment_roots
        ) == (
            before.graph_nodes,
            before.turn_commits,
            before.attachment_roots
        ),
        "N's retention changed N+1's session: {before:?} -> {after_n:?}"
    );
    ensure!(
        after_n.cleanups.iter().all(|row| row.0 == UNKNOWN_KIND),
        "the released pin's cleanup was not delivered: {:?}",
        after_n.cleanups
    );
    if unknown_kind {
        outstanding_and_typed(&after_n, &inspect_n, "N")?;
    } else {
        ensure!(
            after_n.cleanups.is_empty() && inspect_n.stalled.is_empty(),
            "N stalled {:?}",
            inspect_n.stalled
        );
    }

    // N serves and answers a turn on the session N+1 wrote.
    let n_node = n.serve(case)?;
    let n_generation = n_node.generation()?.to_owned();
    let answered = n.turn(case, &session, &marker("n-continues"))?;
    ensure!(
        answered.reply.as_deref() == Some(served_by(BuildLabel::N, &n_generation).as_str()),
        "N did not continue N+1's session: {answered:?}"
    );

    // Return to N+1 as a roll does: N+1 comes up, N's generation drains,
    // and N stops. Nothing is due, nothing is collected, and N+1 answers.
    quiesce(case, &session)?;
    let next_node = next.serve(case)?;
    let next_generation = next_node.generation()?.to_owned();
    if let Some(operator) = operator {
        operator.run("drain", Some(&n_generation))?;
    }
    n_node.stop()?;
    if let Some(operator) = operator {
        drained_with_the_stalled_row_listed(operator, &n_generation)?;
        operator.run("end-drain", Some(&n_generation))?;
    }
    let relay_next: RelayReport = next.retention(case, &["relay"])?;
    ensure!(
        relay_next.claimed == 0 && relay_next.delivered == 0,
        "N+1 delivered again after N: {relay_next:?}"
    );
    let maintain_next: MaintainReport =
        next.retention(case, &["maintain", "--session", &session])?;
    ensure!(
        maintain_next.attachments_reclaimed == 0,
        "N+1's GC reclaimed {maintain_next:?}"
    );
    let inspect_next: InspectReport =
        next.retention(case, &["inspect", "--module", "shared", "--module", "sole"])?;
    ensure!(
        inspect_next.modules == inspect_n.modules
            && inspect_next.stalled == inspect_n.stalled
            && inspect_next.attachments == rooted.attachments,
        "N+1 reads {inspect_next:?} after N read {inspect_n:?}"
    );
    let refused =
        next.retention::<Published>(case, &["publish", "--pin", &ended, "--module", "sole"]);
    ensure!(
        refused.is_err(),
        "the released pin published again after the rollback: {refused:?}"
    );
    let answered = next.turn(case, &session, &marker("next-returns"))?;
    ensure!(
        answered.reply.as_deref() == Some(served_by(BuildLabel::Next, &next_generation).as_str()),
        "N+1 did not answer on its return: {answered:?}"
    );
    quiesce(case, &session)?;
    next_node.stop()?;
    let after_return = rows.snapshot(&session)?;
    ensure!(
        after_return.turn_commits == 3
            && after_return.graph_nodes > after_n.graph_nodes
            && after_return.attachment_roots == before.attachment_roots,
        "the session after the return: {after_return:?}"
    );
    ensure!(
        after_return.edges == kept_edge
            && after_return.modules == after_n.modules
            && after_return.fences == fences
            && after_return.cleanups == after_n.cleanups,
        "the return changed the referrer rows: {after_n:?} -> {after_return:?}"
    );
    if unknown_kind {
        outstanding_and_typed(&after_return, &inspect_next, "N+1")?;
    }
    ensure!(
        next.probe(case, Some(&session))?.session_present == Some(true),
        "N+1 does not read the session"
    );

    // Every delivery once: the one released pin's cleanup, and one model
    // call per turn.
    ensure!(
        relay_n.delivered + relay_next.delivered == 1,
        "the released pin's cleanup was delivered {} times",
        relay_n.delivered + relay_next.delivered
    );
    for step in ["next-writes", "n-continues", "next-returns"] {
        let calls = case.effects_of(&marker(step))?;
        ensure!(
            calls.len() == 1,
            "{step} was answered by {} model calls: {calls:?}",
            calls.len()
        );
    }
    Ok(Evidence {
        before_rollback: before,
        after_n,
        after_return,
        relay_n,
        relay_next,
        maintain_n,
        maintain_next,
        inspect_n,
        inspect_next,
    })
}

/// N's generation holds nothing, so it reads drained (exit 0) although the
/// unknown-kind row is stalled: no build of the window can decode that row,
/// and keeping N's deployment would not settle it. The status lists it alone,
/// by its obligation id, with its typed reason and the kind no build knows.
fn drained_with_the_stalled_row_listed(operator: &Operator, generation: &str) -> Result<()> {
    let (code, status) = operator.answer("drain-status", Some(generation))?;
    let result = &status["result"];
    let counts = &result["stalled_obligations"];
    let listed = result["stalled"].as_array();
    ensure!(
        code == 0
            && status["error"].is_null()
            && result["drained"] == true
            && result["live_processes"] == 0
            && result["parked_processes"] == 0
            && result["in_flight_turns"] == 0
            && result["parked_turns"] == 0
            && result["closing_sessions"] == 0
            && counts["artifact_cleanup"] == 1
            && counts.as_object().is_some_and(|kinds| kinds
                .values()
                .filter(|count| **count != 0)
                .count()
                == 1),
        "a stalled row alone does not read drained: {status}"
    );
    ensure!(
        listed.is_some_and(|listed| listed.len() == 1)
            && result["stalled"][0]["kind"] == "artifact_cleanup"
            && result["stalled"][0]["obligation_id"] == UNKNOWN_OBLIGATION
            && result["stalled"][0]["reason"] == "undecodable"
            && result["stalled"][0]["row"].is_null()
            && result["stalled"][0]["undecodable"]
                .as_str()
                .is_some_and(|detail| detail.contains(UNKNOWN_KIND)),
        "the drain does not list the one stalled row, typed: {status}"
    );
    Ok(())
}

/// The unknown-kind row is still in the ledger, stalled `undecodable`, and
/// the build's typed listing names it and its kind.
fn outstanding_and_typed(snapshot: &Snapshot, inspect: &InspectReport, build: &str) -> Result<()> {
    let unknown = unknown_rows(snapshot);
    ensure!(
        unknown.len() == 1
            && unknown[0].2 == "stalled"
            && unknown[0].3.as_deref() == Some("undecodable"),
        "under {build} the unknown-kind row stands {unknown:?}"
    );
    ensure!(
        inspect.stalled.len() == 1
            && inspect.stalled[0].obligation_id == UNKNOWN_OBLIGATION
            && inspect.stalled[0].reason == "undecodable"
            && inspect.stalled[0].referrer_kind.is_none()
            && inspect.stalled[0]
                .undecodable
                .as_deref()
                .is_some_and(|detail| detail.contains(UNKNOWN_KIND)),
        "{build} lists the stalled cleanups as {:?}",
        inspect.stalled
    );
    Ok(())
}

#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just phase-a` runs it"]
fn retention_delivery_rollback() -> Result<()> {
    let leg = Leg::start("retention_delivery_rollback")?;

    let postgres = Case::postgres_database("retention-pg", &leg.services, &leg.scratch)?;
    // N opens the store first, so the fleet epoch it records is N's.
    Operator::for_case(&postgres, LASHCTL_N_ENV)?.run("migrate", None)?;
    leg.builds.n.probe(&postgres, None)?;
    let operator_next = Operator::for_case(&postgres, LASHCTL_NEXT_ENV)?;
    operator_next.run("migrate", None)?;
    let evidence = roll(&leg, &postgres, Some(&operator_next)).context("PostgreSQL")?;
    record(&leg, "postgres.json", &evidence)?;

    let sqlite = Case::sqlite("retention-sqlite", &leg.services, &leg.scratch)?;
    leg.builds.n.probe(&sqlite, None)?;
    leg.builds.next.probe(&sqlite, None)?;
    let evidence = roll(&leg, &sqlite, None).context("SQLite")?;
    record(&leg, "sqlite.json", &evidence)?;
    Ok(())
}
