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
use crate::compat::{Call, Reply};
use crate::durable_wait::{
    LASH_REPLAY_KEY_HEADER, RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    RestateDurableWaitGroupChildRequest, RestateDurableWaitResolveRequest,
    durable_wait_index_key_for_scope, durable_wait_index_object_key, restate_await_event_key,
};
use crate::object_state::{
    self, FleetView, ObjectFamily, ObjectUpgradeResponse, StoredValueFormats,
};

const INDEX_STATE_KEY: &str = "effect-group/v1/state";
/// The group's accepted membership, apart from the index record every
/// per-child handler reads (FIG-4068): written once at open, read only where
/// children are rebuilt, and cleared when retirement completes.
const MEMBERSHIP_STATE_KEY: &str = "effect-group/v1/membership";

mod drain_barrier;
mod group_waits;
mod protocol;
mod rank_run;
mod reopen;
mod wire;
use drain_barrier::blocking_positions;
pub(crate) use drain_barrier::{drained_wait_lifted, drained_wait_request};
use group_waits::{
    resolve_group_wait, resolve_group_waits, seal_cancel_decisions, wait_resolution,
};
pub(crate) use protocol::EFFECT_GROUP_STATE_FAMILY;
#[cfg(test)]
pub(crate) use protocol::EFFECT_GROUP_STATE_FORMATS;
pub use protocol::{EFFECT_GROUP_DISPATCH_JOURNAL_VERSION, EFFECT_GROUP_STATE_FORMAT_VERSION};
use protocol::{load_index, load_index_shared, load_membership};
use rank_run::served_run;
pub(crate) use reopen::{content_checked_shape_mismatch, content_mismatch};
pub(crate) use wire::btree_map_as_pairs;
pub use wire::{
    EffectGroupAdmitSemanticRequest, EffectGroupAdmitSemanticResponse, EffectGroupPhase,
    EffectGroupProbeResponse,
};

mod shape;
pub use shape::{EffectGroupMembership, EffectGroupShape};

/// Who dispatches a preparing group. The adopted dispatcher's id is kept so a
/// retirement before registration can cancel it and the child calls it
/// tracks; the children's own ids are recorded only by the registration that
/// makes the group ready (FIG-4308).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupDispatchState {
    Unadopted,
    Adopted { id: String },
}

mod state_record;
pub use state_record::*;

mod messages;
pub use messages::*;

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
mod admission_witness {
    use super::*;

    static ADMISSION_WITNESSES: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>,
    > = std::sync::OnceLock::new();

    pub(crate) fn arm(group_key: &str) -> Arc<tokio::sync::Notify> {
        let hook = Arc::new(tokio::sync::Notify::new());
        ADMISSION_WITNESSES
            .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(group_key.to_owned(), Arc::clone(&hook));
        hook
    }

    pub(super) fn notify(group_key: &str) {
        if let Some(hook) = ADMISSION_WITNESSES
            .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(group_key)
        {
            hook.notify_one();
        }
    }
}

#[cfg(test)]
pub(crate) use admission_witness::arm as arm_admission_witness;

fn phase(lifecycle: &EffectGroupLifecycle) -> EffectGroupPhase {
    match lifecycle {
        EffectGroupLifecycle::Preparing { .. } => EffectGroupPhase::Preparing,
        EffectGroupLifecycle::Ready { .. } => EffectGroupPhase::Ready,
        EffectGroupLifecycle::Closed { .. } => EffectGroupPhase::Closed,
        EffectGroupLifecycle::Retired { .. } => EffectGroupPhase::Retired,
    }
}

fn store_index(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    record: EffectGroupStateRecord,
) {
    object_state::set_stamped(ctx, INDEX_STATE_KEY, writer, record);
}

fn store_membership(
    ctx: &ObjectContext<'_>,
    writer: object_state::StoredValueWriter,
    membership: EffectGroupMembership,
) {
    object_state::set_stamped(ctx, MEMBERSHIP_STATE_KEY, writer, membership);
}

/// An effect group's lifecycle and settlement rank, one object per group.
/// Every handler takes a [`Call`] and answers a [`Reply`], and reads the
/// object's `_compat` record before any other state (ADR 0115 §3).
// The registered service keeps its `EffectGroupIndex` name (FIG-3814).
#[restate_sdk::object]
#[name = "EffectGroupIndex"]
pub(crate) trait EffectGroupState {
    #[shared]
    async fn probe(call: Call<()>) -> HandlerResult<Reply<EffectGroupProbeResponse>>;
    #[shared]
    async fn unsettled_children(call: Call<()>) -> HandlerResult<Reply<usize>>;
    async fn open(
        call: Call<EffectGroupOpenRequest>,
    ) -> HandlerResult<Reply<EffectGroupOpenResponse>>;
    async fn probe_and_adopt(
        call: Call<EffectGroupAdoptRequest>,
    ) -> HandlerResult<Reply<EffectGroupProbeAdoptResponse>>;
    async fn register_dispatch(
        call: Call<EffectGroupRegisterDispatchRequest>,
    ) -> HandlerResult<Reply<EffectGroupRegisterDispatchResponse>>;
    async fn register_refusal(
        call: Call<EffectGroupRefusalRequest>,
    ) -> HandlerResult<Reply<EffectGroupRegisterRefusalResponse>>;
    async fn admit_child(
        call: Call<EffectGroupAdmissionRequest>,
    ) -> HandlerResult<Reply<EffectGroupAdmissionResponse>>;
    async fn commit_child(
        call: Call<EffectGroupCommitChildRequest>,
    ) -> HandlerResult<Reply<EffectGroupCommitChildResponse>>;
    async fn admit_semantic(
        call: Call<EffectGroupAdmitSemanticRequest>,
    ) -> HandlerResult<Reply<EffectGroupAdmitSemanticResponse>>;
    #[shared]
    async fn drain_blockers(
        call: Call<EffectGroupDrainBlockersRequest>,
    ) -> HandlerResult<Reply<EffectGroupDrainBlockersResponse>>;
    async fn record_settlement(
        call: Call<EffectGroupRecordSettlementRequest>,
    ) -> HandlerResult<Reply<EffectGroupRecordSettlementResponse>>;
    #[shared]
    async fn read_rank(
        call: Call<EffectGroupReadRankRequest>,
    ) -> HandlerResult<Reply<EffectGroupReadRankResponse>>;
    async fn close(
        call: Call<EffectGroupCloseRequest>,
    ) -> HandlerResult<Reply<EffectGroupCloseResponse>>;
    async fn retire(call: Call<()>) -> HandlerResult<Reply<EffectGroupRetireResponse>>;
    async fn finish_retirement(
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupFinishRetirementResponse>>;
    async fn retirement_cancel(
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupRetirementCancelResponse>>;
    /// Rewrite the group at the newest family format once finalize
    /// has moved the fleet to it, and raise its `_compat` (ADR 0115
    /// §3.2, FIG-4041): the object sweep's step.
    async fn upgrade(call: Call<()>) -> HandlerResult<Reply<ObjectUpgradeResponse>>;
}

/// [`EffectGroupState`] in one deployment's namespace: the durable-wait
/// index a group's waits and scope records live in is its namespace's
/// (FIG-3898).
#[derive(Clone, Debug, Default)]
pub(crate) struct EffectGroupStateImpl {
    namespace: crate::RestateNamespace,
    /// Where the handlers read the fleet epoch their writes are stamped at.
    fleet: FleetView,
}

impl EffectGroupStateImpl {
    pub(crate) fn new(namespace: crate::RestateNamespace, fleet: FleetView) -> Self {
        Self { namespace, fleet }
    }

    /// The group's `_compat` gate for a handler that may write.
    async fn admit(
        &self,
        ctx: &ObjectContext<'_>,
    ) -> Result<object_state::AdmittedObject, TerminalError> {
        object_state::admit_exclusive(ctx, &EFFECT_GROUP_STATE_FAMILY, self.fleet.fleet_format())
            .await
    }
}

impl EffectGroupState for EffectGroupStateImpl {
    async fn upgrade(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<ObjectUpgradeResponse>> {
        let (wire, ()) = call.open()?;
        let response = object_state::upgrade_object(
            &ctx,
            &EFFECT_GROUP_STATE_FAMILY,
            self.fleet.fleet_format(),
        )
        .await?;
        Ok(Reply::at(wire, response))
    }

    async fn probe(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupProbeResponse>> {
        let (wire, ()) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let response = match load_index_shared(&ctx).await? {
            None => EffectGroupProbeResponse::Absent,
            Some(record) => EffectGroupProbeResponse::Exists {
                shape_digest: record.shape_digest,
                phase: phase(&record.lifecycle),
            },
        };
        Ok(Reply::at(wire, response))
    }

    /// How many of this group's children have no settlement yet: the count
    /// the owning scope's quiescence proof reads (FIG-2499). An absent or
    /// retired group, or one whose live record is gone, has none.
    async fn unsettled_children(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<usize>> {
        let (wire, ()) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let unsettled = match load_index_shared(&ctx).await? {
            Some(record) => record.live().map_or(0, |live| {
                live.shape
                    .children()
                    .saturating_sub(live.settled_positions.len())
            }),
            None => 0,
        };
        Ok(Reply::at(wire, unsettled))
    }

    async fn open(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupOpenRequest>,
    ) -> HandlerResult<Reply<EffectGroupOpenResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        request.shape.validate_membership(&request.membership)?;
        // The route is recorded verbatim, so it must name a dispatcher lane
        // a deployment binds (FIG-3795): an opener cannot declare a route
        // no dispatch could ever run under.
        if !self
            .namespace
            .parse(&request.dispatch_route)
            .is_some_and(|route| route.service() == crate::LashService::EffectGroupDispatch)
        {
            return Err(TerminalError::new(format!(
                "effect group {} open declared dispatch route `{}`, which names no \
                 EffectGroupDispatch lane",
                ctx.key(),
                request.dispatch_route
            ))
            .into());
        }
        let Some(mut record) = load_index(&ctx).await? else {
            let shape_digest = request.shape.digest(&request.membership)?;
            let dispatch_route = request.dispatch_route.clone();
            store_membership(&ctx, object.writer, request.membership);
            store_index(
                &ctx,
                object.writer,
                EffectGroupStateRecord {
                    shape_digest,
                    dispatch_route: dispatch_route.clone(),
                    lifecycle: EffectGroupLifecycle::Preparing {
                        dispatch: EffectGroupDispatchState::Unadopted,
                        live: EffectGroupStateLiveRecord {
                            shape: request.shape,
                            next_rank: 1,
                            commit_states: BTreeMap::new(),
                            settlements: BTreeMap::new(),
                            settled_positions: BTreeMap::new(),
                        },
                    },
                },
            );
            return Ok(Reply::at(
                wire,
                EffectGroupOpenResponse::OpenedFresh { dispatch_route },
            ));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupOpenResponse::Retired));
        }
        // The fence is the shared reopen contract — arity, wake rule, declared
        // disposition — never the retained membership: a reopen *offers*
        // children that may disagree with what the journal kept, and the
        // recorded membership wins (ADR 0099 §3).
        if !record.live()?.shape.fences_equivalent(&request.shape) {
            return Ok(Reply::at(wire, EffectGroupOpenResponse::ShapeMismatch));
        }
        // A content-checked reopen (FIG-3586) is refused when any offered
        // child is not the retained one: the aggregate is addressed by its
        // issue ordinal, so a redrive with different leaves reaches this key.
        if request.content_checked
            && let Some(position) = load_membership(&ctx)
                .await?
                .0
                .iter()
                .zip(&request.membership.0)
                .position(|(retained, offered)| retained != offered)
        {
            return Ok(Reply::at(
                wire,
                EffectGroupOpenResponse::ContentMismatch { position },
            ));
        }
        // A reopen is a new caller interest (FIG-3481): a non-refused Closed
        // entry keeps its cumulative disposition but its reopened marker
        // re-enables rank reads — the flag clear the SQL entries take.
        let mut marked = false;
        let dispatch_route = record.dispatch_route.clone();
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => {
                EffectGroupOpenResponse::ReopenedPreparing { dispatch_route }
            }
            EffectGroupLifecycle::Ready { .. } => EffectGroupOpenResponse::ReopenedReady,
            EffectGroupLifecycle::Closed {
                effective,
                reopened,
                ..
            } => {
                if !matches!(effective, EffectGroupCloseOutcome::Refused { .. }) && !*reopened {
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
            store_index(&ctx, object.writer, record);
        }
        Ok(Reply::at(wire, response))
    }

    async fn probe_and_adopt(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupAdoptRequest>,
    ) -> HandlerResult<Reply<EffectGroupProbeAdoptResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(wire, EffectGroupProbeAdoptResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupProbeAdoptResponse::Retired));
        }
        // Read before the mutable match below: the adopting dispatcher gets
        // the recorded shape and membership so its children are always the
        // retained membership, never the envelopes a reopen offered.
        let shape = record.live()?.shape.clone();
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing { dispatch, .. } => match dispatch {
                EffectGroupDispatchState::Unadopted => {
                    *dispatch = EffectGroupDispatchState::Adopted {
                        id: request.invocation_id,
                    };
                    store_index(&ctx, object.writer, record);
                    EffectGroupProbeAdoptResponse::Adopted {
                        shape,
                        membership: load_membership(&ctx).await?,
                    }
                }
                EffectGroupDispatchState::Adopted { id, .. } if id == &request.invocation_id => {
                    EffectGroupProbeAdoptResponse::AlreadyAdopted {
                        shape,
                        membership: load_membership(&ctx).await?,
                    }
                }
                EffectGroupDispatchState::Adopted { .. } => {
                    EffectGroupProbeAdoptResponse::DifferentDispatcher
                }
            },
            EffectGroupLifecycle::Ready { .. } => EffectGroupProbeAdoptResponse::Ready,
            EffectGroupLifecycle::Closed { .. } => EffectGroupProbeAdoptResponse::Closed,
            EffectGroupLifecycle::Retired { .. } => EffectGroupProbeAdoptResponse::Retired,
        };
        Ok(Reply::at(wire, response))
    }

    /// Records every child's invocation id and makes the group ready, in one
    /// step (FIG-4308): the dispatch has issued every child call and journaled
    /// every id before it registers, so nothing is awaited before this point
    /// (ADR 0099 §2). The live record moves over as it stands — a child that
    /// settled while the group was preparing (a generation refusal, which
    /// precedes admission) keeps its seat. Every ADMIT wake and READY resolve
    /// in one round trip; a redriven registration of the same map re-resolves
    /// them, which is idempotent. A retired group is never made ready again.
    async fn register_dispatch(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupRegisterDispatchRequest>,
    ) -> HandlerResult<Reply<EffectGroupRegisterDispatchResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupRegisterDispatchResponse::UnknownGroup,
            ));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(
                wire,
                EffectGroupRegisterDispatchResponse::Retired,
            ));
        }
        let shape = record.live()?.shape.clone();
        let expected_positions = (0..shape.children()).collect::<Vec<_>>();
        if request.addresses.keys().copied().collect::<Vec<_>>() != expected_positions {
            return Ok(Reply::at(
                wire,
                EffectGroupRegisterDispatchResponse::Mismatch,
            ));
        }
        let response = match &record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { .. },
                live,
            } => {
                record.lifecycle = EffectGroupLifecycle::Ready {
                    addresses: request.addresses,
                    live: live.clone(),
                };
                store_index(&ctx, object.writer, record);
                EffectGroupRegisterDispatchResponse::Registered
            }
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Unadopted,
                ..
            } => EffectGroupRegisterDispatchResponse::Mismatch,
            EffectGroupLifecycle::Ready { addresses, .. } if addresses == &request.addresses => {
                EffectGroupRegisterDispatchResponse::AlreadyRegistered
            }
            EffectGroupLifecycle::Closed { addresses, .. } if addresses == &request.addresses => {
                EffectGroupRegisterDispatchResponse::AlreadyClosed
            }
            EffectGroupLifecycle::Ready { .. } | EffectGroupLifecycle::Closed { .. } => {
                EffectGroupRegisterDispatchResponse::Mismatch
            }
            EffectGroupLifecycle::Retired { .. } => EffectGroupRegisterDispatchResponse::Retired,
        };
        if matches!(
            response,
            EffectGroupRegisterDispatchResponse::Registered
                | EffectGroupRegisterDispatchResponse::AlreadyRegistered
        ) {
            resolve_group_waits(
                &ctx,
                &self.namespace,
                &shape.wait_scope,
                &group_key,
                expected_positions
                    .iter()
                    .map(|&position| {
                        (
                            EffectGroupWaitKind::Admit(position),
                            EffectGroupWaitResolution::Admit,
                        )
                    })
                    .chain(std::iter::once((
                        EffectGroupWaitKind::Ready,
                        EffectGroupWaitResolution::Ready,
                    ))),
            )
            .await?;
        }
        Ok(Reply::at(wire, response))
    }

    async fn register_refusal(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupRefusalRequest>,
    ) -> HandlerResult<Reply<EffectGroupRegisterRefusalResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupRegisterRefusalResponse::UnknownGroup,
            ));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupRegisterRefusalResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let response = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { live, .. } => {
                record.lifecycle = EffectGroupLifecycle::Closed {
                    effective: EffectGroupCloseOutcome::Refused {
                        reason: request.reason.clone(),
                    },
                    reopened: false,
                    addresses: BTreeMap::new(),
                    live: live.clone(),
                };
                store_index(&ctx, object.writer, record.clone());
                let refused = EffectGroupWaitResolution::Refused {
                    reason: request.reason.clone(),
                };
                resolve_group_waits(
                    &ctx,
                    &self.namespace,
                    &shape.wait_scope,
                    &group_key,
                    std::iter::once((EffectGroupWaitKind::Ready, refused.clone())).chain(
                        (0..shape.children()).map(|position| {
                            (EffectGroupWaitKind::Admit(position), refused.clone())
                        }),
                    ),
                )
                .await?;
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
        Ok(Reply::at(wire, response))
    }

    async fn admit_child(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupAdmissionRequest>,
    ) -> HandlerResult<Reply<EffectGroupAdmissionResponse>> {
        let (wire, request) = call.open()?;
        object_state::admit_exclusive_read(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        #[cfg(test)]
        let group_key = ctx.key().to_string();
        let Some(record) = load_index(&ctx).await? else {
            return Ok(Reply::at(wire, EffectGroupAdmissionResponse::Refused));
        };
        let response = decide_group_child_admission(
            &record.lifecycle,
            request.position,
            &request.invocation_id,
        );
        #[cfg(test)]
        if response == EffectGroupAdmissionResponse::NotYetRecorded {
            admission_witness::notify(&group_key);
        }
        Ok(Reply::at(wire, response))
    }

    /// The §4 point for one child's final record: `pending` to `committed`,
    /// reserving the settlement rank the child's seat later publishes
    /// (FIG-4308). A cancel decision takes its rank from the same counter, so
    /// rank order is the order of §4 decisions.
    ///
    /// The decision and the rank commit here, before the child's payload and
    /// settlement writes, so a completion reaching the index after a cancel
    /// decision is refused by name and a redrive reads its own commit back
    /// instead of re-deciding: a repeated commit allocates nothing. A repeat
    /// also answers the §5 barrier as the index sees it — every committed
    /// sibling below the reserved rank still owed a seat — for a seat that did
    /// not win the commit and so cannot know what it declared.
    async fn commit_child(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupCommitChildRequest>,
    ) -> HandlerResult<Reply<EffectGroupCommitChildResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupCommitChildResponse::UnknownGroup,
            ));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupCommitChildResponse::Retired));
        }
        let live = record.live_mut()?;
        let Some(position) = live
            .shape
            .replay_keys
            .iter()
            .position(|replay_key| replay_key == &request.replay_key)
        else {
            return Ok(Reply::at(
                wire,
                EffectGroupCommitChildResponse::UnknownChild,
            ));
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
                return Ok(Reply::at(
                    wire,
                    EffectGroupCommitChildResponse::CancelDecided { rank },
                ));
            }
            Some(EffectGroupChildCommitState::Committed { rank }) => {
                return Ok(Reply::at(
                    wire,
                    EffectGroupCommitChildResponse::AlreadyCommitted {
                        rank,
                        blocking_positions: blocking_positions(live, rank),
                    },
                ));
            }
            None => {}
        }
        let rank = live.reserve_rank(&group_key)?;
        live.commit_states
            .insert(position, EffectGroupChildCommitState::Committed { rank });
        store_index(&ctx, object.writer, record);
        Ok(Reply::at(
            wire,
            EffectGroupCommitChildResponse::Committed { rank },
        ))
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
    async fn admit_semantic(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupAdmitSemanticRequest>,
    ) -> HandlerResult<Reply<EffectGroupAdmitSemanticResponse>> {
        let (wire, request) = call.open()?;
        object_state::admit_exclusive_read(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let Some(record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupAdmitSemanticResponse::UnknownGroup,
            ));
        };
        let Ok(live) = record.live() else {
            return Ok(Reply::at(
                wire,
                EffectGroupAdmitSemanticResponse::UnknownGroup,
            ));
        };
        let Some(position) = live
            .shape
            .replay_keys
            .iter()
            .position(|replay_key| replay_key == &request.replay_key)
        else {
            return Ok(Reply::at(
                wire,
                EffectGroupAdmitSemanticResponse::UnknownChild,
            ));
        };
        Ok(Reply::at(
            wire,
            match live.commit_states.get(&position).copied() {
                Some(EffectGroupChildCommitState::CancelDecided) => {
                    EffectGroupAdmitSemanticResponse::CancelDecided
                }
                _ => EffectGroupAdmitSemanticResponse::Admitted,
            },
        ))
    }

    /// Every committed sibling ranked below `rank` that has not seated yet.
    /// An absent or retired group holds no committed children, so nothing
    /// blocks: retirement releases every barrier.
    async fn drain_blockers(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<EffectGroupDrainBlockersRequest>,
    ) -> HandlerResult<Reply<EffectGroupDrainBlockersResponse>> {
        let (wire, request) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let response = match load_index_shared(&ctx).await? {
            Some(record) => match record.live() {
                Ok(live) => {
                    let positions = blocking_positions(live, request.rank);
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
        Ok(Reply::at(wire, response))
    }

    async fn record_settlement(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupRecordSettlementRequest>,
    ) -> HandlerResult<Reply<EffectGroupRecordSettlementResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupRecordSettlementResponse::UnknownGroup,
            ));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(
                wire,
                EffectGroupRecordSettlementResponse::Retired,
            ));
        }
        let live = record.live_mut()?;
        if request.position >= live.shape.children() {
            return Ok(Reply::at(
                wire,
                EffectGroupRecordSettlementResponse::UnknownChild,
            ));
        }
        // The §4 point is the decision, not the seat: a child whose cancel
        // disposition committed first is refused by name, one whose own
        // commit landed publishes the rank that commit reserved, and a
        // settled one reports its rank back idempotently. A settlement
        // arriving with no commit at all is a protocol defect — `commit_child`
        // is the only writer of `Committed` and it runs before any payload
        // exists to settle.
        let rank = match live.commit_states.get(&request.position).copied() {
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
                return Ok(Reply::at(
                    wire,
                    EffectGroupRecordSettlementResponse::CancelDecided { rank },
                ));
            }
            Some(EffectGroupChildCommitState::Committed { rank })
                if live.settled_positions.contains_key(&request.position) =>
            {
                let seated = live
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
                if seated != rank {
                    return Err(TerminalError::new(format!(
                        "effect group {group_key} child {} seated rank {seated}, but its \
                         commit reserved rank {rank}; a seat publishes only its reserved rank",
                        request.position
                    ))
                    .into());
                }
                resolve_group_wait(
                    &ctx,
                    &self.namespace,
                    &live.shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Rank(rank),
                    EffectGroupWaitResolution::Rank,
                )
                .await?;
                return Ok(Reply::at(
                    wire,
                    EffectGroupRecordSettlementResponse::Duplicate { rank },
                ));
            }
            Some(EffectGroupChildCommitState::Committed { rank }) => rank,
            None => {
                return Err(TerminalError::new(format!(
                    "effect group {group_key} child {} reached record_settlement with no \
                     committed §4 decision; commit_child is the only writer of Committed \
                     and must run before the child's payload exists to settle",
                    request.position
                ))
                .into());
            }
        };
        let settlement = EffectGroupSettlementRecord {
            position: request.position,
            sequence: rank,
            terminal: request.terminal,
        };
        live.settlements.insert(rank, settlement);
        live.settled_positions.insert(request.position, rank);
        let wait_scope = live.shape.wait_scope.clone();
        let replay_key = live.shape.member_replay_key(request.position)?.to_string();
        store_index(&ctx, object.writer, record.clone());
        // The seat's three wakes are independent, so they resolve in one
        // round trip while this exclusive handler holds the group (FIG-4269).
        // The rank wake is the opener's. The drained wake is what a §5
        // barrier parks on: siblings that committed above this child resume
        // their drains off it. A seated child is past every cancel: its
        // cancel wait ends here, so the dispatch invocation's watch on it
        // does not stay open on the deployment until the group closes or
        // retires.
        resolve_group_waits(
            &ctx,
            &self.namespace,
            &wait_scope,
            &group_key,
            [
                (
                    EffectGroupWaitKind::Rank(rank),
                    EffectGroupWaitResolution::Rank,
                ),
                (
                    EffectGroupWaitKind::Drained(request.position),
                    EffectGroupWaitResolution::Drained,
                ),
                (
                    EffectGroupWaitKind::Cancel(&replay_key),
                    EffectGroupWaitResolution::Settled,
                ),
            ],
        )
        .await?;
        Ok(Reply::at(
            wire,
            EffectGroupRecordSettlementResponse::Recorded { rank },
        ))
    }

    async fn read_rank(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<EffectGroupReadRankRequest>,
    ) -> HandlerResult<Reply<EffectGroupReadRankResponse>> {
        let (wire, request) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let Some(record) = load_index_shared(&ctx).await? else {
            return Ok(Reply::at(wire, EffectGroupReadRankResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupReadRankResponse::Retired));
        }
        let closed_to_caller = matches!(
            &record.lifecycle,
            EffectGroupLifecycle::Closed {
                effective,
                reopened,
                ..
            } if !reopened || matches!(effective, EffectGroupCloseOutcome::Refused { .. })
        );
        // A caller is refused its closed group whether or not the rank settled.
        if request.for_caller && closed_to_caller {
            return Ok(Reply::at(wire, EffectGroupReadRankResponse::Closed));
        }
        let live = record.live()?;
        // A rank is served only inside the seated prefix: every rank up to it
        // is seated (FIG-4308). A rank reserved at its commit may still be
        // unpublished while a higher one has seated, and no reader — the
        // consuming await or the cursorless read — is answered past that
        // hole. An unserved rank of a group closed to its caller answers
        // `Closed` to every reader. A reopened caller parks like a live group:
        // an RTC loser still lands, and a committed child under Cancel seats
        // its rank when the drain finishes (FIG-3481).
        let served = (1..=live.seated_prefix())
            .contains(&request.rank)
            .then(|| live.settlements.get(&request.rank).cloned())
            .flatten();
        let Some(settlement) = served else {
            return Ok(Reply::at(
                wire,
                if closed_to_caller {
                    EffectGroupReadRankResponse::Closed
                } else {
                    EffectGroupReadRankResponse::NotSettled
                },
            ));
        };
        if request.run {
            let group_key = ctx.key().to_string();
            let ranks = served_run(&ctx, &self.namespace, &group_key, live, request.rank).await?;
            return Ok(Reply::at(
                wire,
                EffectGroupReadRankResponse::SettledRun { ranks },
            ));
        }
        let child_replay_key = live
            .shape
            .member_replay_key(settlement.position)?
            .to_string();
        Ok(Reply::at(
            wire,
            EffectGroupReadRankResponse::Settled {
                settlement,
                child_replay_key,
            },
        ))
    }

    async fn close(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupCloseRequest>,
    ) -> HandlerResult<Reply<EffectGroupCloseResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(wire, EffectGroupCloseResponse::UnknownGroup));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupCloseResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let (declared, addresses, prior) = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => {
                return Ok(Reply::at(wire, EffectGroupCloseResponse::NotReady));
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
                    EffectGroupCloseOutcome::RunToCompletion => LoserPolicy::RunToCompletion,
                    EffectGroupCloseOutcome::Cancel => LoserPolicy::Cancel,
                    EffectGroupCloseOutcome::Refused { .. } => {
                        return Ok(Reply::at(wire, EffectGroupCloseResponse::AlreadyClosed));
                    }
                };
                (declared, addresses.clone(), Some(effective.clone()))
            }
            EffectGroupLifecycle::Retired { .. } => {
                return Ok(Reply::at(wire, EffectGroupCloseResponse::Retired));
            }
        };
        let effective = match LoserPolicy::resolve_close(declared, request.disposition) {
            Ok(effective) => effective,
            Err(_) => return Ok(Reply::at(wire, EffectGroupCloseResponse::WidenRefused)),
        };
        if prior.as_ref() == Some(&EffectGroupCloseOutcome::from(effective)) {
            return Ok(Reply::at(wire, EffectGroupCloseResponse::AlreadyClosed));
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
                let rank = live.reserve_rank(&group_key)?;
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
        seal_cancel_decisions(&ctx, &self.namespace, &group_key, &decided).await?;
        let live = record.live()?.clone();
        record.lifecycle = EffectGroupLifecycle::Closed {
            effective: effective.into(),
            reopened: false,
            addresses: addresses.clone(),
            live,
        };
        store_index(&ctx, object.writer, record.clone());
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
                resolve_group_waits(
                    &ctx,
                    &self.namespace,
                    &shape.wait_scope,
                    &group_key,
                    [
                        (
                            EffectGroupWaitKind::Rank(rank),
                            EffectGroupWaitResolution::Rank,
                        ),
                        (
                            EffectGroupWaitKind::Cancel(shape.member_replay_key(position)?),
                            EffectGroupWaitResolution::Cancel,
                        ),
                        (
                            EffectGroupWaitKind::Admit(position),
                            EffectGroupWaitResolution::Cancel,
                        ),
                    ],
                )
                .await?;
                if let Some(invocation_id) = addresses.get(&position) {
                    ctx.invocation_handle(invocation_id.clone()).cancel();
                }
            }
        }
        Ok(Reply::at(wire, EffectGroupCloseResponse::Closed))
    }

    async fn retire(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupRetireResponse>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(wire, EffectGroupRetireResponse::UnknownGroup));
        };
        if let EffectGroupLifecycle::Retired { cleanup } = &record.lifecycle {
            return Ok(Reply::at(
                wire,
                match cleanup {
                    EffectGroupCleanup::Pending { facts, .. } => {
                        EffectGroupRetireResponse::AlreadyRetired {
                            cleanup: facts.clone(),
                        }
                    }
                    EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
                },
            ));
        }
        let (shape, live) = {
            let live = record.live()?;
            (live.shape.clone(), live.clone())
        };
        let (dispatcher, dispatched) = match &record.lifecycle {
            // Before registration no child id is recorded: retirement cancels
            // the adopted dispatcher, and the child calls it tracks with it.
            EffectGroupLifecycle::Preparing { dispatch, .. } => (dispatch.clone(), BTreeMap::new()),
            EffectGroupLifecycle::Ready { addresses, .. }
            | EffectGroupLifecycle::Closed { addresses, .. } => {
                // A workflow run is exactly-once per workflow key. Re-sending
                // the completed run recovers its stable invocation id without
                // re-executing it; if a test assembled Ready directly, the
                // probe guard makes the newly-created run exit before sending.
                // The send addresses the recorded dispatch route (FIG-3795
                // S10), never a name recomputed from this build.
                let group_key = ctx.key().to_string();
                let handle = ctx
                    .request::<Call<EffectGroupDispatchRequest>, Reply<()>>(
                        restate_sdk::context::RequestTarget::workflow(
                            record.dispatch_route.clone(),
                            group_key.clone(),
                            "run",
                        ),
                        Call::journaled(EffectGroupDispatchRequest {
                            group_key: group_key.clone(),
                        }),
                    )
                    .send()
                    .await?;
                (
                    EffectGroupDispatchState::Adopted {
                        id: handle.invocation_id().to_owned(),
                    },
                    addresses.clone(),
                )
            }
            EffectGroupLifecycle::Retired { cleanup } => {
                return Ok(Reply::at(
                    wire,
                    match cleanup {
                        EffectGroupCleanup::Pending { facts, .. } => {
                            EffectGroupRetireResponse::AlreadyRetired {
                                cleanup: facts.clone(),
                            }
                        }
                        EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
                    },
                ));
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
        store_index(&ctx, object.writer, record);
        Ok(Reply::at(
            wire,
            EffectGroupRetireResponse::Retired { cleanup },
        ))
    }

    async fn finish_retirement(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupFinishRetirementResponse>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupFinishRetirementResponse::UnknownGroup,
            ));
        };
        let response = match record.lifecycle {
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Pending { .. },
            } => {
                record.lifecycle = EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Complete,
                };
                store_index(&ctx, object.writer, record);
                // Nothing rebuilds a child of a tombstone.
                ctx.clear(MEMBERSHIP_STATE_KEY);
                EffectGroupFinishRetirementResponse::Finished
            }
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Complete,
            } => EffectGroupFinishRetirementResponse::AlreadyFinished,
            _ => EffectGroupFinishRetirementResponse::NotRetired,
        };
        Ok(Reply::at(wire, response))
    }

    async fn retirement_cancel(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<()>,
    ) -> HandlerResult<Reply<EffectGroupRetirementCancelResponse>> {
        let (wire, ()) = call.open()?;
        let object = self.admit(&ctx).await?;
        let group_key = ctx.key().to_string();
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupRetirementCancelResponse::UnknownGroup,
            ));
        };
        let mut decided = Vec::new();
        let (facts, ranks, changed) = {
            let (facts, live) = match &mut record.lifecycle {
                EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Pending { facts, live },
                } => (facts, live),
                EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Complete,
                } => {
                    return Ok(Reply::at(
                        wire,
                        EffectGroupRetirementCancelResponse::Tombstone,
                    ));
                }
                _ => {
                    return Ok(Reply::at(
                        wire,
                        EffectGroupRetirementCancelResponse::NotRetired,
                    ));
                }
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
                let rank = live.reserve_rank(&group_key)?;
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
            (facts.clone(), ranks, changed)
        };
        seal_cancel_decisions(&ctx, &self.namespace, &group_key, &decided).await?;
        store_index(&ctx, object.writer, record.clone());
        for (position, rank) in ranks.iter().copied() {
            resolve_group_waits(
                &ctx,
                &self.namespace,
                &facts.wait_scope,
                &group_key,
                [
                    (
                        EffectGroupWaitKind::Rank(rank),
                        EffectGroupWaitResolution::Rank,
                    ),
                    (
                        EffectGroupWaitKind::Cancel(facts.member_replay_key(position)?),
                        EffectGroupWaitResolution::Cancel,
                    ),
                    (
                        EffectGroupWaitKind::Admit(position),
                        EffectGroupWaitResolution::Cancel,
                    ),
                ],
            )
            .await?;
        }
        Ok(Reply::at(
            wire,
            if changed {
                EffectGroupRetirementCancelResponse::Applied
            } else {
                EffectGroupRetirementCancelResponse::AlreadyApplied
            },
        ))
    }
}

mod child_cancel;
mod dispatch;
mod payload;
pub(crate) use child_cancel::{GroupChildCancel, group_child_cancel_verdict};
#[cfg(test)]
pub(crate) use dispatch::EffectGroupChildRequest;
pub use dispatch::EffectGroupDispatchRequest;
pub(crate) use dispatch::{EffectGroupDispatch, EffectGroupDispatchImpl};
pub(crate) use payload::{
    EFFECT_GROUP_PAYLOAD_FAMILY, EffectGroupPayload, EffectGroupPayloadClient,
    EffectGroupPayloadImpl,
};
pub use payload::{
    EFFECT_GROUP_PAYLOAD_FORMAT_VERSION, EffectGroupPayloadGetResponse,
    EffectGroupPayloadPutRequest, EffectGroupPayloadPutResponse,
};
pub(crate) fn payload_key(group_key: &str, position: usize) -> String {
    let digest = Sha256::digest(group_key.as_bytes());
    format!("{:x}:{position}", digest)
}

pub(crate) fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

pub(crate) fn ingress_group_error(
    operation: &str,
    error: crate::RestateHttpError,
) -> RuntimeEffectControllerError {
    if let Some(refusal) = crate::object_state::ingress_stored_format_refusal(&error) {
        return refusal;
    }
    let message = format!("Restate effect-group operation {operation} failed: {error}");
    match error.classification() {
        crate::RestateHttpErrorClass::Transient => {
            RuntimeEffectControllerError::new(RuntimeErrorCode::EngineAwaitEventAwait, message)
        }
        crate::RestateHttpErrorClass::Terminal if error.is_service_unregistered() => {
            RuntimeEffectControllerError::new(RuntimeErrorCode::EngineServiceUnregistered, message)
        }
        crate::RestateHttpErrorClass::Terminal => group_shape_error(message),
    }
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
