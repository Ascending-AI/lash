#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by Lash durable waits"
)]

//! Restate-native durable effect groups.
//!
//! The index owns group lifecycle and settlement rank, the payload object owns
//! successful result bytes and its object-local retirement fence, and the
//! dispatch workflow owns every child send. READY, RANK, CANCEL, and ADMIT use
//! the existing durable-wait services so resolution-before-registration and
//! retained terminal resolutions have one implementation.

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, GroupExecutors, GroupSettlement,
    LoserPolicy, Resolution, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectOutcome, RuntimeErrorCode,
};
use restate_sdk::context::{
    CallFuture, ContextClient, ContextSideEffects, ContextWriteState, ObjectContext, RunFuture,
    RunRetryPolicy, SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::RestateIngressClient;
use crate::durable_wait::{
    LASH_REPLAY_KEY_HEADER, LashDurableWaitIndexClient, LashDurableWaitWorkflowClient,
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest, RestateDurableWaitGroupChildRequest,
    RestateDurableWaitResolveRequest, durable_wait_index_key_for_scope,
    durable_wait_index_object_key, restate_await_event_key,
};
use crate::object_state::{self, StoredValueFormats};

const INDEX_STATE_KEY: &str = "effect-group/v1/state";

#[cfg(test)]
static ADMISSION_WITNESSES: std::sync::OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>> =
    std::sync::OnceLock::new();

mod drain_barrier;
mod group_waits;
mod protocol;
mod reopen;
mod wire;
use drain_barrier::blocking_positions;
pub(crate) use drain_barrier::{drained_wait_lifted, drained_wait_request};
use group_waits::{resolve_group_wait, seal_cancel_decisions, wait_resolution};
pub(crate) use protocol::EFFECT_GROUP_STATE_FORMATS;
pub use protocol::{
    EFFECT_GROUP_DISPATCH_JOURNAL_VERSION, EFFECT_GROUP_STATE_FORMAT_VERSION,
    EFFECT_GROUP_WIRE_VERSION,
};
use protocol::{load_index, load_index_shared};
pub(crate) use reopen::{content_checked_shape_mismatch, content_mismatch};
pub(crate) use wire::btree_map_as_pairs;
pub use wire::{
    EffectGroupAdmitSemanticRequest, EffectGroupAdmitSemanticResponse, EffectGroupPhase,
    EffectGroupProbeResponse,
};

mod shape;
pub use shape::EffectGroupShape;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupDispatchState {
    Unadopted,
    Adopted {
        id: String,
        #[serde(with = "btree_map_as_pairs")]
        dispatched: BTreeMap<usize, String>,
    },
}

mod index_record;
pub use index_record::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupOpenResponse {
    OpenedFresh,
    ReopenedReady,
    ReopenedPreparing,
    ReopenedClosed {
        effective: EffectGroupCloseDisposition,
    },
    Retired,
    ShapeMismatch,
    /// A content-checked reopen offered a child that is not the retained one
    /// at `position`.
    ContentMismatch {
        position: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupProbeAdoptResponse {
    /// The dispatch was adopted; `shape` is the recorded shape whose retained
    /// membership is the authoritative child set — a reopen's offered
    /// children never reach dispatch.
    Adopted {
        shape: EffectGroupShape,
    },
    /// A redrive of an already-adopted dispatch, answered with the same
    /// recorded shape so the replayed `run` rebuilds the same children.
    AlreadyAdopted {
        shape: EffectGroupShape,
    },
    DifferentDispatcher,
    Ready,
    Closed,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRecordDispatchResponse {
    Recorded,
    Duplicate,
    DispatchMismatch,
    NotPreparing,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRegisterResponse {
    Registered,
    AlreadyRegistered,
    RegistrationMismatch,
    AlreadyClosed,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRegisterRefusalResponse {
    Refused,
    AlreadyRegistered,
    AlreadyClosed,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupAdmissionResponse {
    Admitted,
    NotYetRecorded,
    /// The index retains a *different* invocation id for this position: the
    /// retained invocation's retention expired and the idempotency-keyed
    /// re-dispatch minted a fresh one. Distinct from `Refused` because the
    /// successor must surface the typed `AttachExpired` failure rather than
    /// exit silently — the rank it would never settle is a caller's wait
    /// (ADR 0099 §8).
    AttachExpired,
    /// The close decided this child `Cancel` before it was admitted. The
    /// close owns everything that follows, including releasing a wait
    /// child's wait (ADR 0099 §12, FIG-3630), so the child just exits.
    CancelDecided,
    Refused,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRecordSettlementResponse {
    Recorded {
        rank: u64,
    },
    Duplicate {
        rank: u64,
    },
    /// Refused: the cancel disposition won this child's §4 point before its
    /// final record arrived. `rank` is the seat the decision holds — the
    /// child's own terminal journals nothing.
    CancelDecided {
        rank: u64,
    },
    UnknownChild,
    UnknownGroup,
    Retired,
}

/// One child's final record reaching the §4 point: the index-side decision
/// the durable tiers' `commit_group_child` mirrors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupCommitChildRequest {
    /// The child's declared replay key; the index resolves its position from
    /// the retained shape rather than trusting a caller-supplied position.
    pub replay_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCommitChildResponse {
    /// This child's final won the §4 point: `commit_seq` is its durable
    /// position in the group's final-commit order and `blocking_positions`
    /// the committed siblings below it whose seats are still owed — the §5
    /// barrier the child's drain and settlement wait behind.
    Committed {
        commit_seq: u64,
        blocking_positions: Vec<usize>,
    },
    /// The commit already landed (idempotent redrive): the recorded position
    /// and whichever lower siblings still owe their seats.
    AlreadyCommitted {
        commit_seq: u64,
        blocking_positions: Vec<usize>,
    },
    /// The cancel disposition won first; the child's final journals nothing.
    CancelDecided {
        rank: u64,
    },
    UnknownChild,
    UnknownGroup,
    Retired,
}

/// The §5 barrier read: which siblings committed below `commit_seq` still owe
/// their settlement seats.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupDrainBlockersRequest {
    pub commit_seq: u64,
}

/// The §5 barrier as the index sees it for one committed child.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupDrainBlockersResponse {
    /// No sibling below the caller still owes its seat. An absent or retired
    /// group holds no committed children, so it answers this too.
    Admitted,
    /// These lower-commit siblings still owe their seats. Each resolves its
    /// drained wake under the group's retained `wait_scope` when it seats, so
    /// the caller builds the wake keys from the scope the index resolves them
    /// under rather than re-deriving it.
    Blocked {
        wait_scope: ExecutionScope,
        positions: Vec<usize>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupReadRankResponse {
    Settled {
        settlement: EffectGroupSettlementRecord,
        /// The settled child's durable identity — the replay key a §6 prefix
        /// record or a §8 attach names, derived from the retained shape rather
        /// than stored twice on the settlement record.
        child_replay_key: String,
    },
    NotSettled,
    Closed,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCloseResponse {
    Closed,
    AlreadyClosed,
    WidenRefused,
    NotReady,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRetireResponse {
    Retired { cleanup: EffectGroupCleanupFacts },
    AlreadyRetired { cleanup: EffectGroupCleanupFacts },
    Tombstone,
    UnknownGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupFinishRetirementResponse {
    Finished,
    AlreadyFinished,
    NotRetired,
    UnknownGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRetirementCancelResponse {
    Applied,
    AlreadyApplied,
    Tombstone,
    NotRetired,
    UnknownGroup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupOpenRequest {
    pub shape: EffectGroupShape,
    /// The opener asked for a content-checked reopen (FIG-3586,
    /// `GroupReopen::RetainedContent`): a reopen whose offered membership
    /// differs from the retained one is refused rather than served.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_checked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupAdoptRequest {
    pub invocation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupRecordDispatchRequest {
    pub position: usize,
    pub invocation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupRegisterRequest {
    #[serde(with = "btree_map_as_pairs")]
    pub addresses: BTreeMap<usize, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupRefusalRequest {
    pub reason: EffectGroupRefusal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupAdmissionRequest {
    pub position: usize,
    pub invocation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupRecordSettlementRequest {
    pub position: usize,
    pub terminal: EffectGroupSettlementTerminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupReadRankRequest {
    pub rank: u64,
    /// The read is the group caller's own await. A caller is refused a
    /// closed group's ranks until it reopens the group, as the SQL engines'
    /// caller path refuses (FIG-3676); the host's own readers (drain,
    /// finalization, the settlement reader) see every recorded rank.
    #[serde(default)]
    pub for_caller: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupCloseRequest {
    pub disposition: LoserPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupWaitResolution {
    Ready,
    Rank,
    Cancel,
    Admit,
    /// The child at this position seated its settlement: the wake a §5
    /// barrier parks on while a lower-commit sibling finishes its drain.
    Drained,
    /// The child at this position seated its settlement, so no cancel can
    /// reach it: its cancel wait ends without cancelling it, and the watch
    /// its dispatch invocation held on the wait ends with it (FIG-3709).
    Settled,
    Refused {
        reason: EffectGroupRefusal,
    },
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectGroupWaitKind<'a> {
    Ready,
    Rank(u64),
    Cancel(&'a str),
    Admit(usize),
    Drained(usize),
}

fn group_wait_key(
    scope: &ExecutionScope,
    group_key: &str,
    kind: EffectGroupWaitKind<'_>,
) -> Result<AwaitEventKey, TerminalError> {
    let suffix = match kind {
        EffectGroupWaitKind::Ready => "ready".to_string(),
        EffectGroupWaitKind::Rank(rank) => format!("rank:{rank}"),
        EffectGroupWaitKind::Cancel(replay_key) => format!("cancel:{replay_key}"),
        EffectGroupWaitKind::Admit(position) => format!("admit:{position}"),
        EffectGroupWaitKind::Drained(position) => format!("drained:{position}"),
    };
    restate_await_event_key(
        scope,
        AwaitEventWaitIdentity::Custom {
            key: format!("effect-group:{group_key}:{suffix}"),
        },
    )
    .map_err(|error| TerminalError::new(error.to_string()))
}

pub(crate) fn ready_wait_request(
    scope: &ExecutionScope,
    group_key: &str,
) -> Result<RestateDurableWaitAwaitRequest, RuntimeEffectControllerError> {
    group_wait_key(scope, group_key, EffectGroupWaitKind::Ready)
        .map(|key| RestateDurableWaitAwaitRequest {
            key,
            deadline: None,
        })
        .map_err(|error| group_shape_error(error.to_string()))
}

pub(crate) fn rank_wait_request(
    scope: &ExecutionScope,
    group_key: &str,
    rank: u64,
) -> Result<RestateDurableWaitAwaitRequest, RuntimeEffectControllerError> {
    group_wait_key(scope, group_key, EffectGroupWaitKind::Rank(rank))
        .map(|key| RestateDurableWaitAwaitRequest {
            key,
            deadline: None,
        })
        .map_err(|error| group_shape_error(error.to_string()))
}

#[cfg(test)]
pub(crate) fn admit_wait_request(
    scope: &ExecutionScope,
    group_key: &str,
    position: usize,
) -> Result<RestateDurableWaitAwaitRequest, RuntimeEffectControllerError> {
    group_wait_key(scope, group_key, EffectGroupWaitKind::Admit(position))
        .map(|key| RestateDurableWaitAwaitRequest {
            key,
            deadline: None,
        })
        .map_err(|error| group_shape_error(error.to_string()))
}

#[cfg(test)]
pub(crate) fn cancel_wait_request(
    scope: &ExecutionScope,
    group_key: &str,
    replay_key: &str,
) -> Result<RestateDurableWaitAwaitRequest, TerminalError> {
    group_wait_key(scope, group_key, EffectGroupWaitKind::Cancel(replay_key)).map(|key| {
        RestateDurableWaitAwaitRequest {
            key,
            deadline: None,
        }
    })
}

#[cfg(test)]
pub(crate) fn arm_admission_witness(group_key: &str) -> Arc<tokio::sync::Notify> {
    let hook = Arc::new(tokio::sync::Notify::new());
    ADMISSION_WITNESSES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(group_key.to_owned(), Arc::clone(&hook));
    hook
}

#[cfg(test)]
fn notify_admission_witness(group_key: &str) {
    if let Some(hook) = ADMISSION_WITNESSES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(group_key)
    {
        hook.notify_one();
    }
}

fn phase(lifecycle: &EffectGroupLifecycle) -> EffectGroupPhase {
    match lifecycle {
        EffectGroupLifecycle::Preparing { .. } => EffectGroupPhase::Preparing,
        EffectGroupLifecycle::Ready { .. } => EffectGroupPhase::Ready,
        EffectGroupLifecycle::Closed { .. } => EffectGroupPhase::Closed,
        EffectGroupLifecycle::Retired { .. } => EffectGroupPhase::Retired,
    }
}

fn store_index(ctx: &ObjectContext<'_>, record: EffectGroupIndexRecord) {
    object_state::set_stamped(ctx, INDEX_STATE_KEY, &EFFECT_GROUP_STATE_FORMATS, record);
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EffectGroupIndex;

#[restate_sdk::object(name = "EffectGroupIndex")]
impl EffectGroupIndex {
    #[handler]
    async fn probe(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> HandlerResult<Json<EffectGroupProbeResponse>> {
        let response = match load_index_shared(&ctx).await? {
            None => EffectGroupProbeResponse::Absent,
            Some(record) => EffectGroupProbeResponse::Exists {
                shape_digest: record.shape_digest,
                phase: phase(&record.lifecycle),
            },
        };
        Ok(Json(response))
    }

    /// How many of this group's children have no settlement yet: the count
    /// the owning scope's quiescence proof reads (FIG-2499). An absent or
    /// retired group, or one whose live record is gone, has none.
    #[handler]
    async fn unsettled_children(&self, ctx: SharedObjectContext<'_>) -> HandlerResult<Json<usize>> {
        let unsettled = match load_index_shared(&ctx).await? {
            Some(record) => record.live().map_or(0, |live| {
                live.shape
                    .children()
                    .saturating_sub(live.settled_positions.len())
            }),
            None => 0,
        };
        Ok(Json(unsettled))
    }

    #[handler]
    async fn open(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupOpenRequest>,
    ) -> HandlerResult<Json<EffectGroupOpenResponse>> {
        request.shape.validate_wire()?;
        let Some(mut record) = load_index(&ctx).await? else {
            let shape_digest = request.shape.digest()?;
            store_index(
                &ctx,
                EffectGroupIndexRecord {
                    shape_digest,
                    lifecycle: EffectGroupLifecycle::Preparing {
                        dispatch: EffectGroupDispatchState::Unadopted,
                        live: EffectGroupIndexLiveRecord {
                            shape: request.shape,
                            next_rank: 1,
                            next_commit_seq: 1,
                            commit_states: BTreeMap::new(),
                            settlements: BTreeMap::new(),
                            settled_positions: BTreeMap::new(),
                        },
                    },
                },
            );
            return Ok(Json(EffectGroupOpenResponse::OpenedFresh));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupOpenResponse::Retired));
        }
        // The fence is the shared reopen contract — arity, wake rule, declared
        // disposition — never the retained membership: a reopen *offers*
        // children that may disagree with what the journal kept, and the
        // recorded membership wins (ADR 0099 §3).
        if !record.live()?.shape.fences_equivalent(&request.shape) {
            return Ok(Json(EffectGroupOpenResponse::ShapeMismatch));
        }
        // A content-checked reopen (FIG-3586) is refused when any offered
        // child is not the retained one: the aggregate is addressed by its
        // issue ordinal, so a redrive with different leaves reaches this key.
        if request.content_checked
            && let Some(position) = record
                .live()?
                .shape
                .membership
                .iter()
                .zip(&request.shape.membership)
                .position(|(retained, offered)| retained != offered)
        {
            return Ok(Json(EffectGroupOpenResponse::ContentMismatch { position }));
        }
        // A reopen is a new caller interest (FIG-3481): a non-refused Closed
        // entry keeps its cumulative disposition but its reopened marker
        // re-enables rank reads — the flag clear the SQL entries take.
        let mut marked = false;
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => EffectGroupOpenResponse::ReopenedPreparing,
            EffectGroupLifecycle::Ready { .. } => EffectGroupOpenResponse::ReopenedReady,
            EffectGroupLifecycle::Closed {
                effective,
                reopened,
                ..
            } => {
                if !matches!(effective, EffectGroupCloseDisposition::Refused { .. }) && !*reopened {
                    *reopened = true;
                    marked = true;
                }
                EffectGroupOpenResponse::ReopenedClosed {
                    effective: effective.clone(),
                }
            }
            EffectGroupLifecycle::Retired { .. } => EffectGroupOpenResponse::Retired,
        };
        if marked {
            store_index(&ctx, record);
        }
        Ok(Json(response))
    }

    #[handler]
    async fn probe_and_adopt(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupAdoptRequest>,
    ) -> HandlerResult<Json<EffectGroupProbeAdoptResponse>> {
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupProbeAdoptResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupProbeAdoptResponse::Retired));
        }
        // Read before the mutable match below: the adopting dispatcher gets
        // the recorded shape so its children are always the retained
        // membership, never the envelopes a reopen offered.
        let shape = record.live()?.shape.clone();
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing { dispatch, .. } => match dispatch {
                EffectGroupDispatchState::Unadopted => {
                    *dispatch = EffectGroupDispatchState::Adopted {
                        id: request.invocation_id,
                        dispatched: BTreeMap::new(),
                    };
                    store_index(&ctx, record);
                    EffectGroupProbeAdoptResponse::Adopted { shape }
                }
                EffectGroupDispatchState::Adopted { id, .. } if id == &request.invocation_id => {
                    EffectGroupProbeAdoptResponse::AlreadyAdopted { shape }
                }
                EffectGroupDispatchState::Adopted { .. } => {
                    EffectGroupProbeAdoptResponse::DifferentDispatcher
                }
            },
            EffectGroupLifecycle::Ready { .. } => EffectGroupProbeAdoptResponse::Ready,
            EffectGroupLifecycle::Closed { .. } => EffectGroupProbeAdoptResponse::Closed,
            EffectGroupLifecycle::Retired { .. } => EffectGroupProbeAdoptResponse::Retired,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn record_dispatch(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupRecordDispatchRequest>,
    ) -> HandlerResult<Json<EffectGroupRecordDispatchResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupRecordDispatchResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupRecordDispatchResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let children = shape.children();
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
                ..
            } => match dispatched.get(&request.position) {
                Some(existing) if existing == &request.invocation_id => {
                    EffectGroupRecordDispatchResponse::Duplicate
                }
                Some(_) => EffectGroupRecordDispatchResponse::DispatchMismatch,
                None if request.position >= children => {
                    EffectGroupRecordDispatchResponse::DispatchMismatch
                }
                None => {
                    dispatched.insert(request.position, request.invocation_id);
                    store_index(&ctx, record.clone());
                    EffectGroupRecordDispatchResponse::Recorded
                }
            },
            EffectGroupLifecycle::Preparing { .. } => {
                EffectGroupRecordDispatchResponse::DispatchMismatch
            }
            EffectGroupLifecycle::Ready { .. } | EffectGroupLifecycle::Closed { .. } => {
                EffectGroupRecordDispatchResponse::NotPreparing
            }
            EffectGroupLifecycle::Retired { .. } => EffectGroupRecordDispatchResponse::Retired,
        };
        if matches!(
            response,
            EffectGroupRecordDispatchResponse::Recorded
                | EffectGroupRecordDispatchResponse::Duplicate
        ) {
            resolve_group_wait(
                &ctx,
                &shape.wait_scope,
                &group_key,
                EffectGroupWaitKind::Admit(request.position),
                EffectGroupWaitResolution::Admit,
            )
            .await?;
        }
        Ok(Json(response))
    }

    #[handler]
    async fn register_children(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupRegisterRequest>,
    ) -> HandlerResult<Json<EffectGroupRegisterResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupRegisterResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupRegisterResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let expected_positions = (0..shape.children()).collect::<Vec<_>>();
        if request.addresses.keys().copied().collect::<Vec<_>>() != expected_positions {
            return Ok(Json(EffectGroupRegisterResponse::RegistrationMismatch));
        }
        let response = match &record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
                live,
            } if dispatched == &request.addresses => {
                record.lifecycle = EffectGroupLifecycle::Ready {
                    addresses: request.addresses,
                    live: live.clone(),
                };
                store_index(&ctx, record.clone());
                resolve_group_wait(
                    &ctx,
                    &shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Ready,
                    EffectGroupWaitResolution::Ready,
                )
                .await?;
                EffectGroupRegisterResponse::Registered
            }
            EffectGroupLifecycle::Preparing { .. } => {
                EffectGroupRegisterResponse::RegistrationMismatch
            }
            EffectGroupLifecycle::Ready { addresses, .. } if addresses == &request.addresses => {
                EffectGroupRegisterResponse::AlreadyRegistered
            }
            EffectGroupLifecycle::Ready { .. } => EffectGroupRegisterResponse::RegistrationMismatch,
            EffectGroupLifecycle::Closed { addresses, .. } if addresses == &request.addresses => {
                EffectGroupRegisterResponse::AlreadyClosed
            }
            EffectGroupLifecycle::Closed { .. } => {
                EffectGroupRegisterResponse::RegistrationMismatch
            }
            EffectGroupLifecycle::Retired { .. } => EffectGroupRegisterResponse::Retired,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn register_refusal(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupRefusalRequest>,
    ) -> HandlerResult<Json<EffectGroupRegisterRefusalResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupRegisterRefusalResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupRegisterRefusalResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let response = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { live, .. } => {
                record.lifecycle = EffectGroupLifecycle::Closed {
                    effective: EffectGroupCloseDisposition::Refused {
                        reason: request.reason.clone(),
                    },
                    reopened: false,
                    addresses: BTreeMap::new(),
                    live: live.clone(),
                };
                store_index(&ctx, record.clone());
                resolve_group_wait(
                    &ctx,
                    &shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Ready,
                    EffectGroupWaitResolution::Refused {
                        reason: request.reason.clone(),
                    },
                )
                .await?;
                for position in 0..shape.children() {
                    resolve_group_wait(
                        &ctx,
                        &shape.wait_scope,
                        &group_key,
                        EffectGroupWaitKind::Admit(position),
                        EffectGroupWaitResolution::Refused {
                            reason: request.reason.clone(),
                        },
                    )
                    .await?;
                }
                EffectGroupRegisterRefusalResponse::Refused
            }
            EffectGroupLifecycle::Ready { .. } => {
                EffectGroupRegisterRefusalResponse::AlreadyRegistered
            }
            EffectGroupLifecycle::Closed { .. } => {
                EffectGroupRegisterRefusalResponse::AlreadyClosed
            }
            EffectGroupLifecycle::Retired { .. } => EffectGroupRegisterRefusalResponse::Retired,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn admit_child(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupAdmissionRequest>,
    ) -> HandlerResult<Json<EffectGroupAdmissionResponse>> {
        #[cfg(test)]
        let group_key = ctx.key().to_string();
        let Some(record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupAdmissionResponse::Refused));
        };
        let response = decide_group_child_admission(
            &record.lifecycle,
            request.position,
            &request.invocation_id,
        );
        #[cfg(test)]
        if response == EffectGroupAdmissionResponse::NotYetRecorded {
            notify_admission_witness(&group_key);
        }
        Ok(Json(response))
    }

    /// The §4 point for one child's final record: `pending` to `committed`,
    /// allocating the durable `commit_seq` the §5 barrier orders drains by.
    ///
    /// The durable boundary the SQL tiers' `commit_group_child` mirrors: the
    /// decision and the position commit here, before the child's payload and
    /// settlement writes, so a completion reaching the index after a cancel
    /// decision is refused by name and a redrive reads its own commit back
    /// instead of re-deciding. `blocking_positions` is the barrier as the
    /// index sees it at the point — every committed sibling below this
    /// child's position still owed a seat — so the caller waits on durable
    /// wakes rather than polling.
    #[handler]
    async fn commit_child(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupCommitChildRequest>,
    ) -> HandlerResult<Json<EffectGroupCommitChildResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupCommitChildResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupCommitChildResponse::Retired));
        }
        let live = record.live_mut()?;
        let Some(position) = live
            .shape
            .replay_keys
            .iter()
            .position(|replay_key| replay_key == &request.replay_key)
        else {
            return Ok(Json(EffectGroupCommitChildResponse::UnknownChild));
        };
        match live.commit_states.get(&position).copied() {
            Some(EffectGroupChildCommitState::CancelDecided) => {
                let rank = live
                    .settled_positions
                    .get(&position)
                    .copied()
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "effect group {group_key} child {position} is cancel-decided but \
                             holds no rank; the decision and its seat commit in one handler"
                        ))
                    })?;
                return Ok(Json(EffectGroupCommitChildResponse::CancelDecided { rank }));
            }
            Some(EffectGroupChildCommitState::Committed { commit_seq }) => {
                return Ok(Json(EffectGroupCommitChildResponse::AlreadyCommitted {
                    commit_seq,
                    blocking_positions: blocking_positions(live, commit_seq),
                }));
            }
            None => {}
        }
        let commit_seq = live.next_commit_seq;
        live.next_commit_seq = live.next_commit_seq.checked_add(1).ok_or_else(|| {
            TerminalError::new(format!(
                "effect group {group_key} exhausted commit positions"
            ))
        })?;
        live.commit_states.insert(
            position,
            EffectGroupChildCommitState::Committed { commit_seq },
        );
        let blocking_positions = blocking_positions(live, commit_seq);
        store_index(&ctx, record);
        Ok(Json(EffectGroupCommitChildResponse::Committed {
            commit_seq,
            blocking_positions,
        }))
    }

    /// §4's admission fence on this tier (FIG-3470): may a semantic effect
    /// minted under the named child still be admitted? The index answers —
    /// under the same object serialization `close` writes the decision
    /// through — so an admission can never observe a pre-decision state while
    /// its run survives the decision's commit. `CancelDecided` refuses;
    /// `Committed` and undecided admit, because a committed child retains
    /// authority to finish its drain and a live one has no decision to lose
    /// to. An absent or retired group is `UnknownGroup`: a reaped index has
    /// no live state to arbitrate under.
    #[handler]
    async fn admit_semantic(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupAdmitSemanticRequest>,
    ) -> HandlerResult<Json<EffectGroupAdmitSemanticResponse>> {
        let Some(record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupAdmitSemanticResponse::UnknownGroup));
        };
        let Ok(live) = record.live() else {
            return Ok(Json(EffectGroupAdmitSemanticResponse::UnknownGroup));
        };
        let Some(position) = live
            .shape
            .replay_keys
            .iter()
            .position(|replay_key| replay_key == &request.replay_key)
        else {
            return Ok(Json(EffectGroupAdmitSemanticResponse::UnknownChild));
        };
        Ok(Json(match live.commit_states.get(&position).copied() {
            Some(EffectGroupChildCommitState::CancelDecided) => {
                EffectGroupAdmitSemanticResponse::CancelDecided
            }
            _ => EffectGroupAdmitSemanticResponse::Admitted,
        }))
    }

    /// The siblings the §5 barrier still holds `commit_seq` behind: every
    /// one that committed below it and has not seated its settlement. The
    /// caller parks on each one's drained wake — the engine's own durable
    /// wake, never a poll. An absent or retired group holds no committed
    /// children, so nothing blocks.
    #[handler]
    async fn drain_blockers(
        &self,
        ctx: SharedObjectContext<'_>,
        Json(request): Json<EffectGroupDrainBlockersRequest>,
    ) -> HandlerResult<Json<EffectGroupDrainBlockersResponse>> {
        let response = match load_index_shared(&ctx).await? {
            Some(record) => match record.live() {
                Ok(live) => {
                    let positions = blocking_positions(live, request.commit_seq);
                    if positions.is_empty() {
                        EffectGroupDrainBlockersResponse::Admitted
                    } else {
                        EffectGroupDrainBlockersResponse::Blocked {
                            wait_scope: live.shape.wait_scope.clone(),
                            positions,
                        }
                    }
                }
                Err(_) => EffectGroupDrainBlockersResponse::Admitted,
            },
            None => EffectGroupDrainBlockersResponse::Admitted,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn record_settlement(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupRecordSettlementRequest>,
    ) -> HandlerResult<Json<EffectGroupRecordSettlementResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupRecordSettlementResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupRecordSettlementResponse::Retired));
        }
        let live = record.live_mut()?;
        if request.position >= live.shape.children() {
            return Ok(Json(EffectGroupRecordSettlementResponse::UnknownChild));
        }
        // The §4 point is the decision, not the seat: a child whose cancel
        // disposition committed first is refused by name, one whose own
        // commit landed seats its rank here, and a settled one reports its
        // rank back idempotently. A settlement arriving with no commit at
        // all is a protocol defect — `commit_child` is the only writer of
        // `Committed` and it runs before any payload exists to settle.
        match live.commit_states.get(&request.position).copied() {
            Some(EffectGroupChildCommitState::CancelDecided) => {
                let rank = live
                    .settled_positions
                    .get(&request.position)
                    .copied()
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "effect group {group_key} child {} is cancel-decided but holds \
                             no rank; the decision and its seat commit in one handler",
                            request.position
                        ))
                    })?;
                return Ok(Json(EffectGroupRecordSettlementResponse::CancelDecided {
                    rank,
                }));
            }
            Some(EffectGroupChildCommitState::Committed { .. })
                if live.settled_positions.contains_key(&request.position) =>
            {
                let rank = live
                    .settled_positions
                    .get(&request.position)
                    .copied()
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "effect group {group_key} child {} is committed and seated but \
                             holds no rank; the seat and its rank commit in one handler",
                            request.position
                        ))
                    })?;
                resolve_group_wait(
                    &ctx,
                    &live.shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Rank(rank),
                    EffectGroupWaitResolution::Rank,
                )
                .await?;
                return Ok(Json(EffectGroupRecordSettlementResponse::Duplicate {
                    rank,
                }));
            }
            Some(EffectGroupChildCommitState::Committed { .. }) => {}
            None => {
                return Err(TerminalError::new(format!(
                    "effect group {group_key} child {} reached record_settlement with no \
                     committed §4 decision; commit_child is the only writer of Committed \
                     and must run before the child's payload exists to settle",
                    request.position
                ))
                .into());
            }
        }
        let rank = live.next_rank;
        live.next_rank = live.next_rank.checked_add(1).ok_or_else(|| {
            TerminalError::new(format!(
                "effect group {group_key} exhausted settlement ranks"
            ))
        })?;
        let settlement = EffectGroupSettlementRecord {
            position: request.position,
            sequence: rank,
            terminal: request.terminal,
        };
        live.settlements.insert(rank, settlement);
        live.settled_positions.insert(request.position, rank);
        let wait_scope = live.shape.wait_scope.clone();
        let replay_key = live.shape.replay_key(request.position)?.to_string();
        store_index(&ctx, record.clone());
        resolve_group_wait(
            &ctx,
            &wait_scope,
            &group_key,
            EffectGroupWaitKind::Rank(rank),
            EffectGroupWaitResolution::Rank,
        )
        .await?;
        // The seat is what a §5 barrier parks on: siblings that committed
        // above this child resume their drains off this wake.
        resolve_group_wait(
            &ctx,
            &wait_scope,
            &group_key,
            EffectGroupWaitKind::Drained(request.position),
            EffectGroupWaitResolution::Drained,
        )
        .await?;
        // A seated child is past every cancel: its cancel wait ends here, so
        // the dispatch invocation's watch on it does not stay open on the
        // deployment until the group closes or retires.
        resolve_group_wait(
            &ctx,
            &wait_scope,
            &group_key,
            EffectGroupWaitKind::Cancel(&replay_key),
            EffectGroupWaitResolution::Settled,
        )
        .await?;
        Ok(Json(EffectGroupRecordSettlementResponse::Recorded { rank }))
    }

    #[handler]
    async fn read_rank(
        &self,
        ctx: SharedObjectContext<'_>,
        Json(request): Json<EffectGroupReadRankRequest>,
    ) -> HandlerResult<Json<EffectGroupReadRankResponse>> {
        let Some(record) = load_index_shared(&ctx).await? else {
            return Ok(Json(EffectGroupReadRankResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupReadRankResponse::Retired));
        }
        let closed_to_caller = matches!(
            &record.lifecycle,
            EffectGroupLifecycle::Closed {
                effective,
                reopened,
                ..
            } if !reopened || matches!(effective, EffectGroupCloseDisposition::Refused { .. })
        );
        // A caller is refused its closed group whether or not the rank settled.
        if request.for_caller && closed_to_caller {
            return Ok(Json(EffectGroupReadRankResponse::Closed));
        }
        let settlement = record.live()?.settlements.get(&request.rank).cloned();
        if let Some(settlement) = settlement {
            let child_replay_key = record
                .live()?
                .shape
                .replay_key(settlement.position)?
                .to_string();
            return Ok(Json(EffectGroupReadRankResponse::Settled {
                settlement,
                child_replay_key,
            }));
        }
        // An unsettled rank of a group closed to its caller answers `Closed`
        // to every reader. A reopened caller parks like a live group: an RTC
        // loser still lands, and a committed child under Cancel seats its
        // rank when the drain finishes (FIG-3481).
        Ok(Json(if closed_to_caller {
            EffectGroupReadRankResponse::Closed
        } else {
            EffectGroupReadRankResponse::NotSettled
        }))
    }

    #[handler]
    async fn close(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupCloseRequest>,
    ) -> HandlerResult<Json<EffectGroupCloseResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupCloseResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupCloseResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let (declared, addresses, prior) = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => {
                return Ok(Json(EffectGroupCloseResponse::NotReady));
            }
            EffectGroupLifecycle::Ready { addresses, .. } => {
                (shape.loser_disposition, addresses.clone(), None)
            }
            EffectGroupLifecycle::Closed {
                effective,
                addresses,
                ..
            } => {
                let declared = match effective {
                    EffectGroupCloseDisposition::RunToCompletion => LoserPolicy::RunToCompletion,
                    EffectGroupCloseDisposition::Cancel => LoserPolicy::Cancel,
                    EffectGroupCloseDisposition::Refused { .. } => {
                        return Ok(Json(EffectGroupCloseResponse::AlreadyClosed));
                    }
                };
                (declared, addresses.clone(), Some(effective.clone()))
            }
            EffectGroupLifecycle::Retired { .. } => {
                return Ok(Json(EffectGroupCloseResponse::Retired));
            }
        };
        let effective = match LoserPolicy::resolve_close(declared, request.disposition) {
            Ok(effective) => effective,
            Err(_) => return Ok(Json(EffectGroupCloseResponse::WidenRefused)),
        };
        if prior.as_ref() == Some(&EffectGroupCloseDisposition::from(effective)) {
            return Ok(Json(EffectGroupCloseResponse::AlreadyClosed));
        }
        let mut decided = Vec::new();
        if effective == LoserPolicy::Cancel {
            let live = record.live_mut()?;
            for position in 0..live.shape.children() {
                // The §4 decision, not the seat: a committed child is
                // protected by the decision it holds, and an already-cancelled
                // one keeps its first seat.
                if live.commit_states.contains_key(&position) {
                    continue;
                }
                live.commit_states
                    .insert(position, EffectGroupChildCommitState::CancelDecided);
                decided.push(position);
                let rank = live.next_rank;
                live.next_rank = live.next_rank.checked_add(1).ok_or_else(|| {
                    TerminalError::new(format!(
                        "effect group {group_key} exhausted settlement ranks"
                    ))
                })?;
                live.settlements.insert(
                    rank,
                    EffectGroupSettlementRecord {
                        position,
                        sequence: rank,
                        terminal: EffectGroupSettlementTerminal::Cancelled,
                    },
                );
                live.settled_positions.insert(position, rank);
            }
        }
        seal_cancel_decisions(&ctx, &group_key, &shape, &decided).await?;
        let live = record.live()?.clone();
        record.lifecycle = EffectGroupLifecycle::Closed {
            effective: effective.into(),
            reopened: false,
            addresses: addresses.clone(),
            live,
        };
        store_index(&ctx, record.clone());
        if effective == LoserPolicy::Cancel {
            let live = record.live()?;
            for position in 0..shape.children() {
                // A committed-but-undrained child is a pending protected drain
                // (ADR 0099 §4): the cancel decision refused it, so the close
                // seats no rank for it, does not resolve its cancel wait, and
                // does not interrupt its invocation — its `record_settlement`
                // seats the rank and resolves the rank wait when the drain
                // finishes.
                if matches!(
                    live.commit_states.get(&position),
                    Some(EffectGroupChildCommitState::Committed { .. })
                ) && !live.settled_positions.contains_key(&position)
                {
                    continue;
                }
                let rank = live
                    .settled_positions
                    .get(&position)
                    .copied()
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "effect group {group_key} has no settlement rank for child {position}"
                        ))
                    })?;
                resolve_group_wait(
                    &ctx,
                    &shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Rank(rank),
                    EffectGroupWaitResolution::Rank,
                )
                .await?;
                resolve_group_wait(
                    &ctx,
                    &shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Cancel(shape.replay_key(position)?),
                    EffectGroupWaitResolution::Cancel,
                )
                .await?;
                resolve_group_wait(
                    &ctx,
                    &shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Admit(position),
                    EffectGroupWaitResolution::Cancel,
                )
                .await?;
                if let Some(invocation_id) = addresses.get(&position) {
                    ctx.invocation_handle(invocation_id.clone()).cancel();
                }
            }
        }
        Ok(Json(EffectGroupCloseResponse::Closed))
    }

    #[handler]
    async fn retire(
        &self,
        ctx: ObjectContext<'_>,
    ) -> HandlerResult<Json<EffectGroupRetireResponse>> {
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupRetireResponse::UnknownGroup));
        };
        if let EffectGroupLifecycle::Retired { cleanup } = &record.lifecycle {
            return Ok(Json(match cleanup {
                EffectGroupCleanup::Pending { facts, .. } => {
                    EffectGroupRetireResponse::AlreadyRetired {
                        cleanup: facts.clone(),
                    }
                }
                EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
            }));
        }
        let (shape, live) = {
            let live = record.live()?;
            (live.shape.clone(), live.clone())
        };
        let (dispatcher, dispatched) = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { dispatch, .. } => {
                let dispatched = match dispatch {
                    EffectGroupDispatchState::Unadopted => BTreeMap::new(),
                    EffectGroupDispatchState::Adopted { dispatched, .. } => dispatched.clone(),
                };
                (dispatch.clone(), dispatched)
            }
            EffectGroupLifecycle::Ready { addresses, .. }
            | EffectGroupLifecycle::Closed { addresses, .. } => {
                // A workflow run is exactly-once per workflow key. Re-sending
                // the completed run recovers its stable invocation id without
                // re-executing it; if a test assembled Ready directly, the
                // probe guard makes the newly-created run exit before sending.
                let group_key = ctx.key().to_string();
                let handle = ctx
                    .workflow_client::<EffectGroupDispatchClient>(group_key.clone())
                    .run(Json(EffectGroupDispatchRequest { group_key }))
                    .send()
                    .await?;
                (
                    EffectGroupDispatchState::Adopted {
                        id: handle.invocation_id().to_owned(),
                        dispatched: addresses.clone(),
                    },
                    addresses.clone(),
                )
            }
            EffectGroupLifecycle::Retired { cleanup } => {
                return Ok(Json(match cleanup {
                    EffectGroupCleanup::Pending { facts, .. } => {
                        EffectGroupRetireResponse::AlreadyRetired {
                            cleanup: facts.clone(),
                        }
                    }
                    EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
                }));
            }
        };
        let cleanup = EffectGroupCleanupFacts {
            replay_keys: shape.replay_keys,
            dispatcher,
            dispatched,
            wait_scope: shape.wait_scope,
        };
        record.lifecycle = EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Pending {
                facts: cleanup.clone(),
                live: Box::new(live),
            },
        };
        store_index(&ctx, record);
        Ok(Json(EffectGroupRetireResponse::Retired { cleanup }))
    }

    #[handler]
    async fn finish_retirement(
        &self,
        ctx: ObjectContext<'_>,
    ) -> HandlerResult<Json<EffectGroupFinishRetirementResponse>> {
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupFinishRetirementResponse::UnknownGroup));
        };
        let response = match record.lifecycle {
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Pending { .. },
            } => {
                record.lifecycle = EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Complete,
                };
                store_index(&ctx, record);
                EffectGroupFinishRetirementResponse::Finished
            }
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Complete,
            } => EffectGroupFinishRetirementResponse::AlreadyFinished,
            _ => EffectGroupFinishRetirementResponse::NotRetired,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn retirement_cancel(
        &self,
        ctx: ObjectContext<'_>,
    ) -> HandlerResult<Json<EffectGroupRetirementCancelResponse>> {
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupRetirementCancelResponse::UnknownGroup));
        };
        let mut decided = Vec::new();
        let (facts, ranks, changed, shape) = {
            let (facts, live) = match &mut record.lifecycle {
                EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Pending { facts, live },
                } => (facts, live),
                EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Complete,
                } => return Ok(Json(EffectGroupRetirementCancelResponse::Tombstone)),
                _ => return Ok(Json(EffectGroupRetirementCancelResponse::NotRetired)),
            };
            let mut changed = false;
            for position in 0..facts.children() {
                // Same §4 arbitration as a `Cancel` close: a committed child keeps
                // its own settlement, and only the undecided are seated cancelled.
                if live.commit_states.contains_key(&position) {
                    continue;
                }
                live.commit_states
                    .insert(position, EffectGroupChildCommitState::CancelDecided);
                decided.push(position);
                let rank = live.next_rank;
                live.next_rank = live.next_rank.checked_add(1).ok_or_else(|| {
                    TerminalError::new(format!(
                        "effect group {group_key} exhausted settlement ranks during retirement"
                    ))
                })?;
                live.settlements.insert(
                    rank,
                    EffectGroupSettlementRecord {
                        position,
                        sequence: rank,
                        terminal: EffectGroupSettlementTerminal::Cancelled,
                    },
                );
                live.settled_positions.insert(position, rank);
                changed = true;
            }
            // A committed-but-undrained child is a pending protected drain
            // (ADR 0099 §4): it holds no rank yet, and the retirement wait
            // pass resolves every remaining wait as Retired — only children
            // with a seated rank get their Rank/Cancel/Admit resolutions here.
            let ranks = (0..facts.children())
                .filter_map(|position| {
                    live.settled_positions
                        .get(&position)
                        .copied()
                        .map(|rank| (position, rank))
                })
                .collect::<Vec<_>>();
            (facts.clone(), ranks, changed, live.shape.clone())
        };
        seal_cancel_decisions(&ctx, &group_key, &shape, &decided).await?;
        store_index(&ctx, record.clone());
        for (position, rank) in ranks.iter().copied() {
            resolve_group_wait(
                &ctx,
                &facts.wait_scope,
                &group_key,
                EffectGroupWaitKind::Rank(rank),
                EffectGroupWaitResolution::Rank,
            )
            .await?;
            resolve_group_wait(
                &ctx,
                &facts.wait_scope,
                &group_key,
                EffectGroupWaitKind::Cancel(facts.replay_key(position)?),
                EffectGroupWaitResolution::Cancel,
            )
            .await?;
            resolve_group_wait(
                &ctx,
                &facts.wait_scope,
                &group_key,
                EffectGroupWaitKind::Admit(position),
                EffectGroupWaitResolution::Cancel,
            )
            .await?;
        }
        Ok(Json(if changed {
            EffectGroupRetirementCancelResponse::Applied
        } else {
            EffectGroupRetirementCancelResponse::AlreadyApplied
        }))
    }
}

mod dispatch;
mod payload;
#[cfg(test)]
pub(crate) use dispatch::EffectGroupChildRequest;
pub(crate) use dispatch::EffectGroupDispatch;
pub(crate) use dispatch::EffectGroupDispatchClient;
pub use dispatch::EffectGroupDispatchRequest;
pub use payload::{
    EFFECT_GROUP_PAYLOAD_FORMAT_VERSION, EffectGroupPayloadGetResponse,
    EffectGroupPayloadPutRequest, EffectGroupPayloadPutResponse,
};
pub(crate) use payload::{EffectGroupPayload, EffectGroupPayloadClient};
pub(crate) fn payload_key(group_key: &str, position: usize) -> String {
    let digest = Sha256::digest(group_key.as_bytes());
    format!("{:x}:{position}", digest)
}

pub(crate) fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

pub(crate) fn decode_wait_resolution(
    resolution: Resolution,
) -> Result<EffectGroupWaitResolution, RuntimeEffectControllerError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value).map_err(|error| {
            group_shape_error(format!("decode Restate effect-group wake: {error}"))
        }),
        Resolution::Err(lash_core::runtime::ExternalCompletionError { code, message, .. }) => {
            Err(group_shape_error(format!(
                "effect-group durable wait failed with {code}: {message}"
            )))
        }
        Resolution::Timeout => Err(group_shape_error("effect-group durable wait timed out")),
        Resolution::Cancelled => Err(group_shape_error("effect-group durable wait was cancelled")),
    }
}

pub(crate) fn settlement_from_payload(
    record: EffectGroupSettlementRecord,
    payload: Option<Vec<u8>>,
) -> Result<GroupSettlement, RuntimeEffectControllerError> {
    let outcome = match record.terminal {
        EffectGroupSettlementTerminal::StoredPayload => {
            let bytes = payload.ok_or_else(|| {
                group_shape_error(format!(
                    "effect group settlement rank {} refers to a missing payload",
                    record.sequence
                ))
            })?;
            serde_json::from_slice::<RuntimeEffectOutcome>(&bytes).map_err(|error| {
                group_shape_error(format!(
                    "decode effect group settlement rank {} payload: {error}",
                    record.sequence
                ))
            })
        }
        EffectGroupSettlementTerminal::Failed { error } => Err(error),
        EffectGroupSettlementTerminal::Cancelled => Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
            format!("effect group child {} was cancelled", record.position),
        )),
    };
    Ok(GroupSettlement {
        position: record.position,
        sequence: record.sequence,
        outcome,
    })
}
