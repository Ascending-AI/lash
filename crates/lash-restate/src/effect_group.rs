#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by Lash durable waits"
)]

//! Restate-native durable effect groups.
//!
//! The index owns group lifecycle and settlement rank, the payload object owns
//! successful result bytes and its object-local retirement fence, and the
//! dispatch workflow owns every child send. The index also owns the group's
//! own notifications — readiness, a seated rank, a lifted §5 barrier, a
//! child's cancel fact (FIG-4344): a waiter subscribes an awakeable of its own
//! journal, and the index handler whose transition makes the notice true
//! completes it from its own journal, so no group-internal notification goes
//! through the generic durable-wait services.

/// version_surface = "coexist"
/// version_guard(items(EFFECT_GROUP_PREFIX_VERSION, committed_final_state_key))
const EFFECT_GROUP_PREFIX_VERSION: &str = "effect-group/v1/committed-final/";

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(test)]
use std::sync::Mutex;

use lash_core::{
    GroupExecutors, GroupSettlement, LoserPolicy, RuntimeEffectControllerError,
    RuntimeEffectEnvelope, RuntimeEffectOutcome, RuntimeErrorCode,
};
use restate_sdk::context::{
    CallFuture, ContextAwakeables, ContextClient, ContextSideEffects, ContextWriteState,
    ObjectContext, RunFuture, RunRetryPolicy, SharedObjectContext, SharedWorkflowContext,
    WorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::RestateIngressClient;
use crate::compat::{Call, Reply};
use crate::durable_wait::{
    LASH_REPLAY_KEY_HEADER, RestateDurableWaitGroupChildRequest, durable_wait_index_key_for_scope,
};
use crate::object_state::{
    self, FleetView, ObjectFamily, ObjectUpgradeResponse, StoredValueFormats,
};

/// version_surface = "coexist"
/// version_guard(items(INDEX_STATE_KEY, store_index))
const INDEX_STATE_KEY: &str = "effect-group/v1/state";
/// The group's accepted membership, apart from the index record every
/// per-child handler reads (FIG-4068): written once at open, read only where
/// children are rebuilt, and cleared when retirement completes.
/// version_surface = "coexist"
/// version_guard(items(MEMBERSHIP_STATE_KEY, finish_retirement, store_membership))
const MEMBERSHIP_STATE_KEY: &str = "effect-group/v1/membership";
/// The final a child's winning commit offered, one key per position, apart
/// from the index record every handler reads: a tool child's is its sealed
/// drain input, which only a later commit of the same child reads. Written
/// with the commit, and cleared when retirement completes.
fn committed_final_state_key(position: usize) -> String {
    format!("{EFFECT_GROUP_PREFIX_VERSION}{position}")
}

mod drain_barrier;
pub(crate) mod drain_index;
mod group_waits;
mod notifications;
mod protocol;
mod rank_run;
mod recovery;
pub(crate) use recovery::{INDEX_RECORD_KEY, undrained_children_on};
mod reopen;
mod wire;
use drain_barrier::blocking_positions;
use group_waits::seal_cancel_decisions;
#[cfg(test)]
pub(crate) use notifications::subscription_ceiling;
pub use notifications::{
    EffectGroupChildCancelRequest, EffectGroupNotice, EffectGroupNotification,
    EffectGroupSubscribeRequest, EffectGroupSubscribeResponse, EffectGroupUnsubscribeRequest,
};
pub(crate) use notifications::{
    await_group_notice, await_group_notice_via_ingress, subscription_refused,
};
pub(crate) use protocol::EFFECT_GROUP_STATE_FAMILY;
pub(crate) use protocol::EFFECT_GROUP_STATE_FORMATS;
pub use protocol::{EFFECT_GROUP_DISPATCH_JOURNAL_VERSION, EFFECT_GROUP_STATE_FORMAT_VERSION};
use protocol::{load_committed_final, load_index, load_index_shared, load_membership};
use rank_run::served_run;
pub(crate) use reopen::{content_checked_shape_mismatch, content_mismatch};
pub(crate) use wire::btree_map_as_pairs;
pub use wire::{
    EffectGroupAdmitSemanticRequest, EffectGroupAdmitSemanticResponse, EffectGroupPhase,
    EffectGroupProbeResponse,
};
pub(crate) use wire::{EffectGroupOpenerRequest, EffectGroupOpenerResponse};

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

#[cfg(test)]
#[path = "tests/effect_group_drain_cut.rs"]
pub(crate) mod drain_cut;

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
    #[shared]
    async fn opener(
        call: Call<EffectGroupOpenerRequest>,
    ) -> HandlerResult<Reply<EffectGroupOpenerResponse>>;
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
    /// Answer a notice from the record, or record the caller's awakeable to
    /// be completed once the notice is true (FIG-4344).
    async fn subscribe(
        call: Call<EffectGroupSubscribeRequest>,
    ) -> HandlerResult<Reply<EffectGroupSubscribeResponse>>;
    /// Drop a subscriber whose race its other arm won.
    async fn unsubscribe(call: Call<EffectGroupUnsubscribeRequest>) -> HandlerResult<Reply<()>>;
    /// A child's cancel fact as the record holds it: the step-boundary read.
    #[shared]
    async fn child_cancel(
        call: Call<EffectGroupChildCancelRequest>,
    ) -> HandlerResult<Reply<Option<EffectGroupNotification>>>;
    /// A notice awaited from outside any handler: this shared handler holds
    /// the awakeable, so the waiter never holds the group's exclusive lock.
    #[shared]
    async fn await_notice(
        call: Call<EffectGroupNotice>,
    ) -> HandlerResult<Reply<EffectGroupNotification>>;
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
                live.shape.children().saturating_sub(live.seated())
            }),
            None => 0,
        };
        Ok(Reply::at(wire, unsettled))
    }

    /// What the group still needs of one paused dispatcher invocation
    /// (FIG-4617, FIG-4630). Retirement needs its owner through the cleanup
    /// and reply windows, including the final tombstone; a child is needed
    /// until its position is seated, whatever the group's phase.
    async fn opener(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<EffectGroupOpenerRequest>,
    ) -> HandlerResult<Reply<EffectGroupOpenerResponse>> {
        let (wire, request) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let response = match load_index_shared(&ctx).await? {
            Some(record) => paused_work_need(record.lifecycle, &request),
            None => EffectGroupOpenerResponse::Seated,
        };
        Ok(Reply::at(wire, response))
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
        // a deployment binds (FIG-3795), and a generation's lane (FIG-4454):
        // an opener cannot declare a route no dispatch could ever run under,
        // nor one that hands the group's children to whichever build is
        // newest.
        if !self
            .namespace
            .parse(&request.dispatch_route)
            .is_some_and(|route| {
                route.service() == crate::LashService::EffectGroupDispatch
                    && matches!(route.lane(), crate::services::Lane::Generation(_))
            })
        {
            return Err(TerminalError::new(format!(
                "effect group {} open declared dispatch route `{}`, which names no \
                 generation lane of EffectGroupDispatch",
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
                        live: EffectGroupStateLiveRecord::undecided(request.shape),
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
        // A reopen recovers every committed child whose seat is owed: its
        // committing invocation may be gone, and nothing else would drain it
        // (FIG-4454).
        if matches!(
            &record.lifecycle,
            EffectGroupLifecycle::Ready { .. }
                | EffectGroupLifecycle::Closed {
                    effective: EffectGroupCloseOutcome::RunToCompletion
                        | EffectGroupCloseOutcome::Cancel,
                    ..
                }
        ) {
            recovery::resend_owed_children(&ctx, &self.namespace, &record).await?;
        }
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
        // A dispatcher started for a group that is already ready or closed
        // sends no child of its own, so the index recovers every committed
        // child whose seat is owed (FIG-4454).
        let ready_or_closed = match &record.lifecycle {
            EffectGroupLifecycle::Ready { .. } => Some(EffectGroupProbeAdoptResponse::Ready),
            EffectGroupLifecycle::Closed { .. } => Some(EffectGroupProbeAdoptResponse::Closed),
            EffectGroupLifecycle::Preparing { .. } | EffectGroupLifecycle::Retired { .. } => None,
        };
        if let Some(response) = ready_or_closed {
            recovery::resend_owed_children(&ctx, &self.namespace, &record).await?;
            return Ok(Reply::at(wire, response));
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
    /// precedes admission) keeps its seat. The opener and every child waiting
    /// for readiness are notified from this handler's own journal; a redriven
    /// registration of the same map notifies whoever still waits, which is
    /// idempotent. A retired group is never made ready again.
    async fn register_dispatch(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupRegisterDispatchRequest>,
    ) -> HandlerResult<Reply<EffectGroupRegisterDispatchResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
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
                store_index(&ctx, object.writer, record.clone());
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
            notifications::notify_satisfied(&ctx, object.writer, &record).await?;
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
        let Some(mut record) = load_index(&ctx).await? else {
            return Ok(Reply::at(
                wire,
                EffectGroupRegisterRefusalResponse::UnknownGroup,
            ));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Reply::at(wire, EffectGroupRegisterRefusalResponse::Retired));
        }
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
                // The opener and every child waiting for readiness learn of
                // the refusal from this handler's own journal.
                notifications::notify_satisfied(&ctx, object.writer, &record).await?;
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
    /// instead of re-deciding: a repeated commit allocates nothing.
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
        match live.decision(position) {
            Some((rank, EffectGroupSeat::CancelDecided)) => {
                return Ok(Reply::at(
                    wire,
                    EffectGroupCommitChildResponse::CancelDecided { rank },
                ));
            }
            Some((rank, EffectGroupSeat::Committed | EffectGroupSeat::Seated { .. })) => {
                let committed = load_committed_final(&ctx, &committed_final_state_key(position))
                    .await?
                    .ok_or_else(|| {
                        TerminalError::new(format!(
                            "effect group {group_key} child {position} is committed at rank \
                         {rank} but retains no committed final; the two commit in one \
                         handler"
                        ))
                    })?;
                return Ok(Reply::at(
                    wire,
                    EffectGroupCommitChildResponse::AlreadyCommitted { rank, committed },
                ));
            }
            None => {}
        }
        let rank = live.decide(position, EffectGroupSeat::Committed);
        drain_index::register(&ctx, &self.namespace, &record).await?;
        #[cfg(test)]
        drain_cut::pause(&ctx, &group_key, "before_commit").await?;
        store_index(&ctx, object.writer, record);
        object_state::set_stamped(
            &ctx,
            &committed_final_state_key(position),
            object.writer,
            request.committed,
        );
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
            match live.decision(position) {
                Some((_, EffectGroupSeat::CancelDecided)) => {
                    EffectGroupAdmitSemanticResponse::CancelDecided
                }
                Some((_, EffectGroupSeat::Committed | EffectGroupSeat::Seated { .. })) | None => {
                    EffectGroupAdmitSemanticResponse::Admitted
                }
            },
        ))
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
        let rank = match live.decision(request.position) {
            Some((rank, EffectGroupSeat::CancelDecided)) => {
                return Ok(Reply::at(
                    wire,
                    EffectGroupRecordSettlementResponse::CancelDecided { rank },
                ));
            }
            Some((rank, EffectGroupSeat::Seated { .. })) => {
                // A redriven seat completes whoever still waits on it.
                notifications::notify_satisfied(&ctx, object.writer, &record).await?;
                return Ok(Reply::at(
                    wire,
                    EffectGroupRecordSettlementResponse::Duplicate { rank },
                ));
            }
            Some((rank, EffectGroupSeat::Committed)) => rank,
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
        live.seat(
            rank,
            EffectGroupSeat::Seated {
                terminal: request.terminal,
            },
        );
        store_index(&ctx, object.writer, record.clone());
        #[cfg(test)]
        drain_cut::pause(&ctx, &group_key, "after_seat").await?;
        // The seat notifies from its own journal and calls no other service
        // (FIG-4344): the opener parked on a rank this seat completes the
        // prefix to, every §5 barrier this seat lifts, and every watch of
        // this child's cancel fact, which a seated child is past.
        notifications::notify_satisfied(&ctx, object.writer, &record).await?;
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
            .then(|| live.settlement(request.rank))
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
        // The §4 decision, not the seat: a committed child is protected by
        // the decision it holds, and an already-cancelled one keeps its first
        // seat.
        let decided = if effective == LoserPolicy::Cancel {
            record.live_mut()?.decide_pending_cancelled()
        } else {
            Vec::new()
        };
        seal_cancel_decisions(&ctx, &self.namespace, &group_key, &decided).await?;
        let live = record.live()?.clone();
        record.lifecycle = EffectGroupLifecycle::Closed {
            effective: effective.into(),
            reopened: false,
            addresses: addresses.clone(),
            live,
        };
        store_index(&ctx, object.writer, record.clone());
        // The decisions are sealed and stored before anyone hears of them:
        // the opener parked on a rank a cancel seat completes, and every
        // watch of a decided child's cancel fact, are notified from this
        // handler's own journal, and only then are the decided children's
        // invocations interrupted.
        notifications::notify_satisfied(&ctx, object.writer, &record).await?;
        if effective == LoserPolicy::Cancel {
            let live = record.live()?;
            for position in 0..shape.children() {
                // A committed-but-undrained child is a pending protected drain
                // (ADR 0099 §4): the cancel decision refused it, so the close
                // seats no rank for it and does not interrupt its invocation —
                // its `record_settlement` seats the rank and notifies its
                // waiters when the drain finishes.
                if matches!(
                    live.decision(position),
                    Some((_, EffectGroupSeat::Committed))
                ) {
                    continue;
                }
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
                    EffectGroupCleanup::Complete { .. } => EffectGroupRetireResponse::Tombstone,
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
                        EffectGroupCleanup::Complete { .. } => EffectGroupRetireResponse::Tombstone,
                    },
                ));
            }
        };
        let cleanup = EffectGroupCleanupFacts {
            replay_keys: shape.replay_keys,
            dispatcher,
            dispatched,
        };
        record.lifecycle = EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Pending {
                facts: cleanup.clone(),
                live: Box::new(live),
            },
        };
        store_index(&ctx, object.writer, record);
        // Retirement answers every notice: each waiter is completed `Retired`
        // from this handler's journal, and a later one is answered from the
        // retired record.
        notifications::notify_retired(&ctx, object.writer).await?;
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
                cleanup: EffectGroupCleanup::Pending { facts, live },
            } => {
                record.lifecycle = EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Complete {
                        opener: live.shape.opener,
                    },
                };
                store_index(&ctx, object.writer, record);
                // Nothing rebuilds a child of a tombstone, and nothing seats
                // one of its committed finals.
                ctx.clear(MEMBERSHIP_STATE_KEY);
                for position in 0..facts.children() {
                    ctx.clear(&committed_final_state_key(position));
                }
                EffectGroupFinishRetirementResponse::Finished
            }
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Complete { .. },
            } => EffectGroupFinishRetirementResponse::AlreadyFinished,
            EffectGroupLifecycle::Preparing { .. }
            | EffectGroupLifecycle::Ready { .. }
            | EffectGroupLifecycle::Closed { .. } => {
                EffectGroupFinishRetirementResponse::NotRetired
            }
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
        let decided = {
            let live = match &mut record.lifecycle {
                EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Pending { live, .. },
                } => live,
                EffectGroupLifecycle::Retired {
                    cleanup: EffectGroupCleanup::Complete { .. },
                } => {
                    return Ok(Reply::at(
                        wire,
                        EffectGroupRetirementCancelResponse::Tombstone,
                    ));
                }
                EffectGroupLifecycle::Preparing { .. }
                | EffectGroupLifecycle::Ready { .. }
                | EffectGroupLifecycle::Closed { .. } => {
                    return Ok(Reply::at(
                        wire,
                        EffectGroupRetirementCancelResponse::NotRetired,
                    ));
                }
            };
            // Same §4 arbitration as a `Cancel` close: a committed child keeps
            // its own settlement, and only the undecided are seated cancelled.
            // A committed-but-undrained child is a pending protected drain
            // (ADR 0099 §4): its seat is still owed. Nobody waits to hear of
            // these decisions: `retire` already answered every subscriber
            // `Retired`, and a later one is answered from the retired record.
            live.decide_pending_cancelled()
        };
        seal_cancel_decisions(&ctx, &self.namespace, &group_key, &decided).await?;
        store_index(&ctx, object.writer, record);
        Ok(Reply::at(
            wire,
            if !decided.is_empty() {
                EffectGroupRetirementCancelResponse::Applied
            } else {
                EffectGroupRetirementCancelResponse::AlreadyApplied
            },
        ))
    }

    async fn subscribe(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupSubscribeRequest>,
    ) -> HandlerResult<Reply<EffectGroupSubscribeResponse>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        let response = notifications::subscribe(&ctx, object.writer, request).await?;
        Ok(Reply::at(wire, response))
    }

    async fn unsubscribe(
        &self,
        ctx: ObjectContext<'_>,
        call: Call<EffectGroupUnsubscribeRequest>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let object = self.admit(&ctx).await?;
        notifications::unsubscribe(&ctx, object.writer, request).await?;
        Ok(Reply::at(wire, ()))
    }

    async fn child_cancel(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<EffectGroupChildCancelRequest>,
    ) -> HandlerResult<Reply<Option<EffectGroupNotification>>> {
        let (wire, request) = call.open()?;
        object_state::admit_shared(&ctx, &EFFECT_GROUP_STATE_FAMILY).await?;
        let record = load_index_shared(&ctx).await?;
        let answer = notifications::notice_answer(
            record.as_ref(),
            &EffectGroupNotice::ChildCancel {
                position: request.position,
            },
        );
        Ok(Reply::at(wire, answer))
    }

    async fn await_notice(
        &self,
        ctx: SharedObjectContext<'_>,
        call: Call<EffectGroupNotice>,
    ) -> HandlerResult<Reply<EffectGroupNotification>> {
        let (wire, notice) = call.open()?;
        let group_key = ctx.key().to_string();
        let notification = await_group_notice!(&ctx, &self.namespace, &group_key, notice)?;
        Ok(Reply::at(wire, notification))
    }
}

mod child_cancel;
mod dispatch;
mod payload;
pub(crate) use child_cancel::GroupChildCancel;
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

pub(crate) fn settlement_from_payload(
    rank: u64,
    record: EffectGroupSettlementRecord,
    payload: Option<Vec<u8>>,
) -> Result<GroupSettlement, RuntimeEffectControllerError> {
    let outcome = match record.terminal {
        EffectGroupSettlementTerminal::StoredPayload => {
            let bytes = payload.ok_or_else(|| {
                group_shape_error(format!(
                    "effect group settlement rank {rank} refers to a missing payload"
                ))
            })?;
            serde_json::from_slice::<RuntimeEffectOutcome>(&bytes).map_err(|error| {
                group_shape_error(format!(
                    "decode effect group settlement rank {rank} payload: {error}"
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
        sequence: rank,
        outcome,
    })
}
