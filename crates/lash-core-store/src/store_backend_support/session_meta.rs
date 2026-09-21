use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;
use crate::{
    CausalRef, ObserverInheritance, SessionLineage, SessionMeta, SessionObserverIntent,
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
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub effect_id: Option<String>,
    pub call_id: Option<String>,
    pub process_id: Option<ProcessId>,
    pub process_event_sequence: Option<String>,
    pub occurrence_id: Option<String>,
    pub subscription_id: Option<String>,
    pub subscription_incarnation: Option<String>,
    pub subscription_revision: Option<String>,
    pub node_id: Option<String>,
}

impl CausalColumns {
    #[expect(
        clippy::expect_used,
        reason = "an `EffectAddress` is a struct of validated string identities, whose serialization has no failing case"
    )]
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
            Some(CausalRef::Effect { address }) => {
                columns.kind = Some("effect_address".to_string());
                columns.effect_id = Some(
                    serde_json::to_string(address)
                        .expect("validated effect addresses serialize infallibly"),
                );
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

    /// The payload columns each `caused_by_kind` may populate — the same sets
    /// `ck_session_meta_caused_by_family` enforces in both backends' DDL.
    fn family_columns(kind: &str) -> Option<&'static [&'static str]> {
        Some(match kind {
            "turn" => &["caused_by_session_id", "caused_by_turn_id"],
            "effect_address" => &["caused_by_effect_id"],
            "tool_call" => &["caused_by_session_id", "caused_by_call_id"],
            "process" => &["caused_by_process_id"],
            "process_event" => &["caused_by_process_id", "caused_by_process_event_sequence"],
            "trigger_occurrence" => &[
                "caused_by_occurrence_id",
                "caused_by_subscription_id",
                "caused_by_subscription_incarnation",
                "caused_by_subscription_revision",
            ],
            "session_node" => &["caused_by_session_id", "caused_by_node_id"],
            _ => return None,
        })
    }

    /// The populated payload columns, named by their stored column.
    fn populated_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.session_id.is_some() {
            fields.push("caused_by_session_id");
        }
        if self.turn_id.is_some() {
            fields.push("caused_by_turn_id");
        }
        if self.effect_id.is_some() {
            fields.push("caused_by_effect_id");
        }
        if self.call_id.is_some() {
            fields.push("caused_by_call_id");
        }
        if self.process_id.is_some() {
            fields.push("caused_by_process_id");
        }
        if self.process_event_sequence.is_some() {
            fields.push("caused_by_process_event_sequence");
        }
        if self.occurrence_id.is_some() {
            fields.push("caused_by_occurrence_id");
        }
        if self.subscription_id.is_some() {
            fields.push("caused_by_subscription_id");
        }
        if self.subscription_incarnation.is_some() {
            fields.push("caused_by_subscription_incarnation");
        }
        if self.subscription_revision.is_some() {
            fields.push("caused_by_subscription_revision");
        }
        if self.node_id.is_some() {
            fields.push("caused_by_node_id");
        }
        fields
    }

    fn decode(self, codec: SessionMetaCodec) -> Result<Option<CausalRef>, StoreError> {
        let Some(kind) = self.kind.as_deref() else {
            if let Some(field) = self.populated_fields().first() {
                return Err(codec.corrupt(format!(
                    "causal payload column `{field}` is populated without caused_by_kind"
                )));
            }
            return Ok(None);
        };
        if kind == "effect" {
            return Err(codec.corrupt(
                "effect_identity_format_cutover: a session relation with legacy session/effect causal identity cannot be reopened",
            ));
        }
        let Some(family) = Self::family_columns(kind) else {
            return Err(codec.corrupt(format!("unknown caused_by_kind `{kind}`")));
        };
        for field in self.populated_fields() {
            if !family.contains(&field) {
                return Err(
                    codec.corrupt(format!("caused_by_kind `{kind}` cannot carry `{field}`"))
                );
            }
        }
        let cause = match kind {
            "turn" => CausalRef::Turn {
                session_id: codec.required(self.session_id, "caused_by_session_id")?,
                turn_id: codec.required(self.turn_id, "caused_by_turn_id")?,
            },
            "effect_address" => {
                let encoded = codec.required(self.effect_id, "caused_by_effect_address")?;
                let address: crate::EffectAddress =
                    serde_json::from_str(&encoded).map_err(|error| {
                        codec.corrupt(format!("invalid caused_by_effect_address: {error}"))
                    })?;
                address.validate().map_err(|error| {
                    codec.corrupt(format!("invalid caused_by_effect_address: {error}"))
                })?;
                CausalRef::Effect { address }
            }
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
    pub process_id: ProcessId,
    pub process_incarnation: Option<i64>,
    pub attribution: String,
}

/// Backend-neutral representation of one stored session relation and its lists.
pub struct StoredRelation {
    pub session_id: SessionId,
    pub relation_kind: String,
    pub parent_session_id: Option<SessionId>,
    pub cause: CausalColumns,
    pub source_session_id: Option<SessionId>,
    pub source_node_id: Option<String>,
    pub observer_inheritance_kind: Option<String>,
    pub pending_observer_intents: Vec<StoredObserverIntent>,
    pub fork_inheritance_processes: Vec<ProcessId>,
}

/// Shared session-metadata codec and stored-data validator for SQL backends.
#[derive(Clone, Copy)]
pub struct SessionMetaCodec {
    backend_integer_type: &'static str,
}

impl SessionMetaCodec {
    pub const fn new(backend_integer_type: &'static str) -> Self {
        Self {
            backend_integer_type,
        }
    }

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
                stored.source_node_id = Some(source_node_id.to_string());
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
                process_id: process_id.into(),
                process_incarnation,
                attribution,
            });
        }
        stored.fork_inheritance_processes =
            self.decode_process_rows(fork_inheritance_rows, "fork inheritance process_index")?;
        self.decode(stored)
    }

    pub fn decode(self, stored: StoredRelation) -> Result<SessionMeta, StoreError> {
        let relation = match stored.relation_kind.as_str() {
            "root" => {
                self.require_empty(
                    &stored.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                if stored.cause.decode(self)?.is_some() {
                    return Err(self.corrupt("root relation carries a causal payload"));
                }
                if stored.parent_session_id.is_some()
                    || stored.source_session_id.is_some()
                    || stored.source_node_id.is_some()
                    || stored.observer_inheritance_kind.is_some()
                {
                    return Err(
                        self.corrupt("root relation carries an out-of-family payload column")
                    );
                }
                SessionRelation::Root
            }
            "child" => {
                self.require_empty(
                    &stored.fork_inheritance_processes,
                    "fork inheritance processes",
                )?;
                if stored.source_session_id.is_some()
                    || stored.source_node_id.is_some()
                    || stored.observer_inheritance_kind.is_some()
                {
                    return Err(
                        self.corrupt("child relation carries an out-of-family payload column")
                    );
                }
                SessionRelation::Child {
                    parent_session_id: self
                        .required(stored.parent_session_id, "parent_session_id")?,
                    caused_by: stored.cause.decode(self)?,
                }
            }
            "fork" => {
                if stored.cause.decode(self)?.is_some() {
                    return Err(self.corrupt("fork relation carries a causal payload"));
                }
                if stored.parent_session_id.is_some() {
                    return Err(
                        self.corrupt("fork relation carries an out-of-family payload column")
                    );
                }
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
                    source_node_id: crate::NodeId::new(
                        self.required(stored.source_node_id, "source_node_id")?,
                    ),
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

    pub fn write_index(self, value: usize, field: &'static str) -> Result<i64, StoreError> {
        i64::try_from(value).map_err(|_| {
            StoreError::Backend(format!(
                "{field} index does not fit {}",
                self.backend_integer_type
            ))
        })
    }

    pub fn read_index(self, value: i64, field: &'static str) -> Result<usize, StoreError> {
        usize::try_from(value)
            .map_err(|_| self.corrupt(format!("{field} must be non-negative, got {value}")))
    }

    /// Decode the durable lineage columns of an existing `session_meta` row.
    ///
    /// Admission compares this against the lineage a rebind declares, so it
    /// reads only the columns that carry lineage: no causal provenance, no
    /// observer-inheritance list, and therefore no extra queries inside the
    /// admission transaction.
    pub fn decode_lineage(
        self,
        relation_kind: &str,
        parent_session_id: Option<SessionId>,
        source_session_id: Option<SessionId>,
        source_node_id: Option<String>,
    ) -> Result<SessionLineage, StoreError> {
        Ok(match relation_kind {
            "root" => SessionLineage::Root,
            "child" => SessionLineage::Child {
                parent_session_id: self.required(parent_session_id, "parent_session_id")?,
            },
            "fork" => SessionLineage::Fork {
                source_session_id: self.required(source_session_id, "source_session_id")?,
                source_node_id: self.required(source_node_id, "source_node_id")?,
            },
            other => return Err(self.corrupt(format!("unknown relation_kind `{other}`"))),
        })
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

    fn require_empty(self, values: &[ProcessId], field: &'static str) -> Result<(), StoreError> {
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
    ) -> Result<Vec<ProcessId>, StoreError> {
        let mut process_ids = Vec::with_capacity(rows.len());
        for (process_index, process_id) in rows {
            if self.read_index(process_index, field)? != process_ids.len() {
                return Err(self.corrupt("process indexes are not contiguous"));
            }
            process_ids.push(ProcessId::from(process_id));
        }
        Ok(process_ids)
    }

    fn read_u64_text(self, value: String, field: &'static str) -> Result<u64, StoreError> {
        value
            .parse()
            .map_err(|_| self.corrupt(format!("{field} is not an unsigned integer: `{value}`")))
    }
}

/// Refuse a rebind whose declared lineage disagrees with the recorded one.
///
/// Every `SessionCommitStore::admit_and_bind_session` implementation calls this
/// on the rebind branch so all backends answer the same conflict with the same
/// typed error (rule 6 of the admission contract).
pub fn guard_rebind_lineage(
    session_id: &SessionId,
    recorded: &SessionLineage,
    requested: &SessionRelation,
) -> Result<(), StoreError> {
    let requested = SessionLineage::of(requested);
    if recorded.rebind_conflicts_with_recorded(&requested) {
        return Err(StoreError::SessionRelationMismatch {
            session_id: session_id.clone(),
            recorded: Box::new(recorded.clone()),
            requested: Box::new(requested),
        });
    }
    Ok(())
}

/// Refuse a metadata write that would replace the recorded session lineage.
///
/// [`SessionCommitStore::save_session_meta`](crate::store::SessionCommitStore::save_session_meta)
/// replaces every relation column of an existing row, so it is the second way
/// a recorded lineage can move. Admission's rebind comparison reads
/// [`SessionLineage::Root`] as "no claim" because a resume declares no
/// lineage; a metadata write cannot, because the row it writes would record
/// that root and drop the recorded parent. The lineage is therefore write-once
/// here: it must match exactly, and only the rest of the record (the pending
/// observer intents the sole production caller settles, and the causal
/// provenance and observer inheritance that are not lineage) may move.
pub fn guard_session_meta_relation_rewrite(
    session_id: &SessionId,
    recorded: &SessionLineage,
    requested: &SessionRelation,
) -> Result<(), StoreError> {
    let requested = SessionLineage::of(requested);
    if *recorded != requested {
        return Err(StoreError::SessionRelationMismatch {
            session_id: session_id.clone(),
            recorded: Box::new(recorded.clone()),
            requested: Box::new(requested),
        });
    }
    Ok(())
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    #[test]
    fn session_relation_effect_cause_refuses_legacy_shape_and_round_trips_address() {
        let codec = SessionMetaCodec::new("test integer");
        let legacy = CausalColumns {
            kind: Some("effect".to_string()),
            session_id: Some(SessionId::from("legacy-session")),
            effect_id: Some("legacy-effect".to_string()),
            ..CausalColumns::default()
        };
        let error = legacy
            .decode(codec)
            .expect_err("legacy session/effect identity must fail closed");
        assert!(error.to_string().contains("effect_identity_format_cutover"));

        let address = crate::EffectAddress::new(
            crate::ExecutionScope::process("admitted-process"),
            "shared-replay-key",
        )
        .expect("valid effect address");
        let cause = CausalRef::Effect {
            address: address.clone(),
        };
        assert_eq!(
            CausalColumns::encode(Some(&cause))
                .decode(codec)
                .expect("current effect address decodes"),
            Some(CausalRef::Effect { address })
        );
    }

    #[test]
    fn causal_columns_refuse_payloads_outside_their_family() {
        let codec = SessionMetaCodec::new("test integer");

        let kindless = CausalColumns {
            session_id: Some(SessionId::from("cause-session")),
            ..CausalColumns::default()
        };
        let error = kindless
            .decode(codec)
            .expect_err("payload without caused_by_kind must fail closed");
        assert!(error.to_string().contains("without caused_by_kind"));

        let unknown = CausalColumns {
            kind: Some("timer".to_string()),
            ..CausalColumns::default()
        };
        let error = unknown
            .decode(codec)
            .expect_err("unknown caused_by_kind must fail closed");
        assert!(error.to_string().contains("unknown caused_by_kind"));

        let crossed = CausalColumns {
            kind: Some("turn".to_string()),
            session_id: Some(SessionId::from("cause-session")),
            turn_id: Some(TurnId::from("cause-turn")),
            node_id: Some("stray-node".to_string()),
            ..CausalColumns::default()
        };
        let error = crossed
            .decode(codec)
            .expect_err("out-of-family payload column must fail closed");
        assert!(
            error
                .to_string()
                .contains("cannot carry `caused_by_node_id`")
        );
    }

    #[test]
    fn non_child_relations_refuse_causal_payloads() {
        let codec = SessionMetaCodec::new("test integer");
        let cause = CausalColumns::encode(Some(&CausalRef::Turn {
            session_id: SessionId::from("cause-session"),
            turn_id: TurnId::from("cause-turn"),
        }));
        let stored = |relation_kind: &str| StoredRelation {
            session_id: SessionId::from("session"),
            relation_kind: relation_kind.to_string(),
            parent_session_id: None,
            cause: CausalColumns {
                kind: cause.kind.clone(),
                session_id: cause.session_id.clone(),
                turn_id: cause.turn_id.clone(),
                ..CausalColumns::default()
            },
            source_session_id: Some(SessionId::from("source-session")),
            source_node_id: Some("source-node".to_string()),
            observer_inheritance_kind: Some("none".to_string()),
            pending_observer_intents: Vec::new(),
            fork_inheritance_processes: Vec::new(),
        };
        let error = codec
            .decode(stored("root"))
            .expect_err("caused root relation must fail closed");
        assert!(
            error
                .to_string()
                .contains("root relation carries a causal payload")
        );
        let error = codec
            .decode(stored("fork"))
            .expect_err("caused fork relation must fail closed");
        assert!(
            error
                .to_string()
                .contains("fork relation carries a causal payload")
        );
    }

    #[test]
    fn relations_refuse_out_of_family_payload_columns() {
        let codec = SessionMetaCodec::new("test integer");
        let stored = |relation_kind: &str| StoredRelation {
            session_id: SessionId::from("session"),
            relation_kind: relation_kind.to_string(),
            parent_session_id: None,
            cause: CausalColumns::default(),
            source_session_id: None,
            source_node_id: None,
            observer_inheritance_kind: None,
            pending_observer_intents: Vec::new(),
            fork_inheritance_processes: Vec::new(),
        };

        let mut root = stored("root");
        root.parent_session_id = Some(SessionId::from("stray-parent"));
        let error = codec
            .decode(root)
            .expect_err("a stray parent on a root relation must fail closed");
        assert!(error.to_string().contains("out-of-family payload"));

        let mut child = stored("child");
        child.parent_session_id = Some(SessionId::from("parent"));
        child.source_session_id = Some(SessionId::from("stray-source"));
        let error = codec
            .decode(child)
            .expect_err("a stray fork-source on a child relation must fail closed");
        assert!(error.to_string().contains("out-of-family payload"));

        let mut fork = stored("fork");
        fork.source_session_id = Some(SessionId::from("source"));
        fork.source_node_id = Some("source-node".to_string());
        fork.observer_inheritance_kind = Some("none".to_string());
        fork.parent_session_id = Some(SessionId::from("stray-parent"));
        let error = codec
            .decode(fork)
            .expect_err("a stray parent on a fork relation must fail closed");
        assert!(error.to_string().contains("out-of-family payload"));

        codec
            .decode(stored("root"))
            .expect("a clean root relation decodes");
        let mut child = stored("child");
        child.parent_session_id = Some(SessionId::from("parent"));
        codec.decode(child).expect("a clean child relation decodes");
        let mut fork = stored("fork");
        fork.source_session_id = Some(SessionId::from("source"));
        fork.source_node_id = Some("source-node".to_string());
        fork.observer_inheritance_kind = Some("none".to_string());
        codec.decode(fork).expect("a clean fork relation decodes");
    }

    #[test]
    fn causal_u64_text_columns_round_trip_the_full_range() {
        let codec = SessionMetaCodec::new("test integer");
        let cause = CausalRef::ProcessEvent {
            process_id: ProcessId::from("process"),
            sequence: u64::MAX,
        };
        assert_eq!(
            CausalColumns::encode(Some(&cause))
                .decode(codec)
                .expect("u64::MAX process event sequence decodes"),
            Some(cause)
        );
        let cause = CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence".to_string(),
            subscription_id: None,
            subscription_incarnation: None,
            subscription_revision: Some(u64::MAX),
        };
        assert_eq!(
            CausalColumns::encode(Some(&cause))
                .decode(codec)
                .expect("u64::MAX subscription revision decodes"),
            Some(cause)
        );
    }
}
