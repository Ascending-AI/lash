use crate::*;

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
                StoreError::Backend(
                    "observer-intent depth does not fit PostgreSQL BIGINT".to_string(),
                )
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

    fn from_row(row: &PgRow) -> Self {
        Self {
            session_id: row.get("session_id"),
            relation_kind: row.get("relation_kind"),
            observer_intent_depth: row.get("observer_intent_depth"),
            parent_session_id: row.get("parent_session_id"),
            cause: CausalColumns {
                kind: row.get("caused_by_kind"),
                session_id: row.get("caused_by_session_id"),
                turn_id: row.get("caused_by_turn_id"),
                effect_id: row.get("caused_by_effect_id"),
                call_id: row.get("caused_by_call_id"),
                process_id: row.get("caused_by_process_id"),
                process_event_sequence: row.get("caused_by_process_event_sequence"),
                occurrence_id: row.get("caused_by_occurrence_id"),
                subscription_id: row.get("caused_by_subscription_id"),
                subscription_incarnation: row.get("caused_by_subscription_incarnation"),
                subscription_revision: row.get("caused_by_subscription_revision"),
                node_id: row.get("caused_by_node_id"),
            },
            source_session_id: row.get("source_session_id"),
            source_node_id: row.get("source_node_id"),
            observer_inheritance_kind: row.get("observer_inheritance_kind"),
            observer_intent_processes: Vec::new(),
            fork_pending_processes: Vec::new(),
            fork_inheritance_processes: Vec::new(),
        }
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

pub(crate) async fn write_session_meta_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    meta: &SessionMeta,
    mode: SessionMetaWrite,
) -> Result<bool, StoreError> {
    let stored = StoredRelation::encode(meta)?;
    let sql = match mode {
        SessionMetaWrite::Insert => {
            "INSERT INTO lash_session_meta
             (session_id, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                     $14, $15, $16, $17, $18, $19)
             ON CONFLICT (session_id) DO NOTHING"
        }
        SessionMetaWrite::Replace => {
            "INSERT INTO lash_session_meta
             (session_id, relation_kind, observer_intent_depth, parent_session_id,
              caused_by_kind, caused_by_session_id, caused_by_turn_id,
              caused_by_effect_id, caused_by_call_id, caused_by_process_id,
              caused_by_process_event_sequence, caused_by_occurrence_id,
              caused_by_subscription_id, caused_by_subscription_incarnation,
              caused_by_subscription_revision, caused_by_node_id, source_session_id,
              source_node_id, observer_inheritance_kind)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                     $14, $15, $16, $17, $18, $19)
             ON CONFLICT (session_id) DO UPDATE SET
               relation_kind = EXCLUDED.relation_kind,
               observer_intent_depth = EXCLUDED.observer_intent_depth,
               parent_session_id = EXCLUDED.parent_session_id,
               caused_by_kind = EXCLUDED.caused_by_kind,
               caused_by_session_id = EXCLUDED.caused_by_session_id,
               caused_by_turn_id = EXCLUDED.caused_by_turn_id,
               caused_by_effect_id = EXCLUDED.caused_by_effect_id,
               caused_by_call_id = EXCLUDED.caused_by_call_id,
               caused_by_process_id = EXCLUDED.caused_by_process_id,
               caused_by_process_event_sequence = EXCLUDED.caused_by_process_event_sequence,
               caused_by_occurrence_id = EXCLUDED.caused_by_occurrence_id,
               caused_by_subscription_id = EXCLUDED.caused_by_subscription_id,
               caused_by_subscription_incarnation = EXCLUDED.caused_by_subscription_incarnation,
               caused_by_subscription_revision = EXCLUDED.caused_by_subscription_revision,
               caused_by_node_id = EXCLUDED.caused_by_node_id,
               source_session_id = EXCLUDED.source_session_id,
               source_node_id = EXCLUDED.source_node_id,
               observer_inheritance_kind = EXCLUDED.observer_inheritance_kind"
        }
    };
    let result = sqlx::query(sql)
        .bind(&stored.session_id)
        .bind(&stored.relation_kind)
        .bind(stored.observer_intent_depth)
        .bind(&stored.parent_session_id)
        .bind(&stored.cause.kind)
        .bind(&stored.cause.session_id)
        .bind(&stored.cause.turn_id)
        .bind(&stored.cause.effect_id)
        .bind(&stored.cause.call_id)
        .bind(&stored.cause.process_id)
        .bind(stored.cause.process_event_sequence)
        .bind(&stored.cause.occurrence_id)
        .bind(&stored.cause.subscription_id)
        .bind(&stored.cause.subscription_incarnation)
        .bind(stored.cause.subscription_revision)
        .bind(&stored.cause.node_id)
        .bind(&stored.source_session_id)
        .bind(&stored.source_node_id)
        .bind(&stored.observer_inheritance_kind)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    if result.rows_affected() == 0 {
        return Ok(false);
    }

    for table in [
        "lash_session_meta_observer_intent_processes",
        "lash_session_meta_fork_pending_observer_processes",
        "lash_session_meta_fork_inheritance_processes",
    ] {
        sqlx::query(&format!("DELETE FROM {table} WHERE session_id = $1"))
            .bind(&stored.session_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    for (layer_index, process_ids) in stored.observer_intent_processes.iter().enumerate() {
        for (process_index, process_id) in process_ids.iter().enumerate() {
            sqlx::query(
                "INSERT INTO lash_session_meta_observer_intent_processes
                 (session_id, layer_index, process_index, process_id)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(&stored.session_id)
            .bind(write_index(layer_index, "observer-intent layer")?)
            .bind(write_index(process_index, "observer-intent process")?)
            .bind(process_id)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        }
    }
    write_process_list(
        tx,
        "lash_session_meta_fork_pending_observer_processes",
        &stored.session_id,
        &stored.fork_pending_processes,
    )
    .await?;
    write_process_list(
        tx,
        "lash_session_meta_fork_inheritance_processes",
        &stored.session_id,
        &stored.fork_inheritance_processes,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn load_session_meta(
    pool: &PgPool,
    selected_session_id: Option<&str>,
) -> Result<Option<SessionMeta>, StoreError> {
    let mut tx = pool.begin().await.map_err(store_sqlx_error)?;
    let row = if let Some(session_id) = selected_session_id {
        sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM lash_session_meta WHERE session_id = $1 FOR SHARE"
        ))
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
    } else {
        sqlx::query(&format!(
            "SELECT {SELECT_COLUMNS} FROM lash_session_meta
             ORDER BY session_id ASC LIMIT 1 FOR SHARE"
        ))
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_sqlx_error)?
    };
    let Some(row) = row else {
        tx.commit().await.map_err(store_sqlx_error)?;
        return Ok(None);
    };
    let mut stored = StoredRelation::from_row(&row);
    let depth = read_index(stored.observer_intent_depth, "observer_intent_depth")?;
    stored.observer_intent_processes = vec![Vec::new(); depth];
    let observer_rows = sqlx::query_as::<_, (i64, i64, String)>(
        "SELECT layer_index, process_index, process_id
         FROM lash_session_meta_observer_intent_processes
         WHERE session_id = $1 ORDER BY layer_index, process_index",
    )
    .bind(&stored.session_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(store_sqlx_error)?;
    for (layer_index, process_index, process_id) in observer_rows {
        let layer_index = read_index(layer_index, "observer-intent layer_index")?;
        let layer = stored
            .observer_intent_processes
            .get_mut(layer_index)
            .ok_or_else(|| corrupt("observer-intent process names a missing layer"))?;
        let process_index = read_index(process_index, "observer-intent process_index")?;
        if process_index != layer.len() {
            return Err(corrupt(
                "observer-intent process indexes are not contiguous",
            ));
        }
        layer.push(process_id);
    }
    stored.fork_pending_processes = read_process_list(
        &mut tx,
        "lash_session_meta_fork_pending_observer_processes",
        &stored.session_id,
    )
    .await?;
    stored.fork_inheritance_processes = read_process_list(
        &mut tx,
        "lash_session_meta_fork_inheritance_processes",
        &stored.session_id,
    )
    .await?;
    let meta = stored.decode()?;
    tx.commit().await.map_err(store_sqlx_error)?;
    Ok(Some(meta))
}

async fn write_process_list(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    session_id: &str,
    process_ids: &[String],
) -> Result<(), StoreError> {
    for (process_index, process_id) in process_ids.iter().enumerate() {
        sqlx::query(&format!(
            "INSERT INTO {table} (session_id, process_index, process_id) VALUES ($1, $2, $3)"
        ))
        .bind(session_id)
        .bind(write_index(process_index, "process")?)
        .bind(process_id)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    }
    Ok(())
}

async fn read_process_list(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    session_id: &str,
) -> Result<Vec<String>, StoreError> {
    let rows = sqlx::query_as::<_, (i64, String)>(&format!(
        "SELECT process_index, process_id FROM {table}
         WHERE session_id = $1 ORDER BY process_index"
    ))
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
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
        .map_err(|_| StoreError::Backend(format!("{field} index does not fit PostgreSQL BIGINT")))
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
