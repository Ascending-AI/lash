use crate::{
    CausalRef, ObserverInheritance, SessionMeta, SessionObserverIntent,
    SessionObserverIntentAttribution, SessionRelation, StoreError,
};

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

/// Backend-neutral representation of one stored observer intent.
pub struct StoredObserverIntent {
    pub process_id: String,
    pub process_incarnation: Option<i64>,
    pub attribution: String,
}

/// Backend-neutral representation of one stored session relation and its lists.
pub struct StoredRelation {
    pub session_id: String,
    pub relation_kind: String,
    pub parent_session_id: Option<String>,
    pub cause: CausalColumns,
    pub source_session_id: Option<String>,
    pub source_node_id: Option<String>,
    pub observer_inheritance_kind: Option<String>,
    pub pending_observer_intents: Vec<StoredObserverIntent>,
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
        let mut seen_processes = std::collections::BTreeSet::new();
        let mut pending_observer_intents = Vec::with_capacity(meta.pending_observer_intents.len());
        for intent in &meta.pending_observer_intents {
            if !seen_processes.insert(intent.process_id.as_str()) {
                return Err(StoreError::Backend(format!(
                    "session `{}` has duplicate pending observer intent for process `{}`",
                    meta.session_id, intent.process_id
                )));
            }
            let process_incarnation = intent
                .process_incarnation
                .map(|value| {
                    i64::try_from(value).map_err(|_| {
                        StoreError::Backend(format!(
                            "process incarnation does not fit {}",
                            self.backend_integer_type
                        ))
                    })
                })
                .transpose()?;
            let attribution = match intent.attribution {
                SessionObserverIntentAttribution::HostRequested => "host_requested",
                SessionObserverIntentAttribution::ForkInherited => "fork_inherited",
            };
            pending_observer_intents.push(StoredObserverIntent {
                process_id: intent.process_id.clone(),
                process_incarnation,
                attribution: attribution.to_string(),
            });
        }
        let mut stored = StoredRelation {
            session_id: meta.session_id.clone(),
            relation_kind: String::new(),
            parent_session_id: None,
            cause: CausalColumns::default(),
            source_session_id: None,
            source_node_id: None,
            observer_inheritance_kind: None,
            pending_observer_intents,
            fork_inheritance_processes: Vec::new(),
        };
        match &meta.relation {
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
            } => {
                stored.relation_kind = "fork".to_string();
                stored.source_session_id = Some(source_session_id.clone());
                stored.source_node_id = Some(source_node_id.clone());
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
        }
        Ok(stored)
    }

    /// Decode one relation row together with its ordered child-table rows.
    ///
    /// Catalog queries use this entry point after aggregating every relation
    /// child table in the same SQL statement as the metadata row. Keeping the
    /// index validation here makes that single-snapshot path obey the same
    /// corruption contract as the ordinary metadata loader.
    pub fn decode_with_process_rows(
        self,
        mut stored: StoredRelation,
        observer_intent_rows: Vec<(i64, String, Option<i64>, String)>,
        fork_inheritance_rows: Vec<(i64, String)>,
    ) -> Result<SessionMeta, StoreError> {
        for (process_index, process_id, process_incarnation, attribution) in observer_intent_rows {
            if self.read_index(process_index, "observer-intent process_index")?
                != stored.pending_observer_intents.len()
            {
                return Err(self.corrupt("observer-intent process indexes are not contiguous"));
            }
            stored.pending_observer_intents.push(StoredObserverIntent {
                process_id,
                process_incarnation,
                attribution,
            });
        }
        stored.fork_inheritance_processes =
            self.decode_process_rows(fork_inheritance_rows, "fork inheritance process_index")?;
        self.decode(stored)
    }

    /// Decode and validate backend-neutral stored columns as public metadata.
    pub fn decode(self, stored: StoredRelation) -> Result<SessionMeta, StoreError> {
        let relation = match stored.relation_kind.as_str() {
            "root" => {
                self.require_empty(
                    &stored.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                SessionRelation::Root
            }
            "child" => {
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
                }
            }
            other => return Err(self.corrupt(format!("unknown relation_kind `{other}`"))),
        };
        let mut pending_observer_intents =
            Vec::with_capacity(stored.pending_observer_intents.len());
        for intent in stored.pending_observer_intents {
            let process_incarnation = intent
                .process_incarnation
                .map(|value| {
                    u64::try_from(value).map_err(|_| {
                        self.corrupt(format!(
                            "process_incarnation must be non-negative, got {value}"
                        ))
                    })
                })
                .transpose()?;
            let attribution = match intent.attribution.as_str() {
                "host_requested" => SessionObserverIntentAttribution::HostRequested,
                "fork_inherited" => SessionObserverIntentAttribution::ForkInherited,
                other => {
                    return Err(
                        self.corrupt(format!("unknown observer-intent attribution `{other}`"))
                    );
                }
            };
            pending_observer_intents.push(SessionObserverIntent {
                process_id: intent.process_id,
                process_incarnation,
                attribution,
            });
        }
        Ok(SessionMeta {
            session_id: stored.session_id,
            relation,
            pending_observer_intents,
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

    fn decode_process_rows(
        self,
        rows: Vec<(i64, String)>,
        field: &'static str,
    ) -> Result<Vec<String>, StoreError> {
        let mut process_ids = Vec::with_capacity(rows.len());
        for (process_index, process_id) in rows {
            if self.read_index(process_index, field)? != process_ids.len() {
                return Err(self.corrupt("process indexes are not contiguous"));
            }
            process_ids.push(process_id);
        }
        Ok(process_ids)
    }

    fn read_u64_text(self, value: String, field: &'static str) -> Result<u64, StoreError> {
        value
            .parse()
            .map_err(|_| self.corrupt(format!("{field} is not an unsigned integer: `{value}`")))
    }
}
