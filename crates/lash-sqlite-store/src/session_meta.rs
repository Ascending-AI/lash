use super::*;

use lash_core::{CausalRef, ObserverInheritance, SessionRelation};

const RECORD_KIND: &str = "SessionMeta relation";

#[derive(Clone, Copy)]
pub(crate) enum SessionMetaWrite {
    Insert,
    Replace,
}

#[derive(Default)]
struct CausalColumns {
    kind: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    effect_id: Option<String>,
    call_id: Option<String>,
    process_id: Option<String>,
    process_event_sequence: Option<String>,
    occurrence_id: Option<String>,
    subscription_id: Option<String>,
    subscription_incarnation: Option<String>,
    subscription_revision: Option<String>,
    node_id: Option<String>,
}

impl CausalColumns {
    fn encode(cause: Option<&CausalRef>) -> Result<Self, StoreError> {
        let mut columns = Self::default();
        match cause {
            None => {}
            Some(CausalRef::Turn {
                session_id,
                turn_id,
            }) => {
                columns.kind = Some("turn".to_string());
                columns.session_id = Some(session_id.clone());
                columns.turn_id = Some(turn_id.clone());
            }
            Some(CausalRef::Effect {
                session_id,
                turn_id,
                effect_id,
            }) => {
                columns.kind = Some("effect".to_string());
                columns.session_id = Some(session_id.clone());
                columns.turn_id = turn_id.clone();
                columns.effect_id = Some(effect_id.clone());
            }
            Some(CausalRef::ToolCall {
                session_id,
                call_id,
            }) => {
                columns.kind = Some("tool_call".to_string());
                columns.session_id = Some(session_id.clone());
                columns.call_id = Some(call_id.clone());
            }
            Some(CausalRef::Process { process_id }) => {
                columns.kind = Some("process".to_string());
                columns.process_id = Some(process_id.clone());
            }
            Some(CausalRef::ProcessEvent {
                process_id,
                sequence,
            }) => {
                columns.kind = Some("process_event".to_string());
                columns.process_id = Some(process_id.clone());
                columns.process_event_sequence = Some(sequence.to_string());
            }
            Some(CausalRef::TriggerOccurrence {
                occurrence_id,
                subscription_id,
                subscription_incarnation,
                subscription_revision,
            }) => {
                columns.kind = Some("trigger_occurrence".to_string());
                columns.occurrence_id = Some(occurrence_id.clone());
                columns.subscription_id = subscription_id.clone();
                columns.subscription_incarnation = subscription_incarnation.clone();
                columns.subscription_revision =
                    subscription_revision.map(|value| value.to_string());
            }
            Some(CausalRef::SessionNode {
                session_id,
                node_id,
            }) => {
                columns.kind = Some("session_node".to_string());
                columns.session_id = Some(session_id.clone());
                columns.node_id = Some(node_id.clone());
            }
        }
        Ok(columns)
    }

    fn decode(self) -> Result<Option<CausalRef>, StoreError> {
        let Some(kind) = self.kind.as_deref() else {
            return Ok(None);
        };
        let cause = match kind {
            "turn" => CausalRef::Turn {
                session_id: required(self.session_id, "caused_by_session_id")?,
                turn_id: required(self.turn_id, "caused_by_turn_id")?,
            },
            "effect" => CausalRef::Effect {
                session_id: required(self.session_id, "caused_by_session_id")?,
                turn_id: self.turn_id,
                effect_id: required(self.effect_id, "caused_by_effect_id")?,
            },
            "tool_call" => CausalRef::ToolCall {
                session_id: required(self.session_id, "caused_by_session_id")?,
                call_id: required(self.call_id, "caused_by_call_id")?,
            },
            "process" => CausalRef::Process {
                process_id: required(self.process_id, "caused_by_process_id")?,
            },
            "process_event" => CausalRef::ProcessEvent {
                process_id: required(self.process_id, "caused_by_process_id")?,
                sequence: read_u64_text(
                    required(
                        self.process_event_sequence,
                        "caused_by_process_event_sequence",
                    )?,
                    "caused_by_process_event_sequence",
                )?,
            },
            "trigger_occurrence" => CausalRef::TriggerOccurrence {
                occurrence_id: required(self.occurrence_id, "caused_by_occurrence_id")?,
                subscription_id: self.subscription_id,
                subscription_incarnation: self.subscription_incarnation,
                subscription_revision: self
                    .subscription_revision
                    .map(|value| read_u64_text(value, "caused_by_subscription_revision"))
                    .transpose()?,
            },
            "session_node" => CausalRef::SessionNode {
                session_id: required(self.session_id, "caused_by_session_id")?,
                node_id: required(self.node_id, "caused_by_node_id")?,
            },
            other => return Err(corrupt(format!("unknown caused_by_kind `{other}`"))),
        };
        Ok(Some(cause))
    }
}

struct StoredRelation {
    session_id: String,
    relation_kind: String,
    observer_intent_depth: i64,
    parent_session_id: Option<String>,
    cause: CausalColumns,
    source_session_id: Option<String>,
    source_node_id: Option<String>,
    observer_inheritance_kind: Option<String>,
    observer_intent_processes: Vec<Vec<String>>,
    fork_pending_processes: Vec<String>,
    fork_inheritance_processes: Vec<String>,
}

impl StoredRelation {
    fn encode(meta: &SessionMeta) -> Result<Self, StoreError> {
        let mut relation = &meta.relation;
        let mut observer_intent_processes = Vec::new();
        while let SessionRelation::ObserverIntent {
            relation: inner,
            pending_observer_process_ids,
        } = relation
        {
            observer_intent_processes.push(pending_observer_process_ids.clone());
            relation = inner.as_ref();
        }
        let observer_intent_depth =
            i64::try_from(observer_intent_processes.len()).map_err(|_| {
                StoreError::Backend("observer-intent depth does not fit SQLite INTEGER".to_string())
            })?;
        let mut stored = Self {
            session_id: meta.session_id.clone(),
            relation_kind: String::new(),
            observer_intent_depth,
            parent_session_id: None,
            cause: CausalColumns::default(),
            source_session_id: None,
            source_node_id: None,
            observer_inheritance_kind: None,
            observer_intent_processes,
            fork_pending_processes: Vec::new(),
            fork_inheritance_processes: Vec::new(),
        };
        match relation {
            SessionRelation::Root => stored.relation_kind = "root".to_string(),
            SessionRelation::Child {
                parent_session_id,
                caused_by,
            } => {
                stored.relation_kind = "child".to_string();
                stored.parent_session_id = Some(parent_session_id.clone());
                stored.cause = CausalColumns::encode(caused_by.as_ref())?;
            }
            SessionRelation::Fork {
                source_session_id,
                source_node_id,
                observer_inheritance,
                pending_observer_process_ids,
            } => {
                stored.relation_kind = "fork".to_string();
                stored.source_session_id = Some(source_session_id.clone());
                stored.source_node_id = Some(source_node_id.clone());
                stored.fork_pending_processes = pending_observer_process_ids.clone();
                match observer_inheritance {
                    ObserverInheritance::All => {
                        stored.observer_inheritance_kind = Some("all".to_string());
                    }
                    ObserverInheritance::None => {
                        stored.observer_inheritance_kind = Some("none".to_string());
                    }
                    ObserverInheritance::Only(process_ids) => {
                        stored.observer_inheritance_kind = Some("only".to_string());
                        stored.fork_inheritance_processes = process_ids.clone();
                    }
                }
            }
            SessionRelation::ObserverIntent { .. } => {
                unreachable!("observer-intent layers were peeled above")
            }
        }
        Ok(stored)
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            session_id: row.get(0)?,
            relation_kind: row.get(1)?,
            observer_intent_depth: row.get(2)?,
            parent_session_id: row.get(3)?,
            cause: CausalColumns {
                kind: row.get(4)?,
                session_id: row.get(5)?,
                turn_id: row.get(6)?,
                effect_id: row.get(7)?,
                call_id: row.get(8)?,
                process_id: row.get(9)?,
                process_event_sequence: row.get(10)?,
                occurrence_id: row.get(11)?,
                subscription_id: row.get(12)?,
                subscription_incarnation: row.get(13)?,
                subscription_revision: row.get(14)?,
                node_id: row.get(15)?,
            },
            source_session_id: row.get(16)?,
            source_node_id: row.get(17)?,
            observer_inheritance_kind: row.get(18)?,
            observer_intent_processes: Vec::new(),
            fork_pending_processes: Vec::new(),
            fork_inheritance_processes: Vec::new(),
        })
    }

    fn decode(self) -> Result<SessionMeta, StoreError> {
        let mut relation = match self.relation_kind.as_str() {
            "root" => {
                require_empty(&self.fork_pending_processes, "fork pending processes")?;
                require_empty(
                    &self.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                SessionRelation::Root
            }
            "child" => {
                require_empty(&self.fork_pending_processes, "fork pending processes")?;
                require_empty(
                    &self.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                SessionRelation::Child {
                    parent_session_id: required(self.parent_session_id, "parent_session_id")?,
                    caused_by: self.cause.decode()?,
                }
            }
            "fork" => {
                let observer_inheritance = match self.observer_inheritance_kind.as_deref() {
                    Some("all") => ObserverInheritance::All,
                    Some("none") => ObserverInheritance::None,
                    Some("only") => ObserverInheritance::Only(self.fork_inheritance_processes),
                    Some(other) => {
                        return Err(corrupt(format!(
                            "unknown observer_inheritance_kind `{other}`"
                        )));
                    }
                    None => return Err(corrupt("fork relation is missing observer inheritance")),
                };
                SessionRelation::Fork {
                    source_session_id: required(self.source_session_id, "source_session_id")?,
                    source_node_id: required(self.source_node_id, "source_node_id")?,
                    observer_inheritance,
                    pending_observer_process_ids: self.fork_pending_processes,
                }
            }
            other => return Err(corrupt(format!("unknown relation_kind `{other}`"))),
        };
        for pending_observer_process_ids in self.observer_intent_processes.into_iter().rev() {
            relation = SessionRelation::ObserverIntent {
                relation: Box::new(relation),
                pending_observer_process_ids,
            };
        }
        Ok(SessionMeta {
            session_id: self.session_id,
            relation,
        })
    }
}

const SELECT_COLUMNS: &str = "session_id, relation_kind, observer_intent_depth,
    parent_session_id, caused_by_kind, caused_by_session_id, caused_by_turn_id,
    caused_by_effect_id, caused_by_call_id, caused_by_process_id,
    caused_by_process_event_sequence, caused_by_occurrence_id,
    caused_by_subscription_id, caused_by_subscription_incarnation,
    caused_by_subscription_revision, caused_by_node_id, source_session_id,
    source_node_id, observer_inheritance_kind";

pub(crate) fn write_session_meta(
    conn: &Connection,
    meta: &SessionMeta,
    mode: SessionMetaWrite,
) -> Result<bool, StoreError> {
    let stored = StoredRelation::encode(meta)?;
    let sql = match mode {
        SessionMetaWrite::Insert => {
            "INSERT OR IGNORE INTO session_meta
             (session_id, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19)"
        }
        SessionMetaWrite::Replace => {
            "INSERT INTO session_meta
             (session_id, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19)
             ON CONFLICT(session_id) DO UPDATE SET
               relation_kind = excluded.relation_kind,
               observer_intent_depth = excluded.observer_intent_depth,
               parent_session_id = excluded.parent_session_id,
               caused_by_kind = excluded.caused_by_kind,
               caused_by_session_id = excluded.caused_by_session_id,
               caused_by_turn_id = excluded.caused_by_turn_id,
               caused_by_effect_id = excluded.caused_by_effect_id,
               caused_by_call_id = excluded.caused_by_call_id,
               caused_by_process_id = excluded.caused_by_process_id,
               caused_by_process_event_sequence = excluded.caused_by_process_event_sequence,
               caused_by_occurrence_id = excluded.caused_by_occurrence_id,
               caused_by_subscription_id = excluded.caused_by_subscription_id,
               caused_by_subscription_incarnation = excluded.caused_by_subscription_incarnation,
               caused_by_subscription_revision = excluded.caused_by_subscription_revision,
               caused_by_node_id = excluded.caused_by_node_id,
               source_session_id = excluded.source_session_id,
               source_node_id = excluded.source_node_id,
               observer_inheritance_kind = excluded.observer_inheritance_kind"
        }
    };
    let changed = conn
        .execute(
            sql,
            params![
                stored.session_id,
                stored.relation_kind,
                stored.observer_intent_depth,
                stored.parent_session_id,
                stored.cause.kind,
                stored.cause.session_id,
                stored.cause.turn_id,
                stored.cause.effect_id,
                stored.cause.call_id,
                stored.cause.process_id,
                stored.cause.process_event_sequence,
                stored.cause.occurrence_id,
                stored.cause.subscription_id,
                stored.cause.subscription_incarnation,
                stored.cause.subscription_revision,
                stored.cause.node_id,
                stored.source_session_id,
                stored.source_node_id,
                stored.observer_inheritance_kind,
            ],
        )
        .map_err(sqlite_error)?;
    if changed == 0 {
        return Ok(false);
    }
    for table in [
        "session_meta_observer_intent_processes",
        "session_meta_fork_pending_observer_processes",
        "session_meta_fork_inheritance_processes",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            params![stored.session_id],
        )
        .map_err(sqlite_error)?;
    }
    for (layer_index, process_ids) in stored.observer_intent_processes.iter().enumerate() {
        for (process_index, process_id) in process_ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO session_meta_observer_intent_processes
                 (session_id, layer_index, process_index, process_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    stored.session_id,
                    write_index(layer_index, "observer-intent layer")?,
                    write_index(process_index, "observer-intent process")?,
                    process_id,
                ],
            )
            .map_err(sqlite_error)?;
        }
    }
    write_process_list(
        conn,
        "session_meta_fork_pending_observer_processes",
        &stored.session_id,
        &stored.fork_pending_processes,
    )?;
    write_process_list(
        conn,
        "session_meta_fork_inheritance_processes",
        &stored.session_id,
        &stored.fork_inheritance_processes,
    )?;
    Ok(true)
}

pub(crate) fn load_session_meta(
    conn: &Connection,
    selected_session_id: Option<&str>,
) -> Result<Option<SessionMeta>, StoreError> {
    let tx = conn.unchecked_transaction().map_err(sqlite_error)?;
    let session_id = if let Some(session_id) = selected_session_id {
        session_id.to_string()
    } else {
        let mut stmt = tx
            .prepare("SELECT session_id FROM session_meta ORDER BY session_id ASC LIMIT 2")
            .map_err(sqlite_error)?;
        let session_ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sqlite_error)?;
        drop(stmt);
        if session_ids.len() != 1 {
            tx.commit().map_err(sqlite_error)?;
            return Ok(None);
        }
        session_ids.into_iter().next().expect("one session id")
    };
    let mut stored = tx
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM session_meta WHERE session_id = ?1"),
            params![session_id],
            StoredRelation::from_row,
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some(mut stored) = stored.take() else {
        tx.commit().map_err(sqlite_error)?;
        return Ok(None);
    };
    let depth = read_index(stored.observer_intent_depth, "observer_intent_depth")?;
    stored.observer_intent_processes = vec![Vec::new(); depth];
    let mut stmt = tx
        .prepare(
            "SELECT layer_index, process_index, process_id
             FROM session_meta_observer_intent_processes
             WHERE session_id = ?1 ORDER BY layer_index, process_index",
        )
        .map_err(sqlite_error)?;
    let observer_rows = stmt
        .query_map(params![stored.session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    drop(stmt);
    for (layer_index, process_index, process_id) in observer_rows {
        let layer_index = read_index(layer_index, "observer-intent layer_index")?;
        let layer = stored
            .observer_intent_processes
            .get_mut(layer_index)
            .ok_or_else(|| corrupt("observer-intent process names a missing layer"))?;
        if read_index(process_index, "observer-intent process_index")? != layer.len() {
            return Err(corrupt(
                "observer-intent process indexes are not contiguous",
            ));
        }
        layer.push(process_id);
    }
    stored.fork_pending_processes = read_process_list(
        &tx,
        "session_meta_fork_pending_observer_processes",
        &stored.session_id,
    )?;
    stored.fork_inheritance_processes = read_process_list(
        &tx,
        "session_meta_fork_inheritance_processes",
        &stored.session_id,
    )?;
    let meta = stored.decode()?;
    tx.commit().map_err(sqlite_error)?;
    Ok(Some(meta))
}

fn write_process_list(
    conn: &Connection,
    table: &str,
    session_id: &str,
    process_ids: &[String],
) -> Result<(), StoreError> {
    for (process_index, process_id) in process_ids.iter().enumerate() {
        conn.execute(
            &format!(
                "INSERT INTO {table} (session_id, process_index, process_id) VALUES (?1, ?2, ?3)"
            ),
            params![
                session_id,
                write_index(process_index, "process")?,
                process_id
            ],
        )
        .map_err(sqlite_error)?;
    }
    Ok(())
}

fn read_process_list(
    conn: &Connection,
    table: &str,
    session_id: &str,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT process_index, process_id FROM {table}
             WHERE session_id = ?1 ORDER BY process_index"
        ))
        .map_err(sqlite_error)?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sqlite_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sqlite_error)?;
    let mut process_ids = Vec::with_capacity(rows.len());
    for (process_index, process_id) in rows {
        if read_index(process_index, "process_index")? != process_ids.len() {
            return Err(corrupt("process indexes are not contiguous"));
        }
        process_ids.push(process_id);
    }
    Ok(process_ids)
}

fn required<T>(value: Option<T>, field: &'static str) -> Result<T, StoreError> {
    value.ok_or_else(|| corrupt(format!("required column `{field}` is NULL")))
}

fn require_empty(values: &[String], field: &'static str) -> Result<(), StoreError> {
    if values.is_empty() {
        Ok(())
    } else {
        Err(corrupt(format!("non-fork relation has unexpected {field}")))
    }
}

fn write_index(value: usize, field: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} index does not fit SQLite INTEGER")))
}

fn read_index(value: i64, field: &'static str) -> Result<usize, StoreError> {
    usize::try_from(value)
        .map_err(|_| corrupt(format!("{field} must be non-negative, got {value}")))
}

fn read_u64_text(value: String, field: &'static str) -> Result<u64, StoreError> {
    value
        .parse()
        .map_err(|_| corrupt(format!("{field} is not an unsigned integer: `{value}`")))
}

fn corrupt(message: impl Into<String>) -> StoreError {
    StoreError::StoredDataCorrupt {
        record_kind: RECORD_KIND,
        message: message.into(),
    }
}
