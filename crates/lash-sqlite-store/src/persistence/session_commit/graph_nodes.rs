//! The commit-transaction helpers that write the session graph's node rows
//! and release the turn-input claims a cancelled turn withheld.

use super::*;

/// Release the turn-input claims a cancelled turn withheld from its terminal
/// checkpoint (FIG-3531), each under its own fence, inside the commit
/// transaction.
///
/// Each row returns to the open spelling its ingress carries —
/// `pending_active` for the active-turn rows a terminal checkpoint claims — so
/// the cancellation's disposition, which runs next, settles and records it
/// exactly as it does an unclaimed row. A claim this turn no longer holds
/// matches no row and is left to its new holder.
pub(super) fn release_undelivered_turn_input_claims_conn(
    tx: &rusqlite::Connection,
    claims: &[lash_core_execution::TurnInputClaim],
) -> Result<(), StoreError> {
    let sql = crate::turn_ingress::turn_ingress_sql();
    for claim in claims {
        tx.execute(
            sql.pending_inputs_sqlite.abandon_claim.sql(),
            params![
                claim.session_id.as_str(),
                claim.claim_id.as_str(),
                claim.lease_token,
                lash_core_execution::runtime::TurnInputStateKind::PendingActive.as_str(),
                lash_core_execution::runtime::TurnInputStateKind::DeferredNextTurn.as_str(),
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

/// The subset of `nodes` whose ids already occupy a `graph_nodes` row.
///
/// Asked as one statement per commit rather than one per node: the planner
/// needs the whole occupied set before it decides anything, so walking the
/// nodes one query at a time bought nothing and cost a round trip per node.
/// The id list is bound as a single JSON array, the same idiom the checkpoint
/// ref batches use, so the scalar-parameter ceiling is never in play.
pub(super) fn occupied_node_ids_conn(
    tx: &rusqlite::Connection,
    nodes: &[lash_core_execution::SessionNodeRecord],
) -> Result<std::collections::HashSet<lash_core_execution::NodeId>, StoreError> {
    let mut occupied = std::collections::HashSet::new();
    if nodes.is_empty() {
        return Ok(occupied);
    }
    let node_ids = nodes
        .iter()
        .map(|node| node.node_id.as_str())
        .collect::<Vec<_>>();
    for chunk in node_ids.chunks(OCCUPIED_NODE_ID_CHUNK_SIZE) {
        let encoded = serde_json::to_string(chunk).map_err(|error| {
            StoreError::Backend(format!("failed to encode commit node id batch: {error}"))
        })?;
        let mut statement = tx
            .prepare(session_sql().graph_sqlite.select_occupied.sql())
            .map_err(sqlite_error)?;
        let rows = statement
            .query_map(params![encoded], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?;
        for node_id in rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)? {
            occupied.insert(lash_core_execution::NodeId::from(node_id));
        }
    }
    Ok(occupied)
}

/// One JSON-array bind per commit keeps the encoded id list around a MiB while
/// staying far above any realistic per-commit node count.
const OCCUPIED_NODE_ID_CHUNK_SIZE: usize = 16_384;

/// Asked as one multi-row `INSERT` rather than one statement per node: the rows
/// are already known in full before any of them is written, and they all land or
/// none of them do regardless, so a statement per node bought no atomicity — it
/// bought a round trip per node.
///
/// A constraint violation is where the batch would lose something real. The
/// per-node errors name the colliding generation or node id, and SQLite reports
/// only that the batch failed, not which row failed it. So a failed batch is
/// replayed one node at a time to find the offender and raise exactly the error
/// the loop used to raise. That replay runs only on the failing path, where a
/// commit is being refused anyway.
pub(super) fn insert_graph_nodes_conn(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    nodes: &[lash_core_execution::SessionNodeRecord],
    plan: &lash_core_execution::store::RuntimeCommitPlan<'_>,
) -> Result<(), StoreError> {
    for (nodes, facts) in nodes.chunks(GRAPH_NODE_INSERT_CHUNK_SIZE).zip(
        plan.planned_node_facts()
            .chunks(GRAPH_NODE_INSERT_CHUNK_SIZE),
    ) {
        let mut rows = Vec::with_capacity(nodes.len());
        for (node, facts) in nodes.iter().zip(facts) {
            let node_json = node
                .encode_storage_body(plan.fleet_format())
                .map_err(|err| {
                    StoreError::Backend(format!("failed to encode graph node body: {err}"))
                })?;
            let generation = i64::try_from(facts.generation).map_err(|_| {
                StoreError::Backend("node generation does not fit SQLite INTEGER".to_string())
            })?;
            rows.push(serde_json::json!([
                session_id.as_str(),
                node.node_id.as_str(),
                node.parent_node_id.as_deref(),
                generation,
                facts.frame_node_id.as_str(),
                node_json,
            ]));
        }
        let encoded = serde_json::to_string(&rows).map_err(|error| {
            StoreError::Backend(format!("failed to encode commit node batch: {error}"))
        })?;
        if tx
            .execute(
                session_sql().graph_sqlite.insert_batch.sql(),
                params![encoded],
            )
            .is_err()
        {
            insert_graph_nodes_one_at_a_time(tx, session_id, nodes, facts, plan)?;
        }
    }
    Ok(())
}

/// The batch rides as one JSON array bound to a single parameter, so SQLite's
/// 32,766-parameter ceiling is not in play at all; the chunk bounds the encoded
/// array's size instead, and sits far above any per-commit node count, so the
/// chunking never runs in practice.
const GRAPH_NODE_INSERT_CHUNK_SIZE: usize = 512;

/// Replay a failed node batch row by row so the refusal names the offending row.
///
/// Reached only after the batch has already failed and the transaction is headed
/// for a rollback, so the extra statements cost nothing a successful commit pays.
fn insert_graph_nodes_one_at_a_time(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    nodes: &[lash_core_execution::SessionNodeRecord],
    facts: &[lash_core_execution::store::PlannedNodeFacts],
    plan: &lash_core_execution::store::RuntimeCommitPlan<'_>,
) -> Result<(), StoreError> {
    for (node, facts) in nodes.iter().zip(facts) {
        let node_json = node
            .encode_storage_body(plan.fleet_format())
            .map_err(|err| {
                StoreError::Backend(format!("failed to encode graph node body: {err}"))
            })?;
        tx.execute(
            session_sql().graph.insert.sql(),
            params![
                session_id.as_str(),
                node.node_id.as_str(),
                node.parent_node_id.as_deref(),
                i64::try_from(facts.generation).map_err(|_| StoreError::Backend(
                    "node generation does not fit SQLite INTEGER".to_string()
                ))?,
                facts.frame_node_id.as_str(),
                node_json
            ],
        )
        .map_err(|error| {
            sqlite_graph_node_insert_error(error, session_id, facts.generation, &node.node_id)
        })?;
    }
    Ok(())
}
