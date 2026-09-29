//! `history_after_finalize` (ADR 0115 §6): after finalize, N+1 still reads
//! the history N wrote, through the permanent floor and not through
//! `{F, newest}`.
//!
//! N answers a turn, so the session's graph nodes carry N's node-body
//! generation. The fleet rolls to N+1, drains N's generation and finalizes
//! (the synthetic finalize moves the recorded epoch; `lashctl finalize` is
//! FIG-3800 B's). At the new epoch N+1 pins no writer, so its read window
//! for the node body is `{F's version, newest}` = `{N+1's, N+1's}`: N's
//! generation is outside it, and only the node body's history floor, lifted
//! by its permanent upcaster, admits it. N is refused at open. N+1 reads
//! the session and answers a turn on it. N's nodes are still stored byte
//! for byte at N's generation, and the nodes N+1 appends carry its own.

use anyhow::{Context, Result, ensure};
use lash_core_store::compat::CompatRefusal;
use lash_upgrade_harness::harness::{Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, Operator, block_on};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::served_by;
use serde::Serialize;
use sqlx::{Connection, PgConnection, Row};

use crate::support::{Leg, quiesce, record};

/// One stored graph node: its id, its body's generation and its body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct StoredNode {
    node_id: String,
    schema_version: i64,
    node_json: String,
}

fn stored_nodes(case: &Case, session: &str) -> Result<Vec<StoredNode>> {
    let url = case
        .postgres_url()
        .context("history_after_finalize runs on PostgreSQL")?
        .to_owned();
    let session = session.to_owned();
    block_on(async move {
        let mut pg = PgConnection::connect(&url).await?;
        let nodes = sqlx::query(
            "SELECT node_id, (node_json::jsonb ->> 'schema_version')::bigint, node_json
             FROM lash_graph_nodes WHERE session_id = $1 ORDER BY generation",
        )
        .bind(&session)
        .fetch_all(&mut pg)
        .await?
        .iter()
        .map(|row| {
            Ok(StoredNode {
                node_id: row.try_get(0)?,
                schema_version: row.try_get(1)?,
                node_json: row.try_get(2)?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
        pg.close().await.ok();
        Ok(nodes)
    })
}

#[derive(Serialize)]
struct Evidence {
    written_by_n: Vec<StoredNode>,
    after_finalize: Vec<StoredNode>,
}

#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just phase-a` runs it"]
fn history_after_finalize() -> Result<()> {
    let leg = Leg::start("history_after_finalize")?;
    let (n, next) = (&leg.builds.n, &leg.builds.next);
    let case = Case::postgres_database("history", &leg.services, &leg.scratch)?;
    let operator_n = Operator::for_case(&case, LASHCTL_N_ENV)?;
    let operator_next = Operator::for_case(&case, LASHCTL_NEXT_ENV)?;
    operator_n.run("migrate", None)?;

    // N writes the history.
    let n_node = n.serve(&case)?;
    let n_generation = n_node.generation()?.to_owned();
    let session = case.session_id("history");
    let answered = n.turn(&case, &session, "history N writes")?;
    ensure!(
        answered.reply.as_deref() == Some(served_by(BuildLabel::N, &n_generation).as_str()),
        "N did not answer: {answered:?}"
    );
    let written_by_n = stored_nodes(&case, &session)?;
    let n_version = written_by_n
        .first()
        .map(|node| node.schema_version)
        .context("N's turn stored no graph node")?;
    ensure!(
        written_by_n
            .iter()
            .all(|node| node.schema_version == n_version),
        "N wrote mixed node bodies: {written_by_n:?}"
    );

    // Roll to N+1, drain N's generation and finalize.
    operator_next.run("migrate", None)?;
    let next_rolled = next.serve(&case)?;
    operator_next.run("drain", Some(&n_generation))?;
    quiesce(&case, &session)?;
    n_node.stop()?;
    let status = operator_next.run("drain-status", Some(&n_generation))?;
    ensure!(status["drained"] == true, "N did not drain: {status}");
    operator_next.run("end-drain", Some(&n_generation))?;
    case.finalize_postgres(2)?;
    next_rolled.stop()?;

    // N is fenced out of the finalized fleet.
    let refusal = n.probe_refusal(&case)?;
    ensure!(
        matches!(
            refusal,
            CompatRefusal::FleetOutsideWritable { recorded: 2, .. }
        ),
        "N opened the finalized store with {refusal:?}"
    );

    // N+1, at the new epoch, reads N's history and extends it.
    let read = next.probe(&case, Some(&session))?;
    ensure!(
        read.session_present == Some(true),
        "N+1 did not read N's session after finalize: {read:?}"
    );
    let next_final = next.serve(&case)?;
    let next_generation = next_final.generation()?.to_owned();
    let answered = next.turn(&case, &session, "history N+1 extends after finalize")?;
    ensure!(
        answered.reply.as_deref() == Some(served_by(BuildLabel::Next, &next_generation).as_str()),
        "N+1 did not answer on N's history: {answered:?}"
    );
    quiesce(&case, &session)?;
    next_final.stop()?;

    // N's nodes are unchanged; N+1's carry its own generation.
    let after_finalize = stored_nodes(&case, &session)?;
    ensure!(
        after_finalize.starts_with(&written_by_n),
        "N's history was rewritten after finalize: {written_by_n:?} -> {after_finalize:?}"
    );
    let appended = &after_finalize[written_by_n.len()..];
    ensure!(
        !appended.is_empty()
            && appended
                .iter()
                .all(|node| node.schema_version == n_version + 1),
        "N+1 appended {appended:?} after finalize, not bodies at {}",
        n_version + 1
    );
    record(
        &leg,
        "history.json",
        &Evidence {
            written_by_n,
            after_finalize,
        },
    )
}
