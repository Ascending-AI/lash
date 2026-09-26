//! The process-listing selector: who started a row, which parent
//! scope it belongs to, and whether it owes a cancel.

use super::*;

/// Selects rows by who started them, with the frame narrowing the
/// [`ProcessOriginator`] enum can express but a single id string cannot.
///
/// This is a selector, not an originator: `Session` with no
/// `agent_frame_id` selects every process the session started, in any frame,
/// while an originator with no frame records a process bound to none. A
/// frame-scoped selector matches only rows recorded in that frame. `Host`
/// compares its scope by equality, so `Host { scope: None }` selects the
/// unscoped host rows exactly as the `"host"` id string used to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProcessOriginatorFilter {
    Host {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    Session(SessionScope),
}

impl ProcessOriginatorFilter {
    pub fn session(session_id: impl Into<SessionId>) -> Self {
        Self::Session(SessionScope::new(session_id))
    }

    /// The `originator_id` column value this selector narrows to.
    ///
    /// Every tier stores the originator as [`ProcessOriginator::id`], which
    /// carries the host scope or the session id but never the frame, so this
    /// is an index-served narrowing and never the whole predicate:
    /// [`Self::matches`] still decides the frame.
    pub fn originator_id(&self) -> String {
        match self {
            Self::Host { scope } => ProcessOriginator::Host {
                scope: scope.clone(),
            }
            .id(),
            Self::Session(scope) => scope.session_id.to_string(),
        }
    }

    pub fn matches(&self, originator: &ProcessOriginator) -> bool {
        match (self, originator) {
            (Self::Host { scope }, ProcessOriginator::Host { scope: recorded }) => {
                scope == recorded
            }
            (
                Self::Session(scope),
                ProcessOriginator::Session {
                    session_id,
                    agent_frame_id,
                },
            ) => {
                scope.session_id == *session_id
                    && scope
                        .agent_frame_id
                        .as_ref()
                        .is_none_or(|frame| agent_frame_id.as_ref() == Some(frame))
            }
            (Self::Host { .. }, ProcessOriginator::Session { .. })
            | (Self::Session(_), ProcessOriginator::Host { .. }) => false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessListFilter {
    /// Engine-owned process definition value, compared verbatim against the
    /// definition a record's reference names. The core owns no encoding here: a
    /// caller passes the same value the engine that started the run stores, so a
    /// caller holding a definition can filter by it directly. The reference's
    /// signature is deliberately excluded: it is resolved authority about the
    /// same definition, not part of what names it.
    pub definition: Option<super::ProcessDefinitionValue>,
    pub status: ProcessStatusFilter,
    pub originator: Option<ProcessOriginatorFilter>,
    /// Selects the processes that live until one scope, compared by the same
    /// `(kind, id)` pair the stores index.
    pub until: Option<super::ScopeId>,
    /// Selects nonterminal rows whose cancellation request was recorded
    /// strictly before this timestamp. `caller_departed` is nonterminal and
    /// is selected: nothing may terminalize such a row, so a cancel request
    /// on it stays unanswered and is exactly what this filter is for.
    pub cancel_pending_before_ms: Option<u64>,
    pub identity_kind: Option<String>,
    pub identity_label: Option<String>,
    pub caused_by_occurrence_id: Option<String>,
    pub caused_by_subscription_id: Option<String>,
    /// Inclusive lower bound for `created_at_ms`; paired with
    /// `created_at_end_ms` this is a half-open `[start, end)` range.
    pub created_at_start_ms: Option<u64>,
    /// Exclusive upper bound for `created_at_ms`; paired with
    /// `created_at_start_ms` this is a half-open `[start, end)` range.
    pub created_at_end_ms: Option<u64>,
    /// Inclusive lower bound for `updated_at_ms` on retired rows. Live rows
    /// remain eligible regardless of age, so `status: Any` answers a bounded
    /// "live plus recently retired" poll in one store query.
    pub retired_since_ms: Option<u64>,
}

impl ProcessListFilter {
    /// Parses the complete process-list filter for store implementors, rejecting unknown fields and
    /// ill-typed values rather than silently ignoring them.
    pub fn decode(args: &serde_json::Value) -> Result<Self, String> {
        let map = args
            .as_object()
            .ok_or_else(|| "processes.list expects a record of process filters".to_string())?;
        for key in map.keys() {
            match key.as_str() {
                "definition"
                | "status"
                | "originator"
                | "until"
                | "cancel_pending_before_ms"
                | "identity_kind"
                | "identity_label"
                | "caused_by_occurrence_id"
                | "caused_by_subscription_id"
                | "created_at_start_ms"
                | "created_at_end_ms"
                | "retired_since_ms" => {}
                _ => return Err(format!("processes.list unknown filter `{key}`")),
            }
        }
        // Taken verbatim: the definition value is whichever encoding the engine
        // that started the process stores, and `matches_record` compares the two
        // by equality. Normalizing here would reintroduce a second encoding.
        let definition = args
            .get("definition")
            .cloned()
            .map(super::ProcessDefinitionValue::new);
        let status = ProcessStatusFilter::decode(args.get("status"))?;
        let originator = args
            .get("originator")
            .map(|value| {
                serde_json::from_value::<ProcessOriginatorFilter>(value.clone())
                    .map_err(|error| format!("processes.list invalid originator filter: {error}"))
            })
            .transpose()?;
        let until = args
            .get("until")
            .map(|value| {
                serde_json::from_value::<super::ScopeId>(value.clone())
                    .map_err(|error| format!("processes.list invalid until filter: {error}"))
            })
            .transpose()?;
        let cancel_pending_before_ms = optional_u64_filter(args, "cancel_pending_before_ms")?;
        let identity_kind = optional_string_filter(args, "identity_kind")?;
        let identity_label = optional_string_filter(args, "identity_label")?;
        let caused_by_occurrence_id = optional_string_filter(args, "caused_by_occurrence_id")?;
        let caused_by_subscription_id = optional_string_filter(args, "caused_by_subscription_id")?;
        let created_at_start_ms = optional_u64_filter(args, "created_at_start_ms")?;
        let created_at_end_ms = optional_u64_filter(args, "created_at_end_ms")?;
        let retired_since_ms = optional_u64_filter(args, "retired_since_ms")?;
        Ok(Self {
            definition,
            status,
            originator,
            until,
            cancel_pending_before_ms,
            identity_kind,
            identity_label,
            caused_by_occurrence_id,
            caused_by_subscription_id,
            created_at_start_ms,
            created_at_end_ms,
            retired_since_ms,
        })
    }

    /// Exposes list mode to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution.
    pub fn list_mode(&self) -> ProcessListMode {
        self.status.list_mode()
    }

    pub fn matches_record(&self, record: &ProcessRecord) -> bool {
        self.status.matches(record.status)
            && self.definition.as_ref().is_none_or(|definition| {
                record
                    .identity
                    .definition
                    .as_ref()
                    .is_some_and(|reference| &reference.definition == definition)
            })
            && self
                .originator
                .as_ref()
                .is_none_or(|originator| originator.matches(&record.provenance.originator))
            && self
                .until
                .as_ref()
                .is_none_or(|scope| record.lifetime.scope() == Some(scope))
            && self.cancel_pending_before_ms.is_none_or(|before_ms| {
                !record.status.is_terminal()
                    && record
                        .cancel_request
                        .as_ref()
                        .is_some_and(|request| request.requested_at_ms < before_ms)
            })
            && self
                .identity_kind
                .as_ref()
                .is_none_or(|kind| record.identity.kind.as_str() == kind.as_str())
            && self
                .identity_label
                .as_ref()
                .is_none_or(|label| record.identity.label.as_deref() == Some(label.as_str()))
            && self
                .caused_by_occurrence_id
                .as_ref()
                .is_none_or(|occurrence_id| caused_by_occurrence_matches(record, occurrence_id))
            && self
                .caused_by_subscription_id
                .as_ref()
                .is_none_or(|subscription_id| {
                    caused_by_subscription_matches(record, subscription_id)
                })
            && self
                .created_at_start_ms
                .is_none_or(|start_ms| record.created_at_ms >= start_ms)
            && self
                .created_at_end_ms
                .is_none_or(|end_ms| record.created_at_ms < end_ms)
            && self.retired_since_ms.is_none_or(|since_ms| {
                !record.status.is_retired() || record.updated_at_ms >= since_ms
            })
    }
}

fn optional_string_filter(args: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    args.get(key)
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("processes.list `{key}` filter must be a string"))
        })
        .transpose()
}

fn optional_u64_filter(args: &serde_json::Value, key: &str) -> Result<Option<u64>, String> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| format!("processes.list `{key}` filter must be an integer"))
        })
        .transpose()
}

fn caused_by_occurrence_matches(record: &ProcessRecord, occurrence_id: &str) -> bool {
    matches!(
        record.provenance.caused_by.as_ref(),
        Some(crate::CausalRef::TriggerOccurrence { occurrence_id: actual, .. }) if actual == occurrence_id
    )
}

fn caused_by_subscription_matches(record: &ProcessRecord, subscription_id: &str) -> bool {
    matches!(
        record.provenance.caused_by.as_ref(),
        Some(crate::CausalRef::TriggerOccurrence {
            subscription_id: Some(actual),
            ..
        }) if actual == subscription_id
    )
}
