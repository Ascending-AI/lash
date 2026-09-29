use super::*;
use lash_core_execution::runtime::{SessionUsageTotals, UnreportedUsageAttempt, UsageTotalRow};
use lash_core_execution::store::{
    AnchorUnavailable, FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor, HistoryBudget,
    HistoryCursor, HistoryNode, HistoryPage, HistoryStop, LineageStamp, SessionHistoryStore,
    SessionWindowRead, UsageLedgerCursor, UsageLedgerPage, UsageLedgerRow, WindowSelector,
};
use std::num::NonZeroU32;
use std::sync::atomic::Ordering;

fn corrupt(kind: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: kind,
        message: message.into(),
    }
}
fn nonnegative(kind: &'static str, field: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| corrupt(kind, format!("{field} is negative: {value}")))
}
fn live(conn: &Connection, session_id: &SessionId) -> Result<(), StoreError> {
    lash_core_execution::store::validate_session_id(session_id)?;
    if conn
        .query_row(
            session_sql::session_sql().deleted_sqlite.exists.sql(),
            params![session_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sqlite_error)?
        .is_some()
    {
        Err(StoreError::SessionDeleted {
            session_id: session_id.clone(),
        })
    } else {
        Ok(())
    }
}

#[derive(Clone)]
struct Header {
    id: String,
    parent: Option<String>,
    generation: i64,
    frame: String,
    bytes: i64,
    owner: String,
    tombstoned: bool,
}
fn header(row: &rusqlite::Row<'_>) -> rusqlite::Result<Header> {
    Ok(Header {
        id: row.get(0)?,
        parent: row.get(1)?,
        generation: row.get(2)?,
        frame: row.get(3)?,
        bytes: row.get(4)?,
        owner: row.get(5)?,
        tombstoned: row.get::<_, i64>(6)? != 0,
    })
}
fn visible_header(
    conn: &Connection,
    session: &SessionId,
    node: &str,
) -> Result<Option<Header>, StoreError> {
    conn.query_row(
        session_sql::session_sql().graph_sqlite.visible_header.sql(),
        params![node, session.as_str()],
        header,
    )
    .optional()
    .map_err(sqlite_error)
}
fn missing_anchor(
    conn: &Connection,
    session: &SessionId,
    node: &str,
) -> Result<StoreError, StoreError> {
    let tombstoned = visible_header(conn, session, node)?.is_some_and(|row| row.tombstoned);
    Ok(StoreError::HistoryAnchorUnavailable {
        session_id: session.clone(),
        node_id: node.into(),
        reason: if tombstoned {
            AnchorUnavailable::Tombstoned
        } else {
            AnchorUnavailable::NotReadable
        },
    })
}
fn lineage(conn: &Connection, session: &SessionId) -> Result<LineageStamp, StoreError> {
    let mut statement = conn
        .prepare_cached(session_sql::session_sql().lineage.select_for_stamp.sql())
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session.as_str()], |row| {
            Ok((
                SessionId::from(row.get::<_, String>(0)?),
                row.get::<_, i64>(1)?,
            ))
        })
        .map_err(sqlite_error)?;
    let pairs = rows
        .map(|row| {
            let (id, g) = row.map_err(sqlite_error)?;
            Ok((id, nonnegative("ForkLineage", "fork_generation", g)?))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(LineageStamp::of_lineage(
        pairs.iter().map(|(id, g)| (id, *g)),
    ))
}

fn window(
    conn: &Connection,
    session: &SessionId,
    selector: WindowSelector,
    fleet: lash_core_execution::FleetFormat,
    decoded: &AtomicU64,
    holes: &AtomicU64,
) -> Result<Option<SessionWindowRead>, StoreError> {
    live(conn, session)?;
    let Some(meta) = try_load_session_head_meta_from_conn(conn, session, fleet)? else {
        return match selector {
            WindowSelector::Current => Ok(None),
            WindowSelector::Admitted(base) => Err(StoreError::TurnBaseNotRetained {
                revision: base.revision,
            }),
        };
    };
    let admitted = matches!(selector, WindowSelector::Admitted(_));
    let (revision, leaf, checkpoint_ref, pending) = match selector {
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
            let value = SqliteStore::get_checkpoint_conn(conn, reference, fleet)?;
            if admitted && value.is_none() {
                return Err(StoreError::TurnBaseNotRetained { revision });
            }
            value
        }
        None => None,
    };
    let mut config = meta.config.clone();
    let graph = if let Some(leaf) = leaf {
        let last = visible_header(conn, session, leaf.as_str())?
            .filter(|h| !h.tombstoned)
            .ok_or_else(|| corrupt("SessionGraph", format!("leaf `{leaf}` is not readable")))?;
        if !admitted
            && meta.current_frame_node_id.as_ref().map(|id| id.as_str())
                != Some(last.frame.as_str())
        {
            return Err(StoreError::CurrentFrameNodeMismatch {
                claimed: meta
                    .current_frame_node_id
                    .as_ref()
                    .map(|id| id.as_str().to_owned()),
                derived: Some(last.frame),
            });
        }
        let frame = visible_header(conn, session, &last.frame)?
            .filter(|h| !h.tombstoned)
            .ok_or_else(|| corrupt("SessionGraph", "frame is not readable"))?;
        if frame.generation < 0 || frame.generation > last.generation {
            return Err(corrupt("SessionGraph", "frame generation exceeds leaf"));
        }
        let mut stmt = conn
            .prepare_cached(session_sql::session_sql().graph_sqlite.window_rows.sql())
            .map_err(sqlite_error)?;
        let rows = stmt
            .query_map(
                params![session.as_str(), frame.generation, last.generation],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .map_err(sqlite_error)?;
        let mut nodes = Vec::new();
        for row in rows {
            let (id, parent, body, generation, pointer, bytes) = row.map_err(sqlite_error)?;
            if generation != frame.generation + i64::try_from(nodes.len()).unwrap_or(i64::MAX) {
                return Err(corrupt("SessionGraph", format!("generation gap at `{id}`")));
            }
            if pointer != frame.id {
                return Err(StoreError::InvalidWindowAnchor {
                    frame_node_id: frame.id.clone().into(),
                    violation:
                        lash_core_execution::store::WindowAnchorViolation::ForeignFramePointer,
                });
            }
            if nonnegative("SessionGraph", "body_bytes", bytes)? != body.len() as u64 {
                return Err(corrupt(
                    "SessionGraph",
                    format!("body_bytes differs at `{id}`"),
                ));
            }
            nodes.push(
                lash_core_execution::SessionNodeRecord::decode_storage_body_for_fleet(
                    id, parent, &body, fleet,
                )
                .map_err(|error| corrupt("SessionGraph node", error.to_string()))?,
            );
            decoded.fetch_add(1, Ordering::Relaxed);
        }
        let previous = match frame.parent.as_deref() {
            Some(parent) => {
                let row = visible_header(conn, session, parent)?
                    .filter(|h| !h.tombstoned)
                    .ok_or_else(|| corrupt("SessionGraph", "frame parent is missing"))?;
                Some(
                    lash_core_execution::FrameNodeId::new(row.frame)
                        .map_err(|error| corrupt("SessionGraph", error.to_string()))?,
                )
            }
            None => None,
        };
        let anchor = lash_core_execution::session_graph::WindowAnchor {
            frame_node_id: lash_core_execution::FrameNodeId::new(frame.id.clone())
                .map_err(|error| corrupt("SessionGraph", error.to_string()))?,
            generation: nonnegative("SessionGraph", "generation", frame.generation)?,
            external_parent: frame.parent.map(Into::into),
            previous_frame_node_id: previous,
        };
        let graph = lash_core_execution::SessionGraph::from_window(nodes, leaf, anchor)?;
        if meta.current_frame_node_id.as_ref().map(|id| id.as_str()) != Some(frame.id.as_str()) {
            config = graph
                .nodes
                .first()
                .and_then(|node| node.frame_config())
                .ok_or_else(|| corrupt("SessionGraph", "frame has no config"))?;
        }
        graph
    } else {
        lash_core_execution::SessionGraph::default()
    };
    let usage = usage_totals(conn, session, holes)?;
    SessionWindowRead::new(
        session.clone(),
        revision,
        config,
        pending,
        graph,
        checkpoint_ref,
        checkpoint,
        usage,
    )
    .map(Some)
}

#[async_trait::async_trait]
impl SessionHistoryStore for SqliteStore {
    async fn load_session_window(
        &self,
        session_id: &SessionId,
        selector: WindowSelector,
    ) -> Result<Option<SessionWindowRead>, StoreError> {
        let session = session_id.clone();
        let fleet = self.fleet_format;
        let decoded = Arc::clone(&self.decoded_graph_node_bodies);
        let holes = Arc::clone(&self.decoded_usage_holes);
        self.read_connection()
            .read(move |conn| Ok(window(conn, &session, selector, fleet, &decoded, &holes)))
            .await
            .map_err(sqlite_error)?
    }
    async fn load_ancestors(
        &self,
        session_id: &SessionId,
        anchor: HistoryAnchor,
        budget: HistoryBudget,
    ) -> Result<HistoryPage, StoreError> {
        if let HistoryAnchor::Cursor(cursor) = &anchor {
            cursor.check_session(session_id)?;
        }
        let session = session_id.clone();
        let fleet = self.fleet_format;
        let decoded = Arc::clone(&self.decoded_graph_node_bodies);
        self.read_connection()
            .read(move |conn| Ok(ancestors(conn, &session, anchor, budget, fleet, &decoded)))
            .await
            .map_err(sqlite_error)?
    }
    async fn contains_active_ancestor(
        &self,
        session_id: &SessionId,
        node_id: &lash_core_execution::NodeId,
    ) -> Result<bool, StoreError> {
        let session = session_id.clone();
        let node = node_id.clone();
        self.read_connection().read(move|conn|Ok((||{live(conn,&session)?;conn.query_row("SELECT EXISTS(SELECT 1 FROM session_head AS head JOIN graph_nodes AS leaf ON leaf.node_id=head.leaf_node_id JOIN graph_nodes AS node ON node.node_id=?2 WHERE head.session_id=?1 AND leaf.tombstoned=0 AND node.tombstoned=0 AND node.generation<=leaf.generation AND (node.session_id=?1 OR EXISTS(SELECT 1 FROM fork_lineage AS lineage WHERE lineage.session_id=?1 AND lineage.ancestor_session_id=node.session_id AND node.generation<=lineage.fork_generation)))",params![session.as_str(),node.as_str()],|row|row.get::<_,bool>(0)).map_err(sqlite_error)})())).await.map_err(sqlite_error)?
    }
    async fn load_usage_totals(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionUsageTotals, StoreError> {
        let session = session_id.clone();
        let holes = Arc::clone(&self.decoded_usage_holes);
        self.read_connection()
            .read(move |conn| {
                Ok((|| {
                    live(conn, &session)?;
                    usage_totals(conn, &session, &holes)
                })())
            })
            .await
            .map_err(sqlite_error)?
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
        let session = session_id.clone();
        let after = after.map(UsageLedgerCursor::after_seq);
        let decoded = Arc::clone(&self.decoded_usage_rows);
        self.read_connection()
            .read(move |conn| Ok(usage_page(conn, &session, after, limit, &decoded)))
            .await
            .map_err(sqlite_error)?
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
        let session = session_id.clone();
        let after = after.cloned();
        let fleet = self.fleet_format;
        let decoded = Arc::clone(&self.decoded_turn_receipt_bodies);
        self.read_connection()
            .read(move |conn| Ok(failure_page(conn, &session, after, limit, fleet, &decoded)))
            .await
            .map_err(sqlite_error)?
    }
}
fn ancestors(
    conn: &Connection,
    session: &SessionId,
    anchor: HistoryAnchor,
    budget: HistoryBudget,
    fleet: lash_core_execution::FleetFormat,
    decoded: &AtomicU64,
) -> Result<HistoryPage, StoreError> {
    live(conn, session)?;
    let stamp = lineage(conn, session)?;
    let (pinned, start, expected) = match anchor {
        HistoryAnchor::Head => {
            let meta =
                try_load_session_head_meta_from_conn(conn, session, fleet)?.ok_or_else(|| {
                    StoreError::SessionNotFound {
                        session_id: session.clone(),
                    }
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
            if cursor.lineage() != &stamp {
                return Err(StoreError::HistoryCursorLineageChanged {
                    session_id: session.clone(),
                });
            }
            (
                cursor.pinned_leaf().clone(),
                cursor.next_node_id().clone(),
                Some(cursor.next_generation()),
            )
        }
    };
    let first = match visible_header(conn, session, start.as_str())?.filter(|h| !h.tombstoned) {
        Some(row) => row,
        None => return Err(missing_anchor(conn, session, start.as_str())?),
    };
    let start_generation = nonnegative("SessionGraph", "generation", first.generation)?;
    if expected.is_some_and(|g| g != start_generation) {
        return Err(corrupt("SessionGraph", "history cursor generation changed"));
    }
    let mut stmt = conn
        .prepare_cached(session_sql::session_sql().graph_sqlite.page_headers.sql())
        .map_err(sqlite_error)?;
    let rows = stmt
        .query_map(
            params![
                session.as_str(),
                first.generation,
                i64::from(budget.max_nodes.get()) + 1
            ],
            header,
        )
        .map_err(sqlite_error)?;
    let mut headers: Vec<Header> = Vec::new();
    let mut bytes = 0u64;
    let mut stopped_by_bytes = false;
    let mut found_next = false;
    for row in rows {
        let row = row.map_err(sqlite_error)?;
        let generation = nonnegative("SessionGraph", "generation", row.generation)?;
        if let Some(previous) = headers.last() {
            if previous.parent.as_deref() != Some(row.id.as_str())
                || nonnegative("SessionGraph", "generation", previous.generation)? != generation + 1
            {
                return Err(corrupt(
                    "SessionGraph",
                    format!("history gap at `{}`", row.id),
                ));
            }
        } else if row.id != start.as_str() || generation != start_generation {
            return Err(corrupt(
                "SessionGraph",
                "history page does not begin at anchor",
            ));
        }
        let size = nonnegative("SessionGraph", "body_bytes", row.bytes)?;
        if headers.is_empty() && size > budget.max_bytes.get() {
            return Err(StoreError::HistoryNodeTooLarge {
                node_id: row.id.into(),
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
        headers.push(row);
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
        Some(HistoryCursor::new(
            session.clone(),
            pinned.clone(),
            stamp,
            last.parent
                .as_ref()
                .ok_or_else(|| corrupt("SessionGraph", "non-root row lacks parent"))?
                .clone()
                .into(),
            nonnegative("SessionGraph", "generation", last.generation)? - 1,
        ))
    };
    let mut body_stmt = conn
        .prepare_cached(session_sql::session_sql().graph_sqlite.select_body.sql())
        .map_err(sqlite_error)?;
    let mut nodes = Vec::with_capacity(headers.len());
    for header in headers {
        let body: String = body_stmt
            .query_row(params![header.id], |row| row.get(0))
            .map_err(sqlite_error)?;
        if body.len() as u64 != nonnegative("SessionGraph", "body_bytes", header.bytes)? {
            return Err(corrupt(
                "SessionGraph",
                format!("body_bytes differs at `{}`", header.id),
            ));
        }
        let record = lash_core_execution::SessionNodeRecord::decode_storage_body_for_fleet(
            header.id,
            header.parent,
            &body,
            fleet,
        )
        .map_err(|error| corrupt("SessionGraph node", error.to_string()))?;
        decoded.fetch_add(1, Ordering::Relaxed);
        nodes.push(HistoryNode {
            generation: nonnegative("SessionGraph", "generation", header.generation)?,
            owner_session_id: SessionId::from(header.owner),
            frame_node_id: lash_core_execution::FrameNodeId::new(header.frame)
                .map_err(|error| corrupt("SessionGraph", error.to_string()))?,
            body_bytes: nonnegative("SessionGraph", "body_bytes", header.bytes)?,
            record,
        });
    }
    Ok(HistoryPage {
        pinned_leaf: Some(pinned),
        nodes,
        stop,
        next,
    })
}

fn usage_totals(
    conn: &Connection,
    session: &SessionId,
    decoded_holes: &AtomicU64,
) -> Result<SessionUsageTotals, StoreError> {
    let mut totals = SessionUsageTotals::default();
    let mut stmt=conn.prepare_cached("SELECT source,model,SUM(input_tokens),SUM(output_tokens),SUM(cache_read_input_tokens),SUM(cache_write_input_tokens),SUM(reasoning_output_tokens),COUNT(*) FILTER (WHERE reconciled_call_id IS NOT NULL) FROM usage_deltas AS usage WHERE session_id=?1 AND (input_tokens<>0 OR output_tokens<>0 OR cache_read_input_tokens<>0 OR cache_write_input_tokens<>0 OR reasoning_output_tokens<>0 OR reconciled_call_id IS NOT NULL OR EXISTS(SELECT 1 FROM usage_delta_holes AS hole WHERE hole.session_id=usage.session_id AND hole.seq=usage.seq)) GROUP BY source,model ORDER BY source,model").map_err(sqlite_error)?;
    let rows = stmt
        .query_map(params![session.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(sqlite_error)?;
    for row in rows {
        let (
            source,
            model,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
            reconciled,
        ) = row.map_err(sqlite_error)?;
        let usage = lash_core_execution::TokenUsage {
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            reasoning_output_tokens,
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
            reconciled_attempts: nonnegative(
                "TokenLedgerEntry",
                "reconciled_attempts",
                reconciled,
            )?,
        });
    }
    let mut stmt=conn.prepare_cached("SELECT usage.source,usage.model,hole.call_id,hole.attempt_ordinal,hole.generation_id,EXISTS(SELECT 1 FROM usage_deltas AS correction WHERE correction.session_id=?1 AND correction.reconciled_call_id=hole.call_id AND correction.reconciled_attempt_ordinal=hole.attempt_ordinal) FROM usage_delta_holes AS hole JOIN usage_deltas AS usage ON usage.seq=hole.seq AND usage.session_id=hole.session_id WHERE hole.session_id=?1 ORDER BY hole.call_id,hole.attempt_ordinal,hole.seq").map_err(sqlite_error)?;
    let rows = stmt
        .query_map(params![session.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut seen =
        std::collections::BTreeMap::<(String, u32), (String, String, Option<String>)>::new();
    for row in rows {
        let (source, model, call, ordinal, generation, reconciled) = row.map_err(sqlite_error)?;
        let ordinal = u32::try_from(ordinal)
            .map_err(|_| corrupt("TokenLedgerEntry", "invalid attempt ordinal"))?;
        let key = (call.clone(), ordinal);
        if let Some(prior) = seen.get(&key) {
            if prior != &(source.clone(), model.clone(), generation.clone()) {
                return Err(corrupt(
                    "TokenLedgerEntry",
                    "hole attribution differs across usage rows",
                ));
            }
            continue;
        }
        seen.insert(key, (source.clone(), model.clone(), generation.clone()));
        let total = totals
            .rows
            .iter_mut()
            .find(|total| total.source == source && total.model == model)
            .ok_or_else(|| corrupt("TokenLedgerEntry", "hole has no usage aggregate"))?;
        total.unreported_attempts += 1;
        if !reconciled {
            totals.outstanding.push(UnreportedUsageAttempt {
                call_id: call,
                attempt_ordinal: ordinal,
                source,
                model,
                generation_id: generation,
            });
            decoded_holes.fetch_add(1, Ordering::Relaxed);
        }
    }
    Ok(totals)
}

fn usage_page(
    conn: &Connection,
    session: &SessionId,
    after: Option<u64>,
    limit: NonZeroU32,
    decoded: &AtomicU64,
) -> Result<UsageLedgerPage, StoreError> {
    live(conn, session)?;
    let mut stmt=conn.prepare_cached("SELECT seq,operation_storage_key,source,model,input_tokens,output_tokens,cache_read_input_tokens,cache_write_input_tokens,reasoning_output_tokens,reconciled_call_id,reconciled_attempt_ordinal FROM usage_deltas WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3").map_err(sqlite_error)?;
    let rows = stmt
        .query_map(
            params![
                session.as_str(),
                after
                    .map(|n| i64::try_from(n).unwrap_or(i64::MAX))
                    .unwrap_or(0),
                i64::from(limit.get()) + 1
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            },
        )
        .map_err(sqlite_error)?;
    let mut rows = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?;
    let more = rows.len() > limit.get() as usize;
    if more {
        rows.pop();
    }
    let mut hole_stmt=conn.prepare_cached("SELECT call_id,attempt_ordinal,generation_id FROM usage_delta_holes WHERE session_id=?1 AND seq=?2 ORDER BY call_id,attempt_ordinal").map_err(sqlite_error)?;
    let mut output = Vec::new();
    for (
        seq,
        operation_storage_key,
        source,
        model,
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
        reconciled_call_id,
        reconciled_ordinal,
    ) in rows
    {
        let holes = hole_stmt
            .query_map(params![session.as_str(), seq], |row| {
                Ok(lash_core_execution::runtime::UnreportedLedgerAttempt {
                    call_id: row.get(0)?,
                    attempt_ordinal: u32::try_from(row.get::<_, i64>(1)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    generation_id: row.get(2)?,
                })
            })
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        let disposition = match (reconciled_call_id, reconciled_ordinal) {
            (Some(call), Some(ordinal)) if holes.is_empty() => {
                lash_core_execution::LedgerUsageDisposition::Reconciled {
                    call_id: call,
                    attempt_ordinal: u32::try_from(ordinal)
                        .map_err(|_| corrupt("TokenLedgerEntry", "invalid reconciled ordinal"))?,
                }
            }
            (None, None) if !holes.is_empty() => {
                lash_core_execution::LedgerUsageDisposition::unreported(holes)
            }
            (None, None) => lash_core_execution::LedgerUsageDisposition::Reported,
            _ => {
                return Err(corrupt(
                    "TokenLedgerEntry",
                    "reconciliation columns disagree with holes",
                ));
            }
        };
        output.push(UsageLedgerRow {
            seq: nonnegative("TokenLedgerEntry", "seq", seq)?,
            operation_storage_key,
            entry: lash_core_execution::TokenLedgerEntry {
                source,
                model,
                usage: lash_core_execution::TokenUsage {
                    input_tokens,
                    output_tokens,
                    cache_read_input_tokens,
                    cache_write_input_tokens,
                    reasoning_output_tokens,
                },
                usage_disposition: disposition,
            },
        });
        decoded.fetch_add(1, Ordering::Relaxed);
    }
    let next = if more {
        output
            .last()
            .map(|row| UsageLedgerCursor::new(session.clone(), row.seq))
    } else {
        None
    };
    Ok(UsageLedgerPage { rows: output, next })
}
fn failure_page(
    conn: &Connection,
    session: &SessionId,
    after: Option<FailureEvidenceCursor>,
    limit: NonZeroU32,
    fleet: lash_core_execution::FleetFormat,
    decoded: &AtomicU64,
) -> Result<FailureEvidencePage, StoreError> {
    live(conn, session)?;
    let sql = &session_sql::session_sql().turn_commits;
    let (query, bind): (&str, Vec<rusqlite::types::Value>) = match after {
        Some(cursor) => (
            sql.select_failure_settlements_after.sql(),
            vec![
                session.as_str().to_owned().into(),
                i64::try_from(cursor.committed_at_ms())
                    .unwrap_or(i64::MAX)
                    .into(),
                cursor.turn_id().as_str().to_owned().into(),
                (i64::from(limit.get()) + 1).into(),
            ],
        ),
        None => (
            sql.select_failure_settlements.sql(),
            vec![
                session.as_str().to_owned().into(),
                (i64::from(limit.get()) + 1).into(),
            ],
        ),
    };
    let mut stmt = conn.prepare_cached(query).map_err(sqlite_error)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(bind), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(sqlite_error)?;
    let mut rows = rows.collect::<Result<Vec<_>, _>>().map_err(sqlite_error)?;
    let more = rows.len() > limit.get() as usize;
    if more {
        rows.pop();
    }
    let mut settlements = Vec::new();
    let mut last = None;
    for (committed_at_ms, turn_id, result_json, outcome_code) in rows {
        let receipt = lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
            session,
            &turn_id,
            &result_json,
            fleet,
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
        decoded.fetch_add(1, Ordering::Relaxed);
        last = Some(FailureEvidenceCursor::new(
            session.clone(),
            nonnegative("RuntimeCommitReceipt", "committed_at_ms", committed_at_ms)?,
            turn_id.clone().into(),
        ));
        settlements.push(lash_core_execution::TurnFailureSettlement {
            turn_id,
            evidence: receipt.failure_evidence,
        });
    }
    Ok(FailureEvidencePage {
        settlements,
        next: if more { last } else { None },
    })
}
