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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, GroupExecutors, GroupSettlement,
    GroupWakePolicy, LoserPolicy, Resolution, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectOutcome, RuntimeErrorCode,
};
use restate_sdk::context::{
    ContextClient, ContextReadState, ContextSideEffects, ContextWriteState, ObjectContext,
    RunFuture, RunRetryPolicy, SharedObjectContext, SharedWorkflowContext, WorkflowContext,
};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::RestateIngressClient;
use crate::durable_wait::{
    LashDurableWaitIndexClient, LashDurableWaitIndexImpl, LashDurableWaitWorkflowClient,
    LashDurableWaitWorkflowImpl, RestateDurableWaitAddress, RestateDurableWaitAwaitRequest,
    RestateDurableWaitResolveRequest, durable_wait_index_object_key, restate_await_event_key,
};

const INDEX_STATE_KEY: &str = "effect-group/v1/state";
const PAYLOAD_STATE_KEY: &str = "effect-group/v1/payload";
const PAYLOAD_RETIRED_KEY: &str = "effect-group/v1/retired";

mod btree_map_as_pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<K, V, S>(map: &BTreeMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        K: Ord + Serialize,
        V: Serialize,
        S: Serializer,
    {
        map.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Ord + Deserialize<'de>,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Vec::<(K, V)>::deserialize(deserializer).map(|pairs| pairs.into_iter().collect())
    }
}

/// Constructor-owned deployment services for Restate effect groups.
///
/// The resolver, ingress cancellation observer, and an explicitly infinite
/// `ctx.run` retry policy are mandatory. A deployment also binds the existing
/// `LashDurableWaitWorkflow` and `LashDurableWaitIndex` services.
#[derive(Clone)]
pub struct RestateEffectGroupServices {
    pub index: EffectGroupIndex,
    pub payload: EffectGroupPayload,
    pub dispatch: EffectGroupDispatch,
    pub wait: RestateEffectGroupWaitServices,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RestateEffectGroupWaitServices {
    pub workflow: LashDurableWaitWorkflowImpl,
    pub index: LashDurableWaitIndexImpl,
}

#[derive(Clone, Debug)]
pub struct RestateEffectGroupRetryPolicy(RunRetryPolicy);

impl RestateEffectGroupRetryPolicy {
    /// The uncapped policy required by dispatcher preflight and child runs.
    pub fn infinite() -> Self {
        Self(RunRetryPolicy::new())
    }
}

impl RestateEffectGroupServices {
    pub fn new(
        executors: Arc<dyn GroupExecutors>,
        ingress: RestateIngressClient,
        infinite_retry_policy: RestateEffectGroupRetryPolicy,
    ) -> Self {
        Self {
            index: EffectGroupIndex,
            payload: EffectGroupPayload,
            dispatch: EffectGroupDispatch {
                executors,
                ingress,
                infinite_retry_policy: infinite_retry_policy.0,
                resolved: Arc::new(Mutex::new(HashMap::new())),
            },
            wait: RestateEffectGroupWaitServices::default(),
        }
    }
}

impl std::fmt::Debug for RestateEffectGroupServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateEffectGroupServices")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupShape {
    pub children: usize,
    pub wake: GroupWakePolicy,
    pub loser_disposition: LoserPolicy,
    pub replay_keys: Vec<String>,
    pub wait_scope: ExecutionScope,
}

impl EffectGroupShape {
    pub(crate) fn from_group(
        group: &RuntimeEffectGroup,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let replay_keys = group
            .children()
            .iter()
            .enumerate()
            .map(|(position, child)| {
                child
                    .invocation
                    .replay_key()
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        group_shape_error(format!(
                            "child {position} of effect group {} has no replay key",
                            group.group_key()
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let wait_scope = ExecutionScope::runtime_operation(group.group_key());
        Ok(Self {
            children: group.children().len(),
            wake: group.wake(),
            loser_disposition: group.loser_disposition(),
            replay_keys,
            wait_scope,
        })
    }

    fn digest(&self) -> Result<String, TerminalError> {
        let bytes = serde_json::to_vec(self).map_err(|error| {
            TerminalError::new(format!("serialize effect-group shape: {error}"))
        })?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupCleanupFacts {
    pub children: usize,
    pub replay_keys: Vec<String>,
    pub dispatcher: EffectGroupDispatchState,
    #[serde(with = "btree_map_as_pairs")]
    pub dispatched: BTreeMap<usize, String>,
    pub wait_scope: ExecutionScope,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCleanup {
    Pending { facts: EffectGroupCleanupFacts },
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupLifecycle {
    Preparing {
        dispatch: EffectGroupDispatchState,
    },
    Ready {
        #[serde(with = "btree_map_as_pairs")]
        addresses: BTreeMap<usize, String>,
    },
    Closed {
        effective: EffectGroupCloseDisposition,
        #[serde(with = "btree_map_as_pairs")]
        addresses: BTreeMap<usize, String>,
    },
    Retired {
        cleanup: EffectGroupCleanup,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupCloseDisposition {
    RunToCompletion,
    Cancel,
    Refused { reason: EffectGroupRefusal },
}

impl From<LoserPolicy> for EffectGroupCloseDisposition {
    fn from(value: LoserPolicy) -> Self {
        match value {
            LoserPolicy::RunToCompletion => Self::RunToCompletion,
            LoserPolicy::Cancel => Self::Cancel,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRefusal {
    NoExecutor { position: usize },
    Retired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupSettlementTerminal {
    StoredPayload,
    Failed { error: RuntimeEffectControllerError },
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupSettlementRecord {
    pub position: usize,
    pub sequence: u64,
    pub terminal: EffectGroupSettlementTerminal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EffectGroupIndexRecord {
    shape: EffectGroupShape,
    lifecycle: EffectGroupLifecycle,
    next_rank: u64,
    #[serde(with = "btree_map_as_pairs")]
    settlements: BTreeMap<u64, EffectGroupSettlementRecord>,
    #[serde(with = "btree_map_as_pairs")]
    settled_positions: BTreeMap<usize, u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupPhase {
    Preparing,
    Ready,
    Closed,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupProbeResponse {
    Absent,
    Exists {
        shape_digest: String,
        phase: EffectGroupPhase,
    },
}

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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupProbeAdoptResponse {
    Adopted,
    AlreadyAdopted,
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
    Refused,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupRecordSettlementResponse {
    Recorded { rank: u64 },
    Duplicate { rank: u64 },
    UnknownChild,
    UnknownGroup,
    Retired,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupReadRankResponse {
    Settled {
        settlement: EffectGroupSettlementRecord,
    },
    NotSettled,
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
    Refused { reason: EffectGroupRefusal },
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EffectGroupWaitKind<'a> {
    Ready,
    Rank(u64),
    Cancel(&'a str),
    Admit(usize),
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
            timeout_ms: None,
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
            timeout_ms: None,
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
            timeout_ms: None,
        })
        .map_err(|error| group_shape_error(error.to_string()))
}

fn wait_resolution(value: EffectGroupWaitResolution) -> Result<Resolution, TerminalError> {
    serde_json::to_value(value)
        .map(Resolution::Ok)
        .map_err(|error| TerminalError::new(format!("serialize effect-group wake: {error}")))
}

async fn resolve_group_wait(
    ctx: &ObjectContext<'_>,
    scope: &ExecutionScope,
    group_key: &str,
    kind: EffectGroupWaitKind<'_>,
    value: EffectGroupWaitResolution,
) -> Result<(), TerminalError> {
    let key = group_wait_key(scope, group_key, kind)?;
    let address = RestateDurableWaitAddress::for_key(&key);
    let Json(_) = ctx
        .object_client::<LashDurableWaitIndexClient>(durable_wait_index_object_key(&address))
        .resolve(Json(RestateDurableWaitResolveRequest {
            key,
            resolution: wait_resolution(value)?,
        }))
        .call()
        .await?;
    Ok(())
}

fn phase(lifecycle: &EffectGroupLifecycle) -> EffectGroupPhase {
    match lifecycle {
        EffectGroupLifecycle::Preparing { .. } => EffectGroupPhase::Preparing,
        EffectGroupLifecycle::Ready { .. } => EffectGroupPhase::Ready,
        EffectGroupLifecycle::Closed { .. } => EffectGroupPhase::Closed,
        EffectGroupLifecycle::Retired { .. } => EffectGroupPhase::Retired,
    }
}

async fn load_index(
    ctx: &ObjectContext<'_>,
) -> Result<Option<EffectGroupIndexRecord>, TerminalError> {
    Ok(ctx
        .get::<Json<EffectGroupIndexRecord>>(INDEX_STATE_KEY)
        .await?
        .map(|Json(record)| record))
}

async fn load_index_shared(
    ctx: &SharedObjectContext<'_>,
) -> Result<Option<EffectGroupIndexRecord>, TerminalError> {
    Ok(ctx
        .get::<Json<EffectGroupIndexRecord>>(INDEX_STATE_KEY)
        .await?
        .map(|Json(record)| record))
}

fn store_index(ctx: &ObjectContext<'_>, record: EffectGroupIndexRecord) {
    ctx.set(INDEX_STATE_KEY, Json(record));
}

#[derive(Clone, Copy, Debug)]
pub struct EffectGroupIndex;

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
                shape_digest: record.shape.digest()?,
                phase: phase(&record.lifecycle),
            },
        };
        Ok(Json(response))
    }

    #[handler]
    async fn open(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupOpenRequest>,
    ) -> HandlerResult<Json<EffectGroupOpenResponse>> {
        let Some(record) = load_index(&ctx).await? else {
            store_index(
                &ctx,
                EffectGroupIndexRecord {
                    shape: request.shape,
                    lifecycle: EffectGroupLifecycle::Preparing {
                        dispatch: EffectGroupDispatchState::Unadopted,
                    },
                    next_rank: 1,
                    settlements: BTreeMap::new(),
                    settled_positions: BTreeMap::new(),
                },
            );
            return Ok(Json(EffectGroupOpenResponse::OpenedFresh));
        };
        if record.shape != request.shape {
            return Ok(Json(EffectGroupOpenResponse::ShapeMismatch));
        }
        Ok(Json(match record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => EffectGroupOpenResponse::ReopenedPreparing,
            EffectGroupLifecycle::Ready { .. } => EffectGroupOpenResponse::ReopenedReady,
            EffectGroupLifecycle::Closed { effective, .. } => {
                EffectGroupOpenResponse::ReopenedClosed { effective }
            }
            EffectGroupLifecycle::Retired { .. } => EffectGroupOpenResponse::Retired,
        }))
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
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing { dispatch } => match dispatch {
                EffectGroupDispatchState::Unadopted => {
                    *dispatch = EffectGroupDispatchState::Adopted {
                        id: request.invocation_id,
                        dispatched: BTreeMap::new(),
                    };
                    store_index(&ctx, record);
                    EffectGroupProbeAdoptResponse::Adopted
                }
                EffectGroupDispatchState::Adopted { id, .. } if id == &request.invocation_id => {
                    EffectGroupProbeAdoptResponse::AlreadyAdopted
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
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
            } => match dispatched.get(&request.position) {
                Some(existing) if existing == &request.invocation_id => {
                    EffectGroupRecordDispatchResponse::Duplicate
                }
                Some(_) => EffectGroupRecordDispatchResponse::DispatchMismatch,
                None if request.position >= record.shape.children => {
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
                &record.shape.wait_scope,
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
        let expected_positions = (0..record.shape.children).collect::<Vec<_>>();
        if request.addresses.keys().copied().collect::<Vec<_>>() != expected_positions {
            return Ok(Json(EffectGroupRegisterResponse::RegistrationMismatch));
        }
        let response = match &record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
            } if dispatched == &request.addresses => {
                record.lifecycle = EffectGroupLifecycle::Ready {
                    addresses: request.addresses,
                };
                store_index(&ctx, record.clone());
                resolve_group_wait(
                    &ctx,
                    &record.shape.wait_scope,
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
            EffectGroupLifecycle::Ready { addresses } if addresses == &request.addresses => {
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
        let response = match record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => {
                record.lifecycle = EffectGroupLifecycle::Closed {
                    effective: EffectGroupCloseDisposition::Refused {
                        reason: request.reason.clone(),
                    },
                    addresses: BTreeMap::new(),
                };
                store_index(&ctx, record.clone());
                resolve_group_wait(
                    &ctx,
                    &record.shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Ready,
                    EffectGroupWaitResolution::Refused {
                        reason: request.reason.clone(),
                    },
                )
                .await?;
                for position in 0..record.shape.children {
                    resolve_group_wait(
                        &ctx,
                        &record.shape.wait_scope,
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
        let Some(record) = load_index(&ctx).await? else {
            return Ok(Json(EffectGroupAdmissionResponse::Refused));
        };
        let response = match &record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
            } => match dispatched.get(&request.position) {
                None => EffectGroupAdmissionResponse::NotYetRecorded,
                Some(id) if id == &request.invocation_id => EffectGroupAdmissionResponse::Admitted,
                Some(_) => EffectGroupAdmissionResponse::Refused,
            },
            EffectGroupLifecycle::Ready { addresses } => match addresses.get(&request.position) {
                Some(id) if id == &request.invocation_id => EffectGroupAdmissionResponse::Admitted,
                _ => EffectGroupAdmissionResponse::Refused,
            },
            EffectGroupLifecycle::Closed {
                effective,
                addresses,
            } => match effective {
                EffectGroupCloseDisposition::RunToCompletion => {
                    match addresses.get(&request.position) {
                        Some(id) if id == &request.invocation_id => {
                            EffectGroupAdmissionResponse::Admitted
                        }
                        _ => EffectGroupAdmissionResponse::Refused,
                    }
                }
                EffectGroupCloseDisposition::Cancel
                | EffectGroupCloseDisposition::Refused { .. } => {
                    EffectGroupAdmissionResponse::Refused
                }
            },
            EffectGroupLifecycle::Preparing { .. } => EffectGroupAdmissionResponse::NotYetRecorded,
            EffectGroupLifecycle::Retired { .. } => EffectGroupAdmissionResponse::Retired,
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
        if request.position >= record.shape.children {
            return Ok(Json(EffectGroupRecordSettlementResponse::UnknownChild));
        }
        if let Some(rank) = record.settled_positions.get(&request.position).copied() {
            resolve_group_wait(
                &ctx,
                &record.shape.wait_scope,
                &group_key,
                EffectGroupWaitKind::Rank(rank),
                EffectGroupWaitResolution::Rank,
            )
            .await?;
            return Ok(Json(EffectGroupRecordSettlementResponse::Duplicate {
                rank,
            }));
        }
        let rank = record.next_rank;
        record.next_rank = record.next_rank.checked_add(1).ok_or_else(|| {
            TerminalError::new(format!(
                "effect group {group_key} exhausted settlement ranks"
            ))
        })?;
        let settlement = EffectGroupSettlementRecord {
            position: request.position,
            sequence: rank,
            terminal: request.terminal,
        };
        record.settlements.insert(rank, settlement);
        record.settled_positions.insert(request.position, rank);
        store_index(&ctx, record.clone());
        resolve_group_wait(
            &ctx,
            &record.shape.wait_scope,
            &group_key,
            EffectGroupWaitKind::Rank(rank),
            EffectGroupWaitResolution::Rank,
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
        Ok(Json(match record.settlements.get(&request.rank) {
            Some(settlement) => EffectGroupReadRankResponse::Settled {
                settlement: settlement.clone(),
            },
            None => EffectGroupReadRankResponse::NotSettled,
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
        let (declared, addresses, prior) = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { .. } => {
                return Ok(Json(EffectGroupCloseResponse::NotReady));
            }
            EffectGroupLifecycle::Ready { addresses } => {
                (record.shape.loser_disposition, addresses.clone(), None)
            }
            EffectGroupLifecycle::Closed {
                effective,
                addresses,
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
        if effective == LoserPolicy::Cancel {
            for position in 0..record.shape.children {
                if record.settled_positions.contains_key(&position) {
                    continue;
                }
                let rank = record.next_rank;
                record.next_rank = record.next_rank.checked_add(1).ok_or_else(|| {
                    TerminalError::new(format!(
                        "effect group {group_key} exhausted settlement ranks"
                    ))
                })?;
                record.settlements.insert(
                    rank,
                    EffectGroupSettlementRecord {
                        position,
                        sequence: rank,
                        terminal: EffectGroupSettlementTerminal::Cancelled,
                    },
                );
                record.settled_positions.insert(position, rank);
            }
        }
        record.lifecycle = EffectGroupLifecycle::Closed {
            effective: effective.into(),
            addresses: addresses.clone(),
        };
        store_index(&ctx, record.clone());
        if effective == LoserPolicy::Cancel {
            for position in 0..record.shape.children {
                let rank = record.settled_positions[&position];
                resolve_group_wait(
                    &ctx,
                    &record.shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Rank(rank),
                    EffectGroupWaitResolution::Rank,
                )
                .await?;
                resolve_group_wait(
                    &ctx,
                    &record.shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Cancel(&record.shape.replay_keys[position]),
                    EffectGroupWaitResolution::Cancel,
                )
                .await?;
                resolve_group_wait(
                    &ctx,
                    &record.shape.wait_scope,
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
                EffectGroupCleanup::Pending { facts } => {
                    EffectGroupRetireResponse::AlreadyRetired {
                        cleanup: facts.clone(),
                    }
                }
                EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
            }));
        }
        let (dispatcher, dispatched) = match &record.lifecycle {
            EffectGroupLifecycle::Preparing { dispatch } => {
                let dispatched = match dispatch {
                    EffectGroupDispatchState::Unadopted => BTreeMap::new(),
                    EffectGroupDispatchState::Adopted { dispatched, .. } => dispatched.clone(),
                };
                (dispatch.clone(), dispatched)
            }
            EffectGroupLifecycle::Ready { addresses }
            | EffectGroupLifecycle::Closed { addresses, .. } => {
                // A workflow run is exactly-once per workflow key. Re-sending
                // the completed run recovers its stable invocation id without
                // re-executing it; if a test assembled Ready directly, the
                // probe guard makes the newly-created run exit before sending.
                let group_key = ctx.key().to_string();
                let handle = ctx
                    .workflow_client::<EffectGroupDispatchClient>(group_key.clone())
                    .run(Json(EffectGroupDispatchRequest {
                        group_key,
                        shape: record.shape.clone(),
                        children: Vec::new(),
                    }))
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
            EffectGroupLifecycle::Retired { .. } => unreachable!(),
        };
        let cleanup = EffectGroupCleanupFacts {
            children: record.shape.children,
            replay_keys: record.shape.replay_keys.clone(),
            dispatcher,
            dispatched,
            wait_scope: record.shape.wait_scope.clone(),
        };
        record.lifecycle = EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Pending {
                facts: cleanup.clone(),
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
        let facts = match &record.lifecycle {
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Pending { facts },
            } => facts.clone(),
            EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Complete,
            } => return Ok(Json(EffectGroupRetirementCancelResponse::Tombstone)),
            _ => return Ok(Json(EffectGroupRetirementCancelResponse::NotRetired)),
        };
        let mut changed = false;
        for position in 0..facts.children {
            if record.settled_positions.contains_key(&position) {
                continue;
            }
            let rank = record.next_rank;
            record.next_rank = record.next_rank.checked_add(1).ok_or_else(|| {
                TerminalError::new(format!(
                    "effect group {group_key} exhausted settlement ranks during retirement"
                ))
            })?;
            record.settlements.insert(
                rank,
                EffectGroupSettlementRecord {
                    position,
                    sequence: rank,
                    terminal: EffectGroupSettlementTerminal::Cancelled,
                },
            );
            record.settled_positions.insert(position, rank);
            changed = true;
        }
        store_index(&ctx, record.clone());
        for position in 0..facts.children {
            let rank = record.settled_positions[&position];
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
                EffectGroupWaitKind::Cancel(&facts.replay_keys[position]),
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupPayloadPutResponse {
    Written,
    Duplicate,
    Conflict,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupPayloadGetResponse {
    Stored { bytes: Vec<u8> },
    Missing,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupPayloadPutRequest {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct EffectGroupPayload;

#[restate_sdk::object(name = "EffectGroupPayload")]
impl EffectGroupPayload {
    #[handler]
    async fn put(
        &self,
        ctx: ObjectContext<'_>,
        Json(request): Json<EffectGroupPayloadPutRequest>,
    ) -> HandlerResult<Json<EffectGroupPayloadPutResponse>> {
        if ctx.get::<bool>(PAYLOAD_RETIRED_KEY).await?.unwrap_or(false) {
            return Ok(Json(EffectGroupPayloadPutResponse::Retired));
        }
        let response = match ctx.get::<Vec<u8>>(PAYLOAD_STATE_KEY).await? {
            None => {
                ctx.set(PAYLOAD_STATE_KEY, request.bytes);
                EffectGroupPayloadPutResponse::Written
            }
            Some(existing) if existing == request.bytes => EffectGroupPayloadPutResponse::Duplicate,
            Some(_) => EffectGroupPayloadPutResponse::Conflict,
        };
        Ok(Json(response))
    }

    #[handler]
    async fn get(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> HandlerResult<Json<EffectGroupPayloadGetResponse>> {
        if ctx.get::<bool>(PAYLOAD_RETIRED_KEY).await?.unwrap_or(false) {
            return Ok(Json(EffectGroupPayloadGetResponse::Retired));
        }
        Ok(Json(match ctx.get::<Vec<u8>>(PAYLOAD_STATE_KEY).await? {
            Some(bytes) => EffectGroupPayloadGetResponse::Stored { bytes },
            None => EffectGroupPayloadGetResponse::Missing,
        }))
    }

    #[handler]
    async fn retire(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        ctx.set(PAYLOAD_RETIRED_KEY, true);
        Ok(Json(()))
    }

    #[handler]
    async fn delete_bytes(&self, ctx: ObjectContext<'_>) -> HandlerResult<Json<()>> {
        ctx.clear(PAYLOAD_STATE_KEY);
        Ok(Json(()))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupDispatchRequest {
    pub group_key: String,
    pub shape: EffectGroupShape,
    pub children: Vec<RuntimeEffectEnvelope>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectGroupChildRequest {
    pub group_key: String,
    pub shape: EffectGroupShape,
    pub position: usize,
    pub envelope: RuntimeEffectEnvelope,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EffectGroupChildRunOutcome {
    Completed {
        outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
    },
    Cancelled,
}

#[derive(Clone)]
pub struct EffectGroupDispatch {
    executors: Arc<dyn GroupExecutors>,
    ingress: RestateIngressClient,
    infinite_retry_policy: RunRetryPolicy,
    resolved: Arc<Mutex<HashMap<String, lash_core::RuntimeEffectLocalExecutor<'static>>>>,
}

impl std::fmt::Debug for EffectGroupDispatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EffectGroupDispatch")
            .field("infinite_retry_policy", &self.infinite_retry_policy)
            .finish_non_exhaustive()
    }
}

#[restate_sdk::workflow(name = "EffectGroupDispatch")]
impl EffectGroupDispatch {
    fn cache_executor(&self, child: &RuntimeEffectEnvelope) -> bool {
        let Some(replay_key) = child.invocation.replay_key().map(str::to_string) else {
            return false;
        };
        if self
            .resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&replay_key)
        {
            return true;
        }
        let Some(executor) = self.executors.executor_for(child) else {
            return false;
        };
        self.resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(replay_key, executor);
        true
    }

    #[handler]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(request): Json<EffectGroupDispatchRequest>,
    ) -> HandlerResult<Json<()>> {
        let own_id = ctx.invocation_id().to_string();
        let Json(adopted) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .probe_and_adopt(Json(EffectGroupAdoptRequest {
                invocation_id: own_id,
            }))
            .call()
            .await?;
        match adopted {
            EffectGroupProbeAdoptResponse::Adopted
            | EffectGroupProbeAdoptResponse::AlreadyAdopted => {}
            EffectGroupProbeAdoptResponse::Ready
            | EffectGroupProbeAdoptResponse::Closed
            | EffectGroupProbeAdoptResponse::Retired => return Ok(Json(())),
            EffectGroupProbeAdoptResponse::DifferentDispatcher
            | EffectGroupProbeAdoptResponse::UnknownGroup => {
                return Err(TerminalError::new(format!(
                    "effect-group dispatcher protocol defect for {}: {adopted:?}",
                    request.group_key
                ))
                .into());
            }
        }

        let executors = Arc::clone(&self.executors);
        let resolved = Arc::clone(&self.resolved);
        let preflight_children = request.children.clone();
        let Json(missing) = ctx
            .run(move || async move {
                let missing = preflight_children.iter().position(|child| {
                    let Some(replay_key) = child.invocation.replay_key().map(str::to_string) else {
                        return true;
                    };
                    if resolved
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(&replay_key)
                    {
                        return false;
                    }
                    let Some(executor) = executors.executor_for(child) else {
                        return true;
                    };
                    resolved
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(replay_key, executor);
                    false
                });
                Ok(Json(missing))
            })
            .name("lash:effect-group:dispatch-preflight")
            .retry_policy(self.infinite_retry_policy.clone())
            .await?;
        if let Some(position) = missing {
            let Json(outcome) = ctx
                .object_client::<EffectGroupIndexClient>(request.group_key.clone())
                .register_refusal(Json(EffectGroupRefusalRequest {
                    reason: EffectGroupRefusal::NoExecutor { position },
                }))
                .call()
                .await?;
            return match outcome {
                EffectGroupRegisterRefusalResponse::Refused
                | EffectGroupRegisterRefusalResponse::AlreadyRegistered
                | EffectGroupRegisterRefusalResponse::AlreadyClosed
                | EffectGroupRegisterRefusalResponse::Retired => Ok(Json(())),
                EffectGroupRegisterRefusalResponse::UnknownGroup => {
                    Err(TerminalError::new(format!(
                        "dispatcher refused unknown effect group {}",
                        request.group_key
                    ))
                    .into())
                }
            };
        }

        let mut addresses = BTreeMap::new();
        for (position, envelope) in request.children.into_iter().enumerate() {
            let handle = ctx
                .workflow_client::<EffectGroupDispatchClient>(request.group_key.clone())
                .child(Json(EffectGroupChildRequest {
                    group_key: request.group_key.clone(),
                    shape: request.shape.clone(),
                    position,
                    envelope,
                }))
                .send()
                .await?;
            let invocation_id = handle.invocation_id().to_owned();
            let Json(recorded) = ctx
                .object_client::<EffectGroupIndexClient>(request.group_key.clone())
                .record_dispatch(Json(EffectGroupRecordDispatchRequest {
                    position,
                    invocation_id: invocation_id.clone(),
                }))
                .call()
                .await?;
            match recorded {
                EffectGroupRecordDispatchResponse::Recorded
                | EffectGroupRecordDispatchResponse::Duplicate => {}
                EffectGroupRecordDispatchResponse::Retired => return Ok(Json(())),
                other => {
                    return Err(TerminalError::new(format!(
                        "record dispatch protocol defect for {} child {position}: {other:?}",
                        request.group_key
                    ))
                    .into());
                }
            }
            addresses.insert(position, invocation_id);
        }
        let Json(registered) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .register_children(Json(EffectGroupRegisterRequest { addresses }))
            .call()
            .await?;
        match registered {
            EffectGroupRegisterResponse::Registered
            | EffectGroupRegisterResponse::AlreadyRegistered
            | EffectGroupRegisterResponse::AlreadyClosed
            | EffectGroupRegisterResponse::Retired => Ok(Json(())),
            other => Err(TerminalError::new(format!(
                "register children protocol defect for {}: {other:?}",
                request.group_key
            ))
            .into()),
        }
    }

    #[handler]
    async fn preflight(
        &self,
        _ctx: SharedWorkflowContext<'_>,
        Json(children): Json<Vec<RuntimeEffectEnvelope>>,
    ) -> HandlerResult<Json<Option<usize>>> {
        Ok(Json(
            children
                .iter()
                .position(|child| !self.cache_executor(child)),
        ))
    }

    #[handler]
    async fn child(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(request): Json<EffectGroupChildRequest>,
    ) -> HandlerResult<Json<()>> {
        let own_id = ctx.invocation_id().to_string();
        let admission_request = EffectGroupAdmissionRequest {
            position: request.position,
            invocation_id: own_id,
        };
        let Json(first) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .admit_child(Json(admission_request.clone()))
            .call()
            .await?;
        let admission = match first {
            EffectGroupAdmissionResponse::Admitted => EffectGroupAdmissionResponse::Admitted,
            EffectGroupAdmissionResponse::Refused | EffectGroupAdmissionResponse::Retired => {
                return Ok(Json(()));
            }
            EffectGroupAdmissionResponse::NotYetRecorded => {
                let key = group_wait_key(
                    &request.shape.wait_scope,
                    &request.group_key,
                    EffectGroupWaitKind::Admit(request.position),
                )?;
                let address = RestateDurableWaitAddress::for_key(&key);
                let Json(_) = ctx
                    .workflow_client::<LashDurableWaitWorkflowClient>(address.workflow_key)
                    .await_resolution(Json(RestateDurableWaitAwaitRequest {
                        key,
                        timeout_ms: None,
                    }))
                    .call()
                    .await?;
                // ADMIT is notification only. Authorization always comes from
                // this one fresh, mapping-exact call after the wake.
                let Json(fresh) = ctx
                    .object_client::<EffectGroupIndexClient>(request.group_key.clone())
                    .admit_child(Json(admission_request))
                    .call()
                    .await?;
                fresh
            }
        };
        match admission {
            EffectGroupAdmissionResponse::Admitted => {}
            EffectGroupAdmissionResponse::Refused | EffectGroupAdmissionResponse::Retired => {
                return Ok(Json(()));
            }
            EffectGroupAdmissionResponse::NotYetRecorded => {
                return Err(TerminalError::new(format!(
                    "ADMIT notification for {} child {} did not produce a decisive fresh admission",
                    request.group_key, request.position
                ))
                .into());
            }
        }

        let replay_key =
            request.envelope.invocation.replay_key().ok_or_else(|| {
                TerminalError::new("effect-group child is missing its replay key")
            })?;
        let executor = self
            .resolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(replay_key)
            .or_else(|| self.executors.executor_for(&request.envelope));
        let Some(executor) = executor else {
            return Err(std::io::Error::other(format!(
                "no executor currently routes effect group {} child {}; retry on a carrying deployment",
                request.group_key, request.position
            ))
            .into());
        };
        let cancellation = tokio_util::sync::CancellationToken::new();
        let run_cancellation = cancellation.clone();
        let envelope = request.envelope.clone();
        let mut run = Box::pin(
            ctx.run(move || async move {
                let outcome = tokio::select! {
                    biased;
                    _ = run_cancellation.cancelled() => EffectGroupChildRunOutcome::Cancelled,
                    outcome = executor.execute(envelope) => {
                        EffectGroupChildRunOutcome::Completed { outcome }
                    }
                };
                Ok(Json(outcome))
            })
            .name(format!(
                "lash:effect-group:{}:{}",
                request.group_key, request.position
            ))
            .retry_policy(self.infinite_retry_policy.clone()),
        );
        let cancel_key = group_wait_key(
            &request.shape.wait_scope,
            &request.group_key,
            EffectGroupWaitKind::Cancel(&request.shape.replay_keys[request.position]),
        )?;
        let cancel_address = RestateDurableWaitAddress::for_key(&cancel_key);
        let cancel_request = RestateDurableWaitAwaitRequest {
            key: cancel_key,
            timeout_ms: None,
        };
        let cancel_watch = self.ingress.call_workflow_json::<_, Resolution>(
            "LashDurableWaitWorkflow",
            &cancel_address.workflow_key,
            "await_resolution",
            &cancel_request,
        );
        tokio::pin!(cancel_watch);
        let Json(outcome) = tokio::select! {
            biased;
            cancel = &mut cancel_watch => {
                cancel.map_err(|error| std::io::Error::other(format!(
                    "observe effect-group child cancellation: {error}"
                )))?;
                cancellation.cancel();
                run.await?
            }
            outcome = &mut run => outcome?,
        };

        let terminal = match outcome {
            EffectGroupChildRunOutcome::Cancelled => EffectGroupSettlementTerminal::Cancelled,
            EffectGroupChildRunOutcome::Completed {
                outcome: Err(error),
            } => EffectGroupSettlementTerminal::Failed { error },
            EffectGroupChildRunOutcome::Completed {
                outcome: Ok(outcome),
            } => {
                let bytes = serde_json::to_vec(&outcome).map_err(|error| {
                    TerminalError::new(format!(
                        "serialize effect group {} child {} outcome: {error}",
                        request.group_key, request.position
                    ))
                })?;
                let Json(put) = ctx
                    .object_client::<EffectGroupPayloadClient>(payload_key(
                        &request.group_key,
                        request.position,
                    ))
                    .put(Json(EffectGroupPayloadPutRequest { bytes }))
                    .call()
                    .await?;
                match put {
                    EffectGroupPayloadPutResponse::Written
                    | EffectGroupPayloadPutResponse::Duplicate => {
                        EffectGroupSettlementTerminal::StoredPayload
                    }
                    EffectGroupPayloadPutResponse::Retired => return Ok(Json(())),
                    EffectGroupPayloadPutResponse::Conflict => {
                        return Err(TerminalError::new(format!(
                            "payload byte fence conflict for effect group {} child {}",
                            request.group_key, request.position
                        ))
                        .into());
                    }
                }
            }
        };
        let Json(recorded) = ctx
            .object_client::<EffectGroupIndexClient>(request.group_key.clone())
            .record_settlement(Json(EffectGroupRecordSettlementRequest {
                position: request.position,
                terminal,
            }))
            .call()
            .await?;
        match recorded {
            EffectGroupRecordSettlementResponse::Recorded { .. }
            | EffectGroupRecordSettlementResponse::Duplicate { .. }
            | EffectGroupRecordSettlementResponse::Retired => Ok(Json(())),
            other => Err(TerminalError::new(format!(
                "record settlement protocol defect for {} child {}: {other:?}",
                request.group_key, request.position
            ))
            .into()),
        }
    }

    #[handler]
    async fn retire(
        &self,
        ctx: SharedWorkflowContext<'_>,
        group_key: String,
    ) -> HandlerResult<Json<()>> {
        let Json(retired) = ctx
            .object_client::<EffectGroupIndexClient>(group_key.clone())
            .retire()
            .call()
            .await?;
        let cleanup = match retired {
            EffectGroupRetireResponse::Retired { cleanup }
            | EffectGroupRetireResponse::AlreadyRetired { cleanup } => cleanup,
            EffectGroupRetireResponse::Tombstone | EffectGroupRetireResponse::UnknownGroup => {
                return Ok(Json(()));
            }
        };
        if let EffectGroupDispatchState::Adopted { id, .. } = &cleanup.dispatcher {
            ctx.invocation_handle(id.clone()).cancel();
            match ctx.invocation_handle(id.clone()).attach::<Json<()>>().await {
                Ok(_) | Err(_) => {}
            }
        }
        for invocation_id in cleanup.dispatched.values() {
            ctx.invocation_handle(invocation_id.clone()).cancel();
        }
        let Json(cancelled) = ctx
            .object_client::<EffectGroupIndexClient>(group_key.clone())
            .retirement_cancel()
            .call()
            .await?;
        match cancelled {
            EffectGroupRetirementCancelResponse::Applied
            | EffectGroupRetirementCancelResponse::AlreadyApplied => {}
            other => {
                return Err(TerminalError::new(format!(
                    "effect group {group_key} retirement could not install canceller-side terminals: {other:?}"
                ))
                .into());
            }
        }
        for position in 0..cleanup.children {
            let Json(()) = ctx
                .object_client::<EffectGroupPayloadClient>(payload_key(&group_key, position))
                .retire()
                .call()
                .await?;
        }
        // Wait retirement is retained in the durable-wait index. This shared
        // handler cannot mutate the index object directly without journaling
        // the calls, so resolve every one before deleting payload bytes.
        for (kind, resolution) in std::iter::once((
            EffectGroupWaitKind::Ready,
            EffectGroupWaitResolution::Retired,
        ))
        .chain((1..=cleanup.children as u64).map(|rank| {
            (
                EffectGroupWaitKind::Rank(rank),
                EffectGroupWaitResolution::Retired,
            )
        }))
        .chain(cleanup.replay_keys.iter().map(|replay_key| {
            (
                EffectGroupWaitKind::Cancel(replay_key),
                EffectGroupWaitResolution::Retired,
            )
        }))
        .chain((0..cleanup.children).map(|position| {
            (
                EffectGroupWaitKind::Admit(position),
                EffectGroupWaitResolution::Retired,
            )
        })) {
            let key = group_wait_key(&cleanup.wait_scope, &group_key, kind)?;
            let address = RestateDurableWaitAddress::for_key(&key);
            let Json(()) = ctx
                .object_client::<LashDurableWaitIndexClient>(durable_wait_index_object_key(
                    &address,
                ))
                .retain_resolution(Json(RestateDurableWaitResolveRequest {
                    key,
                    resolution: wait_resolution(resolution)?,
                }))
                .call()
                .await?;
        }
        for position in 0..cleanup.children {
            let Json(()) = ctx
                .object_client::<EffectGroupPayloadClient>(payload_key(&group_key, position))
                .delete_bytes()
                .call()
                .await?;
        }
        let Json(finished) = ctx
            .object_client::<EffectGroupIndexClient>(group_key.clone())
            .finish_retirement()
            .call()
            .await?;
        match finished {
            EffectGroupFinishRetirementResponse::Finished
            | EffectGroupFinishRetirementResponse::AlreadyFinished => Ok(Json(())),
            other => Err(TerminalError::new(format!(
                "effect group {group_key} retirement could not reduce the index to its tombstone: {other:?}"
            ))
            .into()),
        }
    }
}

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
