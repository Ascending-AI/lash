use crate::{CausalRef, ObserverInheritance, SessionMeta, SessionRelation, StoreError};

const RECORD_KIND: &str = "SessionMeta relation";

/// Whether a backend inserts absent session metadata or replaces an existing row.
#[derive(Clone, Copy)]
pub enum SessionMetaWrite {
    Insert,
    Replace,
}

/// Backend-neutral columns storing the causal reference of a session relation.
#[derive(Default)]
pub struct CausalColumns {
    pub kind: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub effect_id: Option<String>,
    pub call_id: Option<String>,
    pub process_id: Option<String>,
    pub process_event_sequence: Option<String>,
    pub occurrence_id: Option<String>,
    pub subscription_id: Option<String>,
    pub subscription_incarnation: Option<String>,
    pub subscription_revision: Option<String>,
    pub node_id: Option<String>,
}

impl CausalColumns {
    fn encode(cause: Option<&CausalRef>) -> Self {
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
        columns
    }

    fn decode(self, codec: SessionMetaCodec) -> Result<Option<CausalRef>, StoreError> {
        let Some(kind) = self.kind.as_deref() else {
            return Ok(None);
        };
        let cause = match kind {
            "turn" => CausalRef::Turn {
                session_id: codec.required(self.session_id, "caused_by_session_id")?,
                turn_id: codec.required(self.turn_id, "caused_by_turn_id")?,
            },
            "effect" => CausalRef::Effect {
                session_id: codec.required(self.session_id, "caused_by_session_id")?,
                turn_id: self.turn_id,
                effect_id: codec.required(self.effect_id, "caused_by_effect_id")?,
            },
            "tool_call" => CausalRef::ToolCall {
                session_id: codec.required(self.session_id, "caused_by_session_id")?,
                call_id: codec.required(self.call_id, "caused_by_call_id")?,
            },
            "process" => CausalRef::Process {
                process_id: codec.required(self.process_id, "caused_by_process_id")?,
            },
            "process_event" => CausalRef::ProcessEvent {
                process_id: codec.required(self.process_id, "caused_by_process_id")?,
                sequence: codec.read_u64_text(
                    codec.required(
                        self.process_event_sequence,
                        "caused_by_process_event_sequence",
                    )?,
                    "caused_by_process_event_sequence",
                )?,
            },
            "trigger_occurrence" => CausalRef::TriggerOccurrence {
                occurrence_id: codec.required(self.occurrence_id, "caused_by_occurrence_id")?,
                subscription_id: self.subscription_id,
                subscription_incarnation: self.subscription_incarnation,
                subscription_revision: self
                    .subscription_revision
                    .map(|value| codec.read_u64_text(value, "caused_by_subscription_revision"))
                    .transpose()?,
            },
            "session_node" => CausalRef::SessionNode {
                session_id: codec.required(self.session_id, "caused_by_session_id")?,
                node_id: codec.required(self.node_id, "caused_by_node_id")?,
            },
            other => return Err(codec.corrupt(format!("unknown caused_by_kind `{other}`"))),
        };
        Ok(Some(cause))
    }
}

/// Backend-neutral representation of one stored session relation and its lists.
pub struct StoredRelation {
    pub session_id: String,
    pub relation_kind: String,
    pub observer_intent_depth: i64,
    pub parent_session_id: Option<String>,
    pub cause: CausalColumns,
    pub source_session_id: Option<String>,
    pub source_node_id: Option<String>,
    pub observer_inheritance_kind: Option<String>,
    pub observer_intent_processes: Vec<Vec<String>>,
    pub fork_pending_processes: Vec<String>,
    pub fork_inheritance_processes: Vec<String>,
}

/// Shared session-metadata codec and stored-data validator for SQL backends.
#[derive(Clone, Copy)]
pub struct SessionMetaCodec {
    backend_integer_type: &'static str,
}

impl SessionMetaCodec {
    /// Construct a codec whose overflow diagnostics name the backend integer type.
    pub const fn new(backend_integer_type: &'static str) -> Self {
        Self {
            backend_integer_type,
        }
    }

    /// Encode public session metadata into backend-neutral stored columns.
    pub fn encode(self, meta: &SessionMeta) -> Result<StoredRelation, StoreError> {
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
                StoreError::Backend(format!(
                    "observer-intent depth does not fit {}",
                    self.backend_integer_type
                ))
            })?;
        let mut stored = StoredRelation {
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
                stored.cause = CausalColumns::encode(caused_by.as_ref());
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

    /// Decode and validate backend-neutral stored columns as public metadata.
    pub fn decode(self, stored: StoredRelation) -> Result<SessionMeta, StoreError> {
        let mut relation = match stored.relation_kind.as_str() {
            "root" => {
                self.require_empty(&stored.fork_pending_processes, "fork pending processes")?;
                self.require_empty(
                    &stored.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                SessionRelation::Root
            }
            "child" => {
                self.require_empty(&stored.fork_pending_processes, "fork pending processes")?;
                self.require_empty(
                    &stored.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                SessionRelation::Child {
                    parent_session_id: self
                        .required(stored.parent_session_id, "parent_session_id")?,
                    caused_by: stored.cause.decode(self)?,
                }
            }
            "fork" => {
                let observer_inheritance = match stored.observer_inheritance_kind.as_deref() {
                    Some("all") => ObserverInheritance::All,
                    Some("none") => ObserverInheritance::None,
                    Some("only") => ObserverInheritance::Only(stored.fork_inheritance_processes),
                    Some(other) => {
                        return Err(
                            self.corrupt(format!("unknown observer_inheritance_kind `{other}`"))
                        );
                    }
                    None => {
                        return Err(self.corrupt("fork relation is missing observer inheritance"));
                    }
                };
                SessionRelation::Fork {
                    source_session_id: self
                        .required(stored.source_session_id, "source_session_id")?,
                    source_node_id: self.required(stored.source_node_id, "source_node_id")?,
                    observer_inheritance,
                    pending_observer_process_ids: stored.fork_pending_processes,
                }
            }
            other => return Err(self.corrupt(format!("unknown relation_kind `{other}`"))),
        };
        for pending_observer_process_ids in stored.observer_intent_processes.into_iter().rev() {
            relation = SessionRelation::ObserverIntent {
                relation: Box::new(relation),
                pending_observer_process_ids,
            };
        }
        Ok(SessionMeta {
            session_id: stored.session_id,
            relation,
        })
    }

    /// Convert a collection index to the backend's signed SQL integer type.
    pub fn write_index(self, value: usize, field: &'static str) -> Result<i64, StoreError> {
        i64::try_from(value).map_err(|_| {
            StoreError::Backend(format!(
                "{field} index does not fit {}",
                self.backend_integer_type
            ))
        })
    }

    /// Validate and convert a signed stored collection index.
    pub fn read_index(self, value: i64, field: &'static str) -> Result<usize, StoreError> {
        usize::try_from(value)
            .map_err(|_| self.corrupt(format!("{field} must be non-negative, got {value}")))
    }

    /// Construct the canonical stored-session-metadata corruption error.
    pub fn corrupt(self, message: impl Into<String>) -> StoreError {
        StoreError::StoredDataCorrupt {
            record_kind: RECORD_KIND,
            message: message.into(),
        }
    }

    fn required<T>(self, value: Option<T>, field: &'static str) -> Result<T, StoreError> {
        value.ok_or_else(|| self.corrupt(format!("required column `{field}` is NULL")))
    }

    fn require_empty(self, values: &[String], field: &'static str) -> Result<(), StoreError> {
        if values.is_empty() {
            Ok(())
        } else {
            Err(self.corrupt(format!("non-fork relation has unexpected {field}")))
        }
    }

    fn read_u64_text(self, value: String, field: &'static str) -> Result<u64, StoreError> {
        value
            .parse()
            .map_err(|_| self.corrupt(format!("{field} is not an unsigned integer: `{value}`")))
    }
}
