//! `history`: the lash-provided projection over the session transcript
//! (ADR 0132 §9).
//!
//! A cell's `history` binding is plain data, a [`ResourceRef`] naming the
//! session and the transcript revision the cell saw. Every run gets a provider
//! over its turn's transcript, so whichever node runs the session answers a
//! read of any revision that transcript still holds. Within one agent frame
//! the transcript only grows, so the revision is the frame and the number of
//! entries the cell saw, and the history of an earlier revision is the
//! history of that prefix: a value a session global kept from an earlier cell
//! reads exactly what it read then. A revision of another frame, or past the
//! transcript, is refused rather than answered from a different transcript.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_core::facade_support::ChronologicalProjection;
use lash_vm::{
    ProjectedReadRequest, ProjectedReadResponse, ProjectionError, ProjectionProvider,
    ProjectionType, ResourceRef, Value as FlowValue,
};

use super::context::{RlmHistoryProjection, projected_index};
use super::transport::json_to_flow_value;

/// The projection type `history` is read through. Reserved: no host
/// provider may register it.
pub const HISTORY_PROJECTION: &str = "history";

/// The declared type a `history` value has in the VM.
const HISTORY_TYPE_NAME: &str = "list";

/// The provider of one session's `history`, over the transcript a run
/// started from.
pub(crate) struct HistoryProvider {
    session: String,
    frame: String,
    transcript: Arc<ChronologicalProjection>,
    /// Histories already built, by the transcript length they cover. A
    /// cache of pure derivations, never a source.
    histories: Mutex<BTreeMap<usize, Arc<RlmHistoryProjection>>>,
}

impl HistoryProvider {
    /// The provider over `transcript`, the session's current agent frame.
    pub(crate) fn new(
        session: impl Into<String>,
        frame: impl Into<String>,
        transcript: Arc<ChronologicalProjection>,
    ) -> Self {
        Self {
            session: session.into(),
            frame: frame.into(),
            transcript,
            histories: Mutex::new(BTreeMap::new()),
        }
    }

    /// The resource of the history this transcript has now.
    pub(crate) fn current(&self) -> ResourceRef {
        ResourceRef {
            projection: ProjectionType::new(HISTORY_PROJECTION),
            id: self.session.clone(),
            revision: Some(revision(&self.frame, self.transcript.entries().len())),
        }
    }

    /// The `history` value of [`Self::current`].
    pub(crate) fn binding(&self) -> lash_vm::ProjectedValue {
        lash_vm::ProjectedValue::resource(HISTORY_PROJECTION, HISTORY_TYPE_NAME, self.current())
    }

    /// The history `resource` pins.
    ///
    /// # Errors
    ///
    /// A resource of another session or frame, one past this transcript, or a
    /// transcript that does not decode.
    pub(crate) fn history(
        &self,
        resource: &ResourceRef,
    ) -> Result<Arc<RlmHistoryProjection>, ProjectionError> {
        let refuse = |message: String| ProjectionError { message };
        if resource.id != self.session {
            return Err(refuse(format!(
                "history of session `{}` is not this session's",
                resource.id
            )));
        }
        let pinned = resource
            .revision
            .as_deref()
            .ok_or_else(|| refuse("history is read at a pinned revision".into()))?;
        let length = pinned
            .strip_prefix(self.frame.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            .and_then(|length| length.parse::<usize>().ok())
            .filter(|length| *length <= self.transcript.entries().len())
            .ok_or_else(|| {
                refuse(format!(
                    "history revision `{pinned}` is not retained by this transcript"
                ))
            })?;
        let mut histories = self
            .histories
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(history) = histories.get(&length) {
            return Ok(Arc::clone(history));
        }
        let history = Arc::new(
            RlmHistoryProjection::from_entries(&self.transcript.entries()[..length])
                .map_err(|error| refuse(error.to_string()))?,
        );
        histories.insert(length, Arc::clone(&history));
        Ok(history)
    }
}

fn revision(frame: &str, length: usize) -> String {
    format!("{frame}/{length}")
}

#[async_trait::async_trait]
impl ProjectionProvider for HistoryProvider {
    fn projection_type(&self) -> ProjectionType {
        ProjectionType::new(HISTORY_PROJECTION)
    }

    async fn read(
        &self,
        resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionError> {
        let history = self.history(resource)?;
        Ok(answer(&history, request))
    }

    async fn read_range(
        &self,
        resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionError> {
        let history = self.history(resource)?;
        Ok(requests
            .into_iter()
            .map(|request| answer(&history, request))
            .collect())
    }
}

/// `contains(history, x)` compares `x` against each entry's projected value,
/// the same shape `history[i]` hands back, so the two agree.
fn contains(history: &RlmHistoryProjection, needle: &FlowValue) -> bool {
    (0..history.len())
        .filter_map(|index| history.item(index))
        .filter_map(|item| serde_json::to_value(item).ok())
        .map(json_to_flow_value)
        .any(|item| &item == needle)
}

/// What `history` answers to `request`; `None` for what it does not answer.
pub(crate) fn answer(
    history: &RlmHistoryProjection,
    request: ProjectedReadRequest,
) -> Option<ProjectedReadResponse> {
    match request {
        ProjectedReadRequest::Len => Some(ProjectedReadResponse::Len(history.len())),
        ProjectedReadRequest::Index(index) => {
            let Ok(Some(index)) = projected_index(&index, history.len()) else {
                return None;
            };
            history
                .item(index)
                .and_then(|item| serde_json::to_value(item).ok())
                .map(json_to_flow_value)
                .map(ProjectedReadResponse::Value)
        }
        // `Empty`, `Truthy`, `Keys` and `Contains` below are reachable
        // through the lash_vm intrinsics (`empty`, truthiness, `keys`,
        // `contains`) and through any host that reads the provider directly.
        // TypeScript source reaches none of them: it has no `empty`, its
        // array methods lower to their own operations rather than these
        // hooks, and `Object.keys` materializes first. They are answered, not
        // dead.
        //
        // A list is empty exactly when it has no entries and truthy whatever
        // its length, matching the dialect's own reading of a `Value::List`.
        // Answering both here keeps `if (history)` and `empty(history)` off
        // the materializing path (FIG-2863).
        ProjectedReadRequest::Empty => Some(ProjectedReadResponse::Bool(history.is_empty())),
        ProjectedReadRequest::Truthy => Some(ProjectedReadResponse::Bool(true)),
        // `keys` over a list is the dialect's empty key set, not an
        // unanswerable request: a list has no named fields.
        ProjectedReadRequest::Keys => Some(ProjectedReadResponse::Keys(Vec::new())),
        ProjectedReadRequest::Contains(needle) => {
            Some(ProjectedReadResponse::Bool(contains(history, &needle)))
        }
        // `history.length` is the one field a list answers; every other field
        // is unanswerable and says so rather than degrading.
        ProjectedReadRequest::Field(field) if field.as_ref() == "length" => {
            Some(ProjectedReadResponse::Len(history.len()))
        }
        ProjectedReadRequest::Render => Some(ProjectedReadResponse::Text(
            serde_json::to_string(history.history()).unwrap_or_else(|_| "[]".to_string()),
        )),
        ProjectedReadRequest::Materialize => Some(ProjectedReadResponse::Value(
            json_to_flow_value(history.value()),
        )),
        // Everything else `history` does not answer. The caller turns that
        // into a typed refusal instead of a widened guess.
        ProjectedReadRequest::Field(_)
        | ProjectedReadRequest::Find { .. }
        | ProjectedReadRequest::GrepText(_)
        | ProjectedReadRequest::Values
        | ProjectedReadRequest::StartsWith(_)
        | ProjectedReadRequest::EndsWith(_)
        | ProjectedReadRequest::Split(_)
        | ProjectedReadRequest::Join(_)
        | ProjectedReadRequest::Trim
        | ProjectedReadRequest::Slice { .. }
        | ProjectedReadRequest::Push(_)
        | ProjectedReadRequest::ToNumber
        | ProjectedReadRequest::JsonParse
        | ProjectedReadRequest::SliceBound
        | ProjectedReadRequest::RangeBound => None,
    }
}
