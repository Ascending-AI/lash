use super::*;
use lash_core_execution::store::{
    AnchorUnavailable, CommittedTurnCursor, CommittedTurnNodesPage, CommittedTurnReceipt,
    FailureEvidenceCursor, FailureEvidencePage, HistoryAnchor, HistoryBudget, HistoryCursor,
    HistoryNode, HistoryPage, HistoryStop, LineageStamp, SessionHistoryStore, SessionWindowRead,
    WindowSelector,
};
use lash_core_execution::store_backend_support::{
    HeadPathProbe, OwnerExit, OwnerExitParent, OwnerLowestNode, PathNode,
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
        node_id: lash_core_execution::NodeId::parse(node)?,
        reason: if tombstoned {
            AnchorUnavailable::Tombstoned
        } else {
            AnchorUnavailable::NotReadable
        },
    })
}
fn path_generation(value: i64) -> Result<u64, StoreError> {
    nonnegative("SessionGraph", "generation", value)
}

/// The live head leaf of `session`, where [`head_reaches`] starts. `None`
/// when the session has no head row or its head has no live leaf.
pub(crate) fn head_leaf_path_node(
    conn: &Connection,
    session: &SessionId,
) -> Result<Option<PathNode>, StoreError> {
    conn.query_row(
        session_sql::session_sql()
            .graph_sqlite
            .select_head_leaf_path_node
            .sql(),
        params![session.as_str()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .optional()
    .map_err(sqlite_error)?
    .map(|(node_id, owner, generation)| {
        Ok(PathNode {
            node_id: node_id.try_into()?,
            owner_session_id: owner.try_into()?,
            generation: path_generation(generation)?,
        })
    })
    .transpose()
}

fn owner_exit(conn: &Connection, owner: &SessionId) -> Result<OwnerExit, StoreError> {
    let row = conn
        .query_row(
            session_sql::session_sql()
                .graph_sqlite
                .select_owner_exit
                .sql(),
            params![owner.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some((
        node_id,
        generation,
        parent_edge,
        parent_id,
        parent_owner,
        parent_generation,
        tombstoned,
    )) = row
    else {
        return Ok(OwnerExit { lowest: None });
    };
    let parent = match (
        parent_edge,
        parent_id,
        parent_owner,
        parent_generation,
        tombstoned,
    ) {
        (None, ..) => OwnerExitParent::Root,
        (Some(_), Some(id), Some(owner), Some(generation), Some(tombstoned)) => {
            OwnerExitParent::Node {
                node: PathNode {
                    node_id: id.try_into()?,
                    owner_session_id: owner.try_into()?,
                    generation: path_generation(generation)?,
                },
                tombstoned: tombstoned != 0,
            }
        }
        (Some(edge), ..) => OwnerExitParent::Missing {
            node_id: edge.try_into()?,
        },
    };
    Ok(OwnerExit {
        lowest: Some(OwnerLowestNode {
            node_id: node_id.try_into()?,
            generation: path_generation(generation)?,
            parent,
        }),
    })
}

/// Whether `head_leaf` reaches `candidate` through parent edges (ADR 0057,
/// edge authority). The fork-lineage ceilings that selected the candidate
/// are never consulted.
pub(crate) fn head_reaches(
    conn: &Connection,
    head_leaf: Option<PathNode>,
    candidate: PathNode,
) -> Result<bool, StoreError> {
    let mut probe = HeadPathProbe::new(candidate, head_leaf);
    loop {
        if let Some(reaches) = probe.verdict() {
            return Ok(reaches);
        }
        let exit = owner_exit(conn, probe.owner())?;
        probe.descend(exit)?;
    }
}

fn header_path_node(row: &Header) -> Result<PathNode, StoreError> {
    Ok(PathNode {
        node_id: row.id.clone().try_into()?,
        owner_session_id: row.owner.clone().try_into()?,
        generation: path_generation(row.generation)?,
    })
}

fn lineage(conn: &Connection, session: &SessionId) -> Result<LineageStamp, StoreError> {
    let mut statement = conn
        .prepare_cached(session_sql::session_sql().lineage.select_for_stamp.sql())
        .map_err(sqlite_error)?;
    let rows = statement
        .query_map(params![session.as_str()], |row| {
            Ok((
                crate::codec::sql_identity(row.get::<_, String>(0)?)?,
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
    let (revision, leaf, checkpoint_ref) = match selector {
        WindowSelector::Current => (
            meta.head_revision,
            meta.leaf_node_id.clone(),
            meta.checkpoint_ref.clone(),
        ),
        WindowSelector::Admitted(base) => (base.revision, base.leaf, base.checkpoint),
    };
    let checkpoint = match checkpoint_ref.as_ref() {
        Some(reference) => {
            let value = SqliteStore::get_checkpoint_conn(conn, reference, fleet)?;
            if value.is_none() {
                // An admitted base may have been collected since; the current
                // head's own manifest never is, so its absence is corruption.
                return Err(if admitted {
                    StoreError::TurnBaseNotRetained { revision }
                } else {
                    StoreError::CheckpointComponentMissing {
                        key: "manifest".to_string(),
                        blob_ref: reference.clone(),
                    }
                });
            }
            value
        }
        None => None,
    };
    let mut config = meta.config.clone();
    let graph = if let Some(leaf) = leaf {
        let last = match visible_header(conn, session, leaf.as_str())?.filter(|h| !h.tombstoned) {
            Some(last) => last,
            None if admitted => return Err(StoreError::TurnBaseNotRetained { revision }),
            None => {
                return Err(corrupt(
                    "SessionGraph",
                    format!("leaf `{leaf}` is not readable"),
                ));
            }
        };
        // An admitted base names a leaf the head moved on from; it is this
        // session's base only while the head still reaches it.
        if admitted
            && !head_reaches(
                conn,
                head_leaf_path_node(conn, session)?,
                header_path_node(&last)?,
            )?
        {
            return Err(StoreError::TurnBaseNotRetained { revision });
        }
        let frame = match visible_header(conn, session, &last.frame)?.filter(|h| !h.tombstoned) {
            Some(frame) => frame,
            None if admitted => return Err(StoreError::TurnBaseNotRetained { revision }),
            None => return Err(corrupt("SessionGraph", "frame is not readable")),
        };
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
                    frame_node_id: frame.id.clone().try_into()?,
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
            external_parent: frame.parent.map(TryInto::try_into).transpose()?,
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
    SessionWindowRead::new(
        session.clone(),
        revision,
        config,
        graph,
        checkpoint_ref,
        checkpoint,
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
        let fleet = self.conn.fleet();
        let decoded = Arc::clone(&self.decoded_graph_node_bodies);
        self.read_connection()
            .read(move |conn| Ok(window(conn, &session, selector, fleet, &decoded)))
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
        let fleet = self.conn.fleet();
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
        self.read_connection()
            .read(move |conn| {
                Ok((|| {
                    live(conn, &session)?;
                    let Some((leaf, candidate)) = conn
                        .query_row(
                            session_sql::session_sql()
                                .graph_sqlite
                                .select_active_ancestor_candidate
                                .sql(),
                            params![session.as_str(), node.as_str()],
                            |row| {
                                Ok((
                                    (
                                        row.get::<_, String>(0)?,
                                        row.get::<_, String>(1)?,
                                        row.get::<_, i64>(2)?,
                                    ),
                                    (row.get::<_, String>(3)?, row.get::<_, i64>(4)?),
                                ))
                            },
                        )
                        .optional()
                        .map_err(sqlite_error)?
                    else {
                        return Ok(false);
                    };
                    let (leaf_id, leaf_owner, leaf_generation) = leaf;
                    let (owner, generation) = candidate;
                    head_reaches(
                        conn,
                        Some(PathNode {
                            node_id: leaf_id.try_into()?,
                            owner_session_id: leaf_owner.try_into()?,
                            generation: path_generation(leaf_generation)?,
                        }),
                        PathNode {
                            node_id: node.clone(),
                            owner_session_id: owner.try_into()?,
                            generation: path_generation(generation)?,
                        },
                    )
                })())
            })
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
        let fleet = self.conn.fleet();
        let decoded = Arc::clone(&self.decoded_turn_receipt_bodies);
        self.read_connection()
            .read(move |conn| Ok(failure_page(conn, &session, after, limit, fleet, &decoded)))
            .await
            .map_err(sqlite_error)?
    }
    async fn load_committed_turns(
        &self,
        session_id: &SessionId,
        after: Option<&CommittedTurnCursor>,
        limit: NonZeroU32,
    ) -> Result<CommittedTurnNodesPage, StoreError> {
        if let Some(cursor) = after {
            cursor.check_session(session_id)?;
        }
        let session = session_id.clone();
        let after = after.map_or(0, CommittedTurnCursor::head_revision);
        let fleet = self.conn.fleet();
        let decoded = Arc::clone(&self.decoded_graph_node_bodies);
        self.read_connection()
            .read(move |conn| {
                Ok(committed_turns(
                    conn, &session, after, limit, fleet, &decoded,
                ))
            })
            .await
            .map_err(sqlite_error)?
    }
}

/// The session's committed turns after head revision `after`, each with
/// the nodes its commit appended that the session still holds, in one read
/// snapshot.
fn committed_turns(
    conn: &Connection,
    session: &SessionId,
    after: u64,
    limit: NonZeroU32,
    fleet: lash_core_execution::FleetFormat,
    decoded: &AtomicU64,
) -> Result<CommittedTurnNodesPage, StoreError> {
    live(conn, session)?;
    let mut stmt = conn
        .prepare_cached(
            session_sql::session_sql()
                .turn_commits
                .select_committed_turns_after
                .sql(),
        )
        .map_err(sqlite_error)?;
    let receipts = stmt
        .query_map(
            params![
                session.as_str(),
                i64::try_from(after).unwrap_or(i64::MAX),
                i64::from(limit.get())
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut body_stmt = conn
        .prepare_cached(
            session_sql::session_sql()
                .graph_sqlite
                .select_live_owned_body
                .sql(),
        )
        .map_err(sqlite_error)?;
    let mut turns = Vec::with_capacity(receipts.len());
    for (head_revision, operation, result_json, outcome_code, committed_at_ms) in receipts {
        let receipt = CommittedTurnReceipt::from_stored(
            session,
            head_revision,
            &operation,
            &result_json,
            outcome_code.as_deref(),
            committed_at_ms,
            fleet,
        )?;
        let mut nodes = Vec::with_capacity(receipt.appended().len());
        for node_id in receipt.appended() {
            let Some((parent, body, bytes)) = body_stmt
                .query_row(params![node_id.as_str(), session.as_str()], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .optional()
                .map_err(sqlite_error)?
            else {
                continue;
            };
            if body.len() as u64 != nonnegative("SessionGraph", "body_bytes", bytes)? {
                return Err(corrupt(
                    "SessionGraph",
                    format!("body_bytes differs at `{node_id}`"),
                ));
            }
            nodes.push(
                lash_core_execution::SessionNodeRecord::decode_storage_body_for_fleet(
                    node_id.to_string(),
                    parent,
                    &body,
                    fleet,
                )
                .map_err(|error| corrupt("SessionGraph node", error.to_string()))?,
            );
            decoded.fetch_add(1, Ordering::Relaxed);
        }
        turns.push(receipt.into_turn(nodes));
    }
    let next = turns.last().map_or_else(
        || CommittedTurnCursor::new(session.clone(), after),
        |turn| turn.cursor.clone(),
    );
    Ok(CommittedTurnNodesPage { turns, next })
}
/// Whether a page starts at the head leaf or at a node the caller named.
enum AnchorKind {
    Head,
    Named,
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
    let anchor_kind = match &anchor {
        HistoryAnchor::Head => AnchorKind::Head,
        HistoryAnchor::Node(_) | HistoryAnchor::Cursor(_) => AnchorKind::Named,
    };
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
    // The ceilings only selected the anchor; the head's parent edges admit
    // it. A `Head` anchor is the head leaf itself.
    if !matches!(anchor_kind, AnchorKind::Head)
        && !head_reaches(
            conn,
            head_leaf_path_node(conn, session)?,
            header_path_node(&first)?,
        )?
    {
        return Err(StoreError::HistoryAnchorUnavailable {
            session_id: session.clone(),
            node_id: start.clone(),
            reason: AnchorUnavailable::NotReadable,
        });
    }
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
                node_id: row.id.try_into()?,
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
                .try_into()?,
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
            owner_session_id: SessionId::parse(header.owner)?,
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
            turn_id.clone().try_into()?,
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
