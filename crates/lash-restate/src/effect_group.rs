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

#[cfg(test)]
static ADMISSION_WITNESSES: std::sync::OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>> =
    std::sync::OnceLock::new();

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

    /// The invariant `from_group` establishes, re-checked on the way in.
    ///
    /// `children` and `replay_keys` are two public fields of a public type, so
    /// a shape that arrives over the wire carries whatever the caller put in
    /// it. Every later reader pairs a position drawn from `children` with the
    /// replay key at that position; a shape whose two halves disagree is a
    /// caller defect that can never become valid by retrying, so it is refused
    /// once, terminally, at the boundary rather than surviving into stored
    /// state where a later handler would meet it.
    pub(crate) fn validate_wire(&self) -> Result<(), TerminalError> {
        if self.children != self.replay_keys.len() {
            return Err(TerminalError::new(format!(
                "effect-group shape declares {} children but carries {} replay keys",
                self.children,
                self.replay_keys.len()
            )));
        }
        Ok(())
    }

    /// The replay key of a child position, as a terminal error when the shape
    /// does not have one.
    pub(crate) fn replay_key(&self, position: usize) -> Result<&str, TerminalError> {
        self.replay_keys
            .get(position)
            .map(String::as_str)
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "effect-group shape has no replay key for child {position} of {}",
                    self.children
                ))
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

impl EffectGroupCleanupFacts {
    /// The replay key of a child position, as a terminal error when the
    /// retirement facts do not have one. Same pairing, same independent public
    /// fields, and the same refusal as [`EffectGroupShape::replay_key`].
    pub(crate) fn replay_key(&self, position: usize) -> Result<&str, TerminalError> {
        self.replay_keys
            .get(position)
            .map(String::as_str)
            .ok_or_else(|| {
                TerminalError::new(format!(
                    "effect-group retirement facts have no replay key for child {position} of {}",
                    self.children
                ))
            })
    }
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
struct EffectGroupIndexLiveRecord {
    shape: EffectGroupShape,
    next_rank: u64,
    #[serde(with = "btree_map_as_pairs")]
    settlements: BTreeMap<u64, EffectGroupSettlementRecord>,
    #[serde(with = "btree_map_as_pairs")]
    settled_positions: BTreeMap<usize, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EffectGroupIndexRecord {
    shape_digest: String,
    lifecycle: EffectGroupLifecycle,
    #[serde(skip_serializing_if = "Option::is_none")]
    live: Option<EffectGroupIndexLiveRecord>,
}

impl EffectGroupIndexRecord {
    fn live(&self) -> Result<&EffectGroupIndexLiveRecord, TerminalError> {
        self.live.as_ref().ok_or_else(|| {
            TerminalError::new(format!(
                "effect-group index {} has no live state before completed retirement",
                self.shape_digest
            ))
        })
    }

    fn live_mut(&mut self) -> Result<&mut EffectGroupIndexLiveRecord, TerminalError> {
        self.live.as_mut().ok_or_else(|| {
            TerminalError::new(format!(
                "effect-group index {} has no live state before completed retirement",
                self.shape_digest
            ))
        })
    }
}

#[cfg(test)]
#[test]
fn completed_retirement_index_serializes_as_tombstone_only() {
    let record = EffectGroupIndexRecord {
        shape_digest: "shape-digest".to_owned(),
        lifecycle: EffectGroupLifecycle::Retired {
            cleanup: EffectGroupCleanup::Complete,
        },
        live: None,
    };

    assert_eq!(
        serde_json::to_value(record).expect("serialize completed retirement tombstone"),
        serde_json::json!({
            "shape_digest": "shape-digest",
            "lifecycle": {
                "type": "retired",
                "cleanup": { "type": "complete" }
            }
        })
    );
}

#[cfg(test)]
#[test]
fn missing_live_index_state_is_a_typed_terminal_error_before_retirement() {
    let record = EffectGroupIndexRecord {
        shape_digest: "shape-digest".to_owned(),
        lifecycle: EffectGroupLifecycle::Preparing {
            dispatch: EffectGroupDispatchState::Unadopted,
        },
        live: None,
    };

    let error = record
        .live()
        .expect_err("missing live state must be rejected without panicking");
    assert!(
        error
            .to_string()
            .contains("has no live state before completed retirement")
    );
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
                shape_digest: record.shape_digest,
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
        request.shape.validate_wire()?;
        let Some(record) = load_index(&ctx).await? else {
            let shape_digest = request.shape.digest()?;
            store_index(
                &ctx,
                EffectGroupIndexRecord {
                    shape_digest,
                    lifecycle: EffectGroupLifecycle::Preparing {
                        dispatch: EffectGroupDispatchState::Unadopted,
                    },
                    live: Some(EffectGroupIndexLiveRecord {
                        shape: request.shape,
                        next_rank: 1,
                        settlements: BTreeMap::new(),
                        settled_positions: BTreeMap::new(),
                    }),
                },
            );
            return Ok(Json(EffectGroupOpenResponse::OpenedFresh));
        };
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupOpenResponse::Retired));
        }
        if record.live()?.shape != request.shape {
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
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupRecordDispatchResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
        let children = shape.children;
        let response = match &mut record.lifecycle {
            EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Adopted { dispatched, .. },
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
        let expected_positions = (0..shape.children).collect::<Vec<_>>();
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
        if matches!(record.lifecycle, EffectGroupLifecycle::Retired { .. }) {
            return Ok(Json(EffectGroupRegisterRefusalResponse::Retired));
        }
        let shape = record.live()?.shape.clone();
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
                    &shape.wait_scope,
                    &group_key,
                    EffectGroupWaitKind::Ready,
                    EffectGroupWaitResolution::Refused {
                        reason: request.reason.clone(),
                    },
                )
                .await?;
                for position in 0..shape.children {
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
        #[cfg(test)]
        if response == EffectGroupAdmissionResponse::NotYetRecorded {
            notify_admission_witness(&group_key);
        }
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
        if request.position >= live.shape.children {
            return Ok(Json(EffectGroupRecordSettlementResponse::UnknownChild));
        }
        if let Some(rank) = live.settled_positions.get(&request.position).copied() {
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
        store_index(&ctx, record.clone());
        resolve_group_wait(
            &ctx,
            &wait_scope,
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
        let settlement = record.live()?.settlements.get(&request.rank).cloned();
        if let Some(settlement) = settlement {
            return Ok(Json(EffectGroupReadRankResponse::Settled { settlement }));
        }
        Ok(Json(match record.lifecycle {
            EffectGroupLifecycle::Closed { .. } => EffectGroupReadRankResponse::Closed,
            _ => EffectGroupReadRankResponse::NotSettled,
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
            EffectGroupLifecycle::Ready { addresses } => {
                (shape.loser_disposition, addresses.clone(), None)
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
            let live = record.live_mut()?;
            for position in 0..live.shape.children {
                if live.settled_positions.contains_key(&position) {
                    continue;
                }
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
        record.lifecycle = EffectGroupLifecycle::Closed {
            effective: effective.into(),
            addresses: addresses.clone(),
        };
        store_index(&ctx, record.clone());
        if effective == LoserPolicy::Cancel {
            for position in 0..shape.children {
                let rank = record
                    .live()?
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
                EffectGroupCleanup::Pending { facts } => {
                    EffectGroupRetireResponse::AlreadyRetired {
                        cleanup: facts.clone(),
                    }
                }
                EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
            }));
        }
        let shape = record.live()?.shape.clone();
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
                        shape: shape.clone(),
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
            EffectGroupLifecycle::Retired { cleanup } => {
                return Ok(Json(match cleanup {
                    EffectGroupCleanup::Pending { facts } => {
                        EffectGroupRetireResponse::AlreadyRetired {
                            cleanup: facts.clone(),
                        }
                    }
                    EffectGroupCleanup::Complete => EffectGroupRetireResponse::Tombstone,
                }));
            }
        };
        let cleanup = EffectGroupCleanupFacts {
            children: shape.children,
            replay_keys: shape.replay_keys,
            dispatcher,
            dispatched,
            wait_scope: shape.wait_scope,
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
                record.live = None;
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
        let live = record.live_mut()?;
        for position in 0..facts.children {
            if live.settled_positions.contains_key(&position) {
                continue;
            }
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
        store_index(&ctx, record.clone());
        for position in 0..facts.children {
            let rank = record
                .live()?
                .settled_positions
                .get(&position)
                .copied()
                .ok_or_else(|| {
                    TerminalError::new(format!(
                        "effect group {group_key} has no retirement settlement rank for child {position}"
                    ))
                })?;
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

include!("effect_group/dispatch.rs");
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
