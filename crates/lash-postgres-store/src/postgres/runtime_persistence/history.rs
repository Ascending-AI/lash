use super::*;
use lash_core_execution::runtime::usage::{
    SessionUsageTotals, UnreportedUsageAttempt, UsageTotalRow,
};
use lash_core_execution::store::{
    AnchorUnavailable, FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor, HistoryBudget,
    HistoryCursor, HistoryNode, HistoryPage, HistoryStop, LineageStamp, SessionHistoryStore,
    SessionWindowRead, UsageLedgerCursor, UsageLedgerPage, UsageLedgerRow, WindowSelector,
};
use std::num::NonZeroU32;
use std::sync::atomic::Ordering;

type PgTx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

fn corrupt(kind: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: kind,
        message: message.into(),
    }
}

async fn read_tx(store: &PostgresStore) -> Result<PgTx<'_>, StoreError> {
    let mut tx = store.pool.begin().await.map_err(store_sqlx_error)?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .begin_repeatable_read_read_only
            .sql(),
    )
    .execute(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(tx)
}

async fn check_live(tx: &mut PgTx<'_>, session_id: &SessionId) -> Result<(), StoreError> {
    lash_core_execution::store::validate_session_id(session_id)?;
    let deleted: bool = sqlx::query_scalar(
        crate::session_sql::session_sql()
            .deleted_postgres
            .exists
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if deleted {
        Err(StoreError::SessionDeleted {
            session_id: session_id.clone(),
        })
    } else {
        Ok(())
    }
}

// Every row resolved here passes the same owner-or-individual-fork-ceiling
// predicate used by the range reads. A direct node-id lookup without it could
// turn another session's node into this session's window base.
const READABLE_NODE: &str = "SELECT node.node_id, node.parent_node_id, node.generation,
    node.frame_node_id, node.body_bytes, node.session_id, node.tombstoned
    FROM lash_graph_nodes AS node WHERE node.node_id = $1 AND
    (node.session_id = $2 OR EXISTS (
      SELECT 1 FROM lash_fork_lineage AS lineage
      WHERE lineage.session_id = $2 AND lineage.ancestor_session_id = node.session_id
        AND node.generation <= lineage.fork_generation))";

const WINDOW_ROWS: &str = "WITH readable_sessions AS (
    SELECT $1::text AS session_id, NULL::bigint AS generation_ceiling
    UNION ALL SELECT ancestor_session_id, fork_generation FROM lash_fork_lineage
      WHERE session_id = $1)
    SELECT node.node_id, node.parent_node_id, node.node_json, node.generation,
           node.frame_node_id, node.body_bytes
    FROM readable_sessions AS readable
    JOIN lash_graph_nodes AS node ON node.session_id = readable.session_id
      AND node.generation BETWEEN $2 AND $3
      AND (readable.generation_ceiling IS NULL
           OR node.generation <= readable.generation_ceiling)
    WHERE node.tombstoned = FALSE ORDER BY node.generation";

const PAGE_HEADERS: &str = "WITH readable_sessions AS (
    SELECT $1::text AS session_id, NULL::bigint AS generation_ceiling
    UNION ALL SELECT ancestor_session_id, fork_generation FROM lash_fork_lineage
      WHERE session_id = $1)
    SELECT node.node_id, node.parent_node_id, node.generation,
           node.frame_node_id, node.body_bytes, node.session_id
    FROM readable_sessions AS readable
    JOIN lash_graph_nodes AS node ON node.session_id = readable.session_id
      AND node.generation <= $2
      AND (readable.generation_ceiling IS NULL
           OR node.generation <= readable.generation_ceiling)
    WHERE node.tombstoned = FALSE ORDER BY node.generation DESC LIMIT $3";

async fn readable_row(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    node_id: &str,
) -> Result<Option<PgRow>, StoreError> {
    sqlx::query(READABLE_NODE)
        .bind(node_id)
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)
}

async fn lineage_stamp(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<LineageStamp, StoreError> {
    let rows = sqlx::query(
        crate::session_sql::session_sql()
            .lineage
            .select_for_stamp
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let pairs = rows
        .into_iter()
        .map(|row| {
            let id: String = row.get(0);
            let generation: i64 = row.get(1);
            Ok((
                SessionId::from(id),
                u64_from_sql("ForkLineage", "fork_generation", generation)?,
            ))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(LineageStamp::of_lineage(
        pairs.iter().map(|(id, generation)| (id, *generation)),
    ))
}

async fn missing_anchor(
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
    node_id: &str,
) -> Result<StoreError, StoreError> {
    let tombstoned: Option<bool> =
        sqlx::query_scalar("SELECT tombstoned FROM lash_graph_nodes WHERE node_id = $1")
            .bind(node_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    Ok(StoreError::HistoryAnchorUnavailable {
        session_id: session_id.clone(),
        node_id: node_id.into(),
        reason: if tombstoned == Some(true) {
            AnchorUnavailable::Tombstoned
        } else {
            AnchorUnavailable::NotReadable
        },
    })
}

#[async_trait::async_trait]
impl SessionHistoryStore for PostgresStore {
    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError> {
        let mut tx = read_tx(self).await?;
        check_live(&mut tx, session_id).await?;
        read_session_state_version_tx(&mut tx, session_id, false, self.fleet_format).await?;
        let Some(meta) =
            load_session_head_meta_tx(&mut tx, session_id, false, self.fleet_format).await?
        else {
            return match selector {
                WindowSelector::Current => Ok(None),
                WindowSelector::Admitted(base) => Err(StoreError::TurnBaseNotRetained {
                    revision: base.revision,
                }),
            };
        };
        let admitted = matches!(selector, WindowSelector::Admitted(_));
        let (revision, leaf, checkpoint_ref, pending_follow_on) = match selector {
            WindowSelector::Current => (
                meta.head_revision,
                meta.leaf_node_id.clone(),
                meta.checkpoint_ref.clone(),
                meta.pending_follow_on.clone(),
            ),
            WindowSelector::Admitted(base) => (base.revision, base.leaf, base.checkpoint, None),
        };
        let checkpoint = match checkpoint_ref.as_ref() {
            Some(reference) => {
                let checkpoint = get_checkpoint_tx(&mut tx, reference, self.fleet_format).await?;
                if admitted && checkpoint.is_none() {
                    return Err(StoreError::TurnBaseNotRetained { revision });
                }
                checkpoint
            }
            None => None,
        };
        let mut config = meta.config.clone();
        let window = if let Some(leaf) = leaf {
            let leaf_row = readable_row(&mut tx, session_id, leaf.as_str())
                .await?
                .ok_or_else(|| {
                    if admitted {
                        StoreError::TurnBaseNotRetained { revision }
                    } else {
                        corrupt("SessionGraph", format!("leaf `{leaf}` is not readable"))
                    }
                })?;
            if leaf_row.get::<bool, _>("tombstoned") {
                return Err(if admitted {
                    StoreError::TurnBaseNotRetained { revision }
                } else {
                    corrupt("SessionGraph", format!("leaf `{leaf}` is tombstoned"))
                });
            }
            let leaf_generation: i64 = leaf_row.get("generation");
            let frame_id: String = leaf_row.get("frame_node_id");
            if !admitted
                && meta
                    .current_frame_node_id
                    .as_ref()
                    .map(|frame| frame.as_str())
                    != Some(frame_id.as_str())
            {
                return Err(StoreError::CurrentFrameNodeMismatch {
                    claimed: meta
                        .current_frame_node_id
                        .as_ref()
                        .map(|frame| frame.as_str().to_owned()),
                    derived: Some(frame_id),
                });
            }
            let frame_row = readable_row(&mut tx, session_id, &frame_id)
                .await?
                .ok_or_else(|| {
                    corrupt(
                        "SessionGraph",
                        format!("frame `{frame_id}` is not readable"),
                    )
                })?;
            if frame_row.get::<bool, _>("tombstoned") {
                return Err(corrupt(
                    "SessionGraph",
                    format!("frame `{frame_id}` is tombstoned"),
                ));
            }
            let first_generation: i64 = frame_row.get("generation");
            if first_generation < 0 || first_generation > leaf_generation {
                return Err(corrupt(
                    "SessionGraph",
                    "frame generation exceeds leaf generation",
                ));
            }
            let rows = sqlx::query(WINDOW_ROWS)
                .bind(session_id.as_str())
                .bind(first_generation)
                .bind(leaf_generation)
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?;
            let mut nodes = Vec::with_capacity(rows.len());
            for (offset, row) in rows.into_iter().enumerate() {
                let node_id: String = row.get("node_id");
                let parent: Option<String> = row.get("parent_node_id");
                let body: String = row.get("node_json");
                let body_bytes: i64 = row.get("body_bytes");
                if body_bytes < 0 || usize::try_from(body_bytes).ok() != Some(body.len()) {
                    return Err(corrupt(
                        "SessionGraph",
                        format!("body_bytes differs at `{node_id}`"),
                    ));
                }
                let generation: i64 = row.get("generation");
                if generation != first_generation + i64::try_from(offset).unwrap_or(i64::MAX) {
                    return Err(corrupt(
                        "SessionGraph",
                        format!("generation gap at `{node_id}`"),
                    ));
                }
                if row.get::<String, _>("frame_node_id") != frame_id {
                    return Err(StoreError::InvalidWindowAnchor {
                        frame_node_id: frame_id.clone().into(),
                        violation:
                            lash_core_execution::store::WindowAnchorViolation::ForeignFramePointer,
                    });
                }
                let node = SessionNodeRecord::decode_storage_body_for_fleet(
                    node_id,
                    parent,
                    &body,
                    self.fleet_format,
                )
                .map_err(|error| corrupt("SessionGraph node", error.to_string()))?;
                #[cfg(any(test, feature = "testing"))]
                self.decoded_graph_node_bodies
                    .fetch_add(1, Ordering::Relaxed);
                nodes.push(node);
            }
            let external_parent: Option<String> = frame_row.get("parent_node_id");
            let previous_frame_node_id = match external_parent.as_deref() {
                Some(parent) => {
                    let row = readable_row(&mut tx, session_id, parent)
                        .await?
                        .ok_or_else(|| corrupt("SessionGraph", "frame parent is missing"))?;
                    if row.get::<bool, _>("tombstoned")
                        || row.get::<i64, _>("generation") != first_generation - 1
                    {
                        return Err(corrupt(
                            "SessionGraph",
                            "frame parent is not the preceding live node",
                        ));
                    }
                    lash_core_execution::FrameNodeId::new(row.get::<String, _>("frame_node_id"))
                        .map(Some)
                        .map_err(|error| corrupt("SessionGraph", error.to_string()))?
                }
                None => None,
            };
            let frame_node_id = lash_core_execution::FrameNodeId::new(frame_id.clone())
                .map_err(|error| corrupt("SessionGraph", error.to_string()))?;
            let anchor = lash_core_execution::session_graph::WindowAnchor {
                frame_node_id,
                generation: u64_from_sql("SessionGraph", "generation", first_generation)?,
                external_parent: external_parent.map(Into::into),
                previous_frame_node_id,
            };
            let graph = lash_core_execution::SessionGraph::from_window(nodes, leaf, anchor)?;
            if meta
                .current_frame_node_id
                .as_ref()
                .map(|frame| frame.as_str())
                != Some(frame_id.as_str())
            {
                config = graph
                    .nodes
                    .first()
                    .and_then(|node| node.frame_config())
                    .ok_or_else(|| corrupt("SessionGraph", "frame has no config"))?;
            }
            graph
        } else {
            if !admitted && meta.current_frame_node_id.is_some() {
                return Err(StoreError::CurrentFrameNodeMismatch {
                    claimed: meta
                        .current_frame_node_id
                        .as_ref()
                        .map(|frame| frame.as_str().to_owned()),
                    derived: None,
                });
            }
            lash_core_execution::SessionGraph::default()
        };
        let usage = load_usage_totals_tx(self, &mut tx, session_id).await?;
        let read = SessionWindowRead::new(
            session_id.clone(),
            revision,
            config,
            pending_follow_on,
            window,
            checkpoint_ref,
            checkpoint,
            usage,
        )?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(Some(read))
    }

    async fn load_ancestors(
        &self,
        session_id: &SessionId,
        anchor: HistoryAnchor,
        budget: HistoryBudget,
    ) -> Result<HistoryPage, StoreError> {
        let mut tx = read_tx(self).await?;
        if let HistoryAnchor::Cursor(cursor) = &anchor {
            cursor.check_session(session_id)?;
        }
        check_live(&mut tx, session_id).await?;
        let lineage = lineage_stamp(&mut tx, session_id).await?;
        let (pinned_leaf, start, expected_generation) = match anchor {
            HistoryAnchor::Head => {
                let meta = load_session_head_meta_tx(&mut tx, session_id, false, self.fleet_format)
                    .await?
                    .ok_or_else(|| StoreError::SessionNotFound {
                        session_id: session_id.clone(),
                    })?;
                let Some(leaf) = meta.leaf_node_id else {
                    return Ok(HistoryPage {
                        pinned_leaf: None,
                        nodes: Vec::new(),
                        stop: HistoryStop::Root,
                        next: None,
                    });
                };
                (leaf.clone(), leaf, None)
            }
            HistoryAnchor::Node(node) => (node.clone(), node, None),
            HistoryAnchor::Cursor(cursor) => {
                if cursor.lineage() != &lineage {
                    return Err(StoreError::HistoryCursorLineageChanged {
                        session_id: session_id.clone(),
                    });
                }
                (
                    cursor.pinned_leaf().clone(),
                    cursor.next_node_id().clone(),
                    Some(cursor.next_generation()),
                )
            }
        };
        let first = match readable_row(&mut tx, session_id, start.as_str()).await? {
            Some(row) => row,
            None => return Err(missing_anchor(&mut tx, session_id, start.as_str()).await?),
        };
        if first.get::<bool, _>("tombstoned") {
            return Err(missing_anchor(&mut tx, session_id, start.as_str()).await?);
        }
        let start_generation = u64_from_sql("SessionGraph", "generation", first.get("generation"))?;
        if expected_generation.is_some_and(|expected| expected != start_generation) {
            return Err(corrupt("SessionGraph", "history cursor generation changed"));
        }
        let rows = sqlx::query(PAGE_HEADERS)
            .bind(session_id.as_str())
            .bind(
                i64::try_from(start_generation)
                    .map_err(|_| corrupt("SessionGraph", "generation exceeds BIGINT"))?,
            )
            .bind(i64::from(budget.max_nodes.get()) + 1)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?;
        let mut headers: Vec<PageHeader> = Vec::new();
        let mut bytes = 0_u64;
        let mut stopped_by_bytes = false;
        let mut found_next = false;
        for row in rows {
            let id: String = row.get("node_id");
            let generation = u64_from_sql("SessionGraph", "generation", row.get("generation"))?;
            if let Some(previous) = headers.last() {
                if previous.parent.as_deref() != Some(id.as_str())
                    || previous.generation != generation + 1
                {
                    return Err(corrupt("SessionGraph", format!("history gap at `{id}`")));
                }
            } else {
                if id != start.as_str() || generation != start_generation {
                    return Err(corrupt(
                        "SessionGraph",
                        "history page does not begin at anchor",
                    ));
                }
            }
            let size = u64_from_sql("SessionGraph", "body_bytes", row.get("body_bytes"))?;
            if size > budget.max_bytes.get() && headers.is_empty() {
                return Err(StoreError::HistoryNodeTooLarge {
                    node_id: id.into(),
                    required_bytes: size,
                    max_bytes: budget.max_bytes.get(),
                });
            }
            if headers.len() == budget.max_nodes.get() as usize {
                found_next = true;
                break;
            }
            if bytes
                .checked_add(size)
                .is_none_or(|sum| sum > budget.max_bytes.get())
            {
                stopped_by_bytes = true;
                found_next = true;
                break;
            }
            bytes += size;
            headers.push(PageHeader {
                id,
                parent: row.get("parent_node_id"),
                generation,
                frame: row.get("frame_node_id"),
                size,
                owner: row.get("session_id"),
            });
        }
        let last = headers
            .last()
            .ok_or_else(|| corrupt("SessionGraph", "history page has no anchor row"))?;
        if last.generation > 0 && !found_next {
            return Err(corrupt(
                "SessionGraph",
                "history chain ends above generation zero",
            ));
        }
        let stop = if last.generation == 0 {
            HistoryStop::Root
        } else if stopped_by_bytes {
            HistoryStop::ByteBudget
        } else {
            HistoryStop::NodeBudget
        };
        let next = if stop == HistoryStop::Root {
            None
        } else {
            let parent = last
                .parent
                .as_ref()
                .ok_or_else(|| corrupt("SessionGraph", "non-root history row has no parent"))?;
            Some(HistoryCursor::new(
                session_id.clone(),
                pinned_leaf.clone(),
                lineage,
                parent.clone().into(),
                last.generation - 1,
            ))
        };
        let ids = headers
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>();
        let bodies = sqlx::query("SELECT node_id, node_json FROM lash_graph_nodes WHERE node_id = ANY($1) ORDER BY generation DESC")
            .bind(&ids).fetch_all(&mut *tx).await.map_err(store_sqlx_error)?;
        if bodies.len() != headers.len() {
            return Err(corrupt("SessionGraph", "history body row is missing"));
        }
        let mut nodes = Vec::with_capacity(headers.len());
        for (header, body) in headers.into_iter().zip(bodies) {
            let id: String = body.get("node_id");
            if id != header.id {
                return Err(corrupt(
                    "SessionGraph",
                    "history body order differs from headers",
                ));
            }
            let json: String = body.get("node_json");
            if json.len() as u64 != header.size {
                return Err(corrupt(
                    "SessionGraph",
                    format!("body_bytes differs at `{id}`"),
                ));
            }
            let record = SessionNodeRecord::decode_storage_body_for_fleet(
                id,
                header.parent,
                &json,
                self.fleet_format,
            )
            .map_err(|error| corrupt("SessionGraph node", error.to_string()))?;
            #[cfg(any(test, feature = "testing"))]
            self.decoded_graph_node_bodies
                .fetch_add(1, Ordering::Relaxed);
            nodes.push(HistoryNode {
                generation: header.generation,
                owner_session_id: SessionId::from(header.owner),
                frame_node_id: lash_core_execution::FrameNodeId::new(header.frame)
                    .map_err(|error| corrupt("SessionGraph", error.to_string()))?,
                body_bytes: header.size,
                record,
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(HistoryPage {
            pinned_leaf: Some(pinned_leaf),
            nodes,
            stop,
            next,
        })
    }

    async fn contains_active_ancestor(
        &self,
        session_id: &SessionId,
        node_id: &lash_core_execution::NodeId,
    ) -> Result<bool, StoreError> {
        let mut tx = read_tx(self).await?;
        check_live(&mut tx, session_id).await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM lash_sessions AS head
             JOIN lash_graph_nodes AS leaf ON leaf.node_id = head.leaf_node_id
             JOIN lash_graph_nodes AS node ON node.node_id = $2
             WHERE head.session_id = $1 AND NOT leaf.tombstoned AND NOT node.tombstoned
               AND node.generation <= leaf.generation
               AND (node.session_id = $1 OR EXISTS (
                 SELECT 1 FROM lash_fork_lineage AS lineage WHERE lineage.session_id = $1
                   AND lineage.ancestor_session_id = node.session_id
                   AND node.generation <= lineage.fork_generation)))",
        )
        .bind(session_id.as_str())
        .bind(node_id.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(exists)
    }

    async fn load_usage_totals(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionUsageTotals, StoreError> {
        let mut tx = read_tx(self).await?;
        check_live(&mut tx, session_id).await?;
        let totals = load_usage_totals_tx(self, &mut tx, session_id).await?;
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(totals)
    }

    async fn load_usage_ledger_page(
        &self,
        session_id: &SessionId,
        after: Option<&UsageLedgerCursor>,
        limit: NonZeroU32,
    ) -> Result<UsageLedgerPage, StoreError> {
        if let Some(cursor) = after {
            cursor.check_session(session_id)?;
        }
        let mut tx = read_tx(self).await?;
        check_live(&mut tx, session_id).await?;
        let mut rows = sqlx::query(
            "SELECT seq, operation_storage_key, source, model, input_tokens, output_tokens,
                    cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens,
                    reconciled_call_id, reconciled_attempt_ordinal
             FROM lash_usage_deltas WHERE session_id = $1 AND seq > $2
             ORDER BY seq LIMIT $3",
        )
        .bind(session_id.as_str())
        .bind(
            after
                .map(|cursor| i64::try_from(cursor.after_seq()).unwrap_or(i64::MAX))
                .unwrap_or(0),
        )
        .bind(i64::from(limit.get()) + 1)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let more = rows.len() > limit.get() as usize;
        if more {
            rows.pop();
        }
        let seqs = rows
            .iter()
            .map(|row| row.get::<i64, _>("seq"))
            .collect::<Vec<_>>();
        let holes = sqlx::query(
            "SELECT seq, call_id, attempt_ordinal, generation_id FROM lash_usage_delta_holes
             WHERE session_id = $1 AND seq = ANY($2) ORDER BY seq, call_id, attempt_ordinal",
        )
        .bind(session_id.as_str())
        .bind(&seqs)
        .fetch_all(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        let mut holes_by_seq = std::collections::HashMap::<
            i64,
            Vec<lash_core_execution::runtime::usage::UnreportedLedgerAttempt>,
        >::new();
        for row in holes {
            let seq: i64 = row.get("seq");
            holes_by_seq.entry(seq).or_default().push(
                lash_core_execution::runtime::usage::UnreportedLedgerAttempt {
                    call_id: row.get("call_id"),
                    attempt_ordinal: u32::try_from(row.get::<i64, _>("attempt_ordinal"))
                        .map_err(|_| corrupt("TokenLedgerEntry", "invalid attempt ordinal"))?,
                    generation_id: row.get("generation_id"),
                },
            );
        }
        let mut output = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.get("seq");
            let reconciled_call_id: Option<String> = row.get("reconciled_call_id");
            let ordinal: Option<i64> = row.get("reconciled_attempt_ordinal");
            let attempts = holes_by_seq.remove(&seq).unwrap_or_default();
            let disposition = match (reconciled_call_id, ordinal) {
                (Some(call_id), Some(ordinal)) if attempts.is_empty() => {
                    lash_core_execution::LedgerUsageDisposition::Reconciled {
                        call_id,
                        attempt_ordinal: u32::try_from(ordinal).map_err(|_| {
                            corrupt("TokenLedgerEntry", "invalid reconciled ordinal")
                        })?,
                    }
                }
                (None, None) if !attempts.is_empty() => {
                    lash_core_execution::LedgerUsageDisposition::unreported(attempts)
                }
                (None, None) => lash_core_execution::LedgerUsageDisposition::Reported,
                _ => {
                    return Err(corrupt(
                        "TokenLedgerEntry",
                        "reconciliation columns disagree with holes",
                    ));
                }
            };
            let entry = TokenLedgerEntry {
                source: row.get("source"),
                model: row.get("model"),
                usage: lash_core_execution::TokenUsage {
                    input_tokens: row.get("input_tokens"),
                    output_tokens: row.get("output_tokens"),
                    cache_read_input_tokens: row.get("cache_read_input_tokens"),
                    cache_write_input_tokens: row.get("cache_write_input_tokens"),
                    reasoning_output_tokens: row.get("reasoning_output_tokens"),
                },
                usage_disposition: disposition,
            };
            #[cfg(any(test, feature = "testing"))]
            self.decoded_usage_rows.fetch_add(1, Ordering::Relaxed);
            output.push(UsageLedgerRow {
                seq: u64_from_sql("TokenLedgerEntry", "seq", seq)?,
                operation_storage_key: row.get("operation_storage_key"),
                entry,
            });
        }
        let next = if more {
            output
                .last()
                .map(|row| UsageLedgerCursor::new(session_id.clone(), row.seq))
        } else {
            None
        };
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(UsageLedgerPage { rows: output, next })
    }

    async fn load_failure_evidence_page(
        &self,
        session_id: &SessionId,
        after: Option<&FailureEvidenceCursor>,
        limit: NonZeroU32,
    ) -> Result<FailureEvidencePage, StoreError> {
        if let Some(cursor) = after {
            cursor.check_session(session_id)?;
        }
        let mut tx = read_tx(self).await?;
        check_live(&mut tx, session_id).await?;
        let statement = crate::session_sql::session_sql();
        let mut rows = match after {
            Some(cursor) => sqlx::query(
                statement
                    .turn_commits
                    .select_failure_settlements_after
                    .sql(),
            )
            .bind(session_id.as_str())
            .bind(i64::try_from(cursor.committed_at_ms()).unwrap_or(i64::MAX))
            .bind(cursor.turn_id().as_str())
            .bind(i64::from(limit.get()) + 1)
            .fetch_all(&mut *tx)
            .await
            .map_err(store_sqlx_error)?,
            None => sqlx::query(statement.turn_commits.select_failure_settlements.sql())
                .bind(session_id.as_str())
                .bind(i64::from(limit.get()) + 1)
                .fetch_all(&mut *tx)
                .await
                .map_err(store_sqlx_error)?,
        };
        let more = rows.len() > limit.get() as usize;
        if more {
            rows.pop();
        }
        let mut settlements = Vec::with_capacity(rows.len());
        let mut last = None;
        for row in rows {
            let committed_at_ms: i64 = row.get("committed_at_ms");
            let turn_id: String = row.get("turn_id");
            let result_json: String = row.get("result_json");
            let outcome_code: Option<String> = row.get("outcome_code");
            let receipt = lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                session_id,
                &turn_id,
                &result_json,
                self.fleet_format,
            )?;
            lash_core_execution::store::validate_turn_commit_outcome_code(
                &receipt,
                outcome_code.as_deref(),
            )?;
            if receipt.failure_evidence.is_empty() {
                return Err(corrupt(
                    "RuntimeCommitReceipt",
                    "failure flag has no evidence",
                ));
            }
            #[cfg(any(test, feature = "testing"))]
            self.decoded_turn_receipts.fetch_add(1, Ordering::Relaxed);
            let committed_at_ms =
                u64_from_sql("RuntimeCommitReceipt", "committed_at_ms", committed_at_ms)?;
            last = Some(FailureEvidenceCursor::new(
                session_id.clone(),
                committed_at_ms,
                turn_id.clone().into(),
            ));
            settlements.push(lash_core_execution::TurnFailureSettlement {
                turn_id,
                evidence: receipt.failure_evidence,
            });
        }
        tx.commit().await.map_err(store_sqlx_error)?;
        Ok(FailureEvidencePage {
            settlements,
            next: if more { last } else { None },
        })
    }
}

struct PageHeader {
    id: String,
    parent: Option<String>,
    generation: u64,
    frame: String,
    size: u64,
    owner: String,
}

fn aggregate_counter(
    row: &PgRow,
    column: &'static str,
    source: &str,
    model: &str,
) -> Result<i64, StoreError> {
    let decimal: String = row.get(column);
    decimal
        .parse::<i64>()
        .map_err(|_| StoreError::TokenUsageAccountingOverflow {
            usage_source: source.to_owned(),
            model: model.to_owned(),
            counter: column,
        })
}

async fn load_usage_totals_tx(
    store: &PostgresStore,
    tx: &mut PgTx<'_>,
    session_id: &SessionId,
) -> Result<SessionUsageTotals, StoreError> {
    let rows = sqlx::query(
        "SELECT source, model,
           COALESCE(SUM(input_tokens), 0)::text AS input_tokens,
           COALESCE(SUM(output_tokens), 0)::text AS output_tokens,
           COALESCE(SUM(cache_read_input_tokens), 0)::text AS cache_read_input_tokens,
           COALESCE(SUM(cache_write_input_tokens), 0)::text AS cache_write_input_tokens,
           COALESCE(SUM(reasoning_output_tokens), 0)::text AS reasoning_output_tokens,
           COUNT(*) FILTER (WHERE reconciled_call_id IS NOT NULL) AS reconciled_attempts
         FROM lash_usage_deltas AS usage
         WHERE usage.session_id = $1
           AND (input_tokens <> 0 OR output_tokens <> 0
                OR cache_read_input_tokens <> 0 OR cache_write_input_tokens <> 0
                OR reasoning_output_tokens <> 0 OR reconciled_call_id IS NOT NULL
                OR EXISTS (SELECT 1 FROM lash_usage_delta_holes AS hole
                           WHERE hole.seq = usage.seq AND hole.session_id = $1))
         GROUP BY source, model ORDER BY source, model",
    )
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let mut totals = SessionUsageTotals::default();
    for row in rows {
        let source: String = row.get("source");
        let model: String = row.get("model");
        let usage = lash_core_execution::TokenUsage {
            input_tokens: aggregate_counter(&row, "input_tokens", &source, &model)?,
            output_tokens: aggregate_counter(&row, "output_tokens", &source, &model)?,
            cache_read_input_tokens: aggregate_counter(
                &row,
                "cache_read_input_tokens",
                &source,
                &model,
            )?,
            cache_write_input_tokens: aggregate_counter(
                &row,
                "cache_write_input_tokens",
                &source,
                &model,
            )?,
            reasoning_output_tokens: aggregate_counter(
                &row,
                "reasoning_output_tokens",
                &source,
                &model,
            )?,
        };
        usage
            .checked_total()
            .map_err(|overflow| StoreError::TokenUsageAccountingOverflow {
                usage_source: source.clone(),
                model: model.clone(),
                counter: overflow.counter(),
            })?;
        totals.rows.push(UsageTotalRow {
            source,
            model,
            usage,
            unreported_attempts: 0,
            reconciled_attempts: u64_from_sql(
                "TokenLedgerEntry",
                "reconciled_attempts",
                row.get("reconciled_attempts"),
            )?,
        });
    }
    let conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM lash_usage_delta_holes AS hole
         JOIN lash_usage_deltas AS usage ON usage.seq = hole.seq
         WHERE hole.session_id = $1 AND usage.session_id = $1
         GROUP BY hole.call_id, hole.attempt_ordinal
         HAVING COUNT(DISTINCT (usage.source, usage.model, hole.generation_id)) > 1)",
    )
    .bind(session_id.as_str())
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if conflict {
        return Err(corrupt(
            "TokenLedgerEntry",
            "hole attribution differs across usage rows",
        ));
    }
    let counts = sqlx::query(
        "SELECT usage.source, usage.model,
                COUNT(DISTINCT (hole.call_id, hole.attempt_ordinal)) AS hole_count
         FROM lash_usage_delta_holes AS hole
         JOIN lash_usage_deltas AS usage ON usage.seq = hole.seq
         WHERE hole.session_id = $1 AND usage.session_id = $1
         GROUP BY usage.source, usage.model",
    )
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for row in counts {
        let source: String = row.get("source");
        let model: String = row.get("model");
        let Some(total) = totals
            .rows
            .iter_mut()
            .find(|total| total.source == source && total.model == model)
        else {
            return Err(corrupt("TokenLedgerEntry", "hole has no usage aggregate"));
        };
        total.unreported_attempts =
            u64_from_sql("TokenLedgerEntry", "hole_count", row.get("hole_count"))?;
    }
    let outstanding = sqlx::query(
        "SELECT DISTINCT ON (hole.call_id, hole.attempt_ordinal)
                hole.call_id, hole.attempt_ordinal, usage.source, usage.model, hole.generation_id
         FROM lash_usage_delta_holes AS hole
         JOIN lash_usage_deltas AS usage ON usage.seq = hole.seq
         WHERE hole.session_id = $1 AND usage.session_id = $1
           AND NOT EXISTS (SELECT 1 FROM lash_usage_deltas AS correction
             WHERE correction.session_id = $1
               AND correction.reconciled_call_id = hole.call_id
               AND correction.reconciled_attempt_ordinal = hole.attempt_ordinal)
         ORDER BY hole.call_id, hole.attempt_ordinal, hole.seq",
    )
    .bind(session_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    for row in outstanding {
        totals.outstanding.push(UnreportedUsageAttempt {
            call_id: row.get("call_id"),
            attempt_ordinal: u32::try_from(row.get::<i64, _>("attempt_ordinal"))
                .map_err(|_| corrupt("TokenLedgerEntry", "invalid attempt ordinal"))?,
            source: row.get("source"),
            model: row.get("model"),
            generation_id: row.get("generation_id"),
        });
        #[cfg(any(test, feature = "testing"))]
        store.decoded_usage_holes.fetch_add(1, Ordering::Relaxed);
    }
    Ok(totals)
}
