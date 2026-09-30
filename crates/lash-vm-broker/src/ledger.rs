//! The parent's ledger of one run, and the checkpoint that commits it with
//! the VM state it matches.
//!
//! The parent issues every journal ordinal: a request it admits takes the
//! next one, and every identity the request's calls take derives from it
//! (ADR 0117). A worker cannot choose, skip or repeat one. The ledger also
//! holds the handles the parent granted.
//!
//! A [`Checkpoint`] is the unit of durability: VM bytes and the ledger that
//! matches them, committed together or not at all. The ledger only ever
//! reaches a store inside a checkpoint, so no store holds counters advanced
//! past the VM state they belong to. A run is started from a committed
//! checkpoint or from the start; its ledger is never rewound in place (the
//! VM and its counters are rebuilt only by replaying the journal).

use std::collections::BTreeMap;

use lash_vm_protocol::{FrameEpoch, OpaqueVmState};
use serde::{Deserialize, Serialize};

use crate::authority::{
    AdmittedContext, HandleGrant, RequestFingerprint, ResolvedCall, ResolvedRequest,
};

/// The ledger's durable form.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerSnapshot {
    /// The ordinal the run's next admitted command takes.
    pub next_ordinal: u64,
    /// The handles the parent granted, by handle.
    pub grants: BTreeMap<String, HandleGrant>,
}

/// One admitted call: its parent-derived identity and where it routes.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedCall {
    pub call_id: lash_sansio::ToolCallId,
    pub call: ResolvedCall,
}

/// What an admitted request does.
#[derive(Clone, Debug, PartialEq)]
pub enum AdmittedKind {
    Invoke(AdmittedCall),
    /// Every leaf of an aggregate, at its first-appearance index.
    Aggregate(Vec<AdmittedCall>),
    Await {
        handle: String,
        grant: HandleGrant,
    },
    Sleep {
        millis: u64,
    },
    Control {
        kind: lash_vm_protocol::EffectKind,
        payload: lash_vm_protocol::EncodedPayload,
    },
}

/// A request the parent admitted: its ordinal, the fingerprint of what it
/// asked, and the calls it resolved to under their identities.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedOperation {
    pub ordinal: u64,
    pub fingerprint: RequestFingerprint,
    pub kind: AdmittedKind,
    pub request: Option<lash_vm_protocol::EncodedPayload>,
}

impl AdmittedOperation {
    /// Every call id the operation's calls take: one per logical call.
    pub fn call_ids(&self) -> Vec<lash_sansio::ToolCallId> {
        match &self.kind {
            AdmittedKind::Invoke(call) => vec![call.call_id.clone()],
            AdmittedKind::Aggregate(calls) => {
                calls.iter().map(|call| call.call_id.clone()).collect()
            }
            AdmittedKind::Await { grant, .. } => vec![grant.call_id.clone()],
            AdmittedKind::Sleep { .. } | AdmittedKind::Control { .. } => Vec::new(),
        }
    }

    /// The identity the operation is retained and addressed under: its call,
    /// an aggregate's own command id, or the command id of a wait.
    pub fn command_id(&self, context: &AdmittedContext) -> lash_sansio::ToolCallId {
        context.identities.call_id(self.ordinal)
    }
}

/// One run's ledger, live.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParentLedger {
    state: LedgerSnapshot,
}

impl ParentLedger {
    /// A run's ledger from its start, or from the ledger its committed
    /// checkpoint carried.
    pub fn restore(snapshot: LedgerSnapshot) -> Self {
        Self { state: snapshot }
    }

    pub fn snapshot(&self) -> LedgerSnapshot {
        self.state.clone()
    }

    pub fn next_ordinal(&self) -> u64 {
        self.state.next_ordinal
    }

    pub fn grants(&self) -> &BTreeMap<String, HandleGrant> {
        &self.state.grants
    }

    /// Admits a resolved request under the run's next ordinal, deriving its
    /// calls' identities.
    pub fn admit(
        &mut self,
        context: &AdmittedContext,
        request: ResolvedRequest,
    ) -> AdmittedOperation {
        let ordinal = self.state.next_ordinal;
        self.state.next_ordinal += 1;
        let fingerprint = RequestFingerprint::of(&request);
        let kind = match request {
            ResolvedRequest::Invoke(call) => AdmittedKind::Invoke(AdmittedCall {
                call_id: context.identities.call_id(ordinal),
                call,
            }),
            ResolvedRequest::Aggregate(calls) => AdmittedKind::Aggregate(
                calls
                    .into_iter()
                    .zip(0_u64..)
                    .map(|(call, leaf)| AdmittedCall {
                        call_id: context.identities.child_call_id(ordinal, leaf),
                        call,
                    })
                    .collect(),
            ),
            ResolvedRequest::Await { handle, grant } => AdmittedKind::Await { handle, grant },
            ResolvedRequest::Sleep { millis } => AdmittedKind::Sleep { millis },
            ResolvedRequest::Control { kind, payload } => AdmittedKind::Control { kind, payload },
        };
        AdmittedOperation {
            ordinal,
            fingerprint,
            kind,
            request: None,
        }
    }

    /// Records a handle an admitted call's outcome granted, scoped to the
    /// frame it was granted in.
    pub fn grant(
        &mut self,
        handle: String,
        call: &AdmittedCall,
        ordinal: u64,
        frame_epoch: FrameEpoch,
    ) {
        self.state.grants.insert(
            handle,
            HandleGrant {
                ordinal,
                call_id: call.call_id.clone(),
                frame_epoch,
            },
        );
    }
}

/// VM bytes and the ledger that matches them: committed together or not at
/// all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub vm: OpaqueVmState,
    pub ledger: LedgerSnapshot,
    /// The frame the state belongs to.
    pub frame_epoch: FrameEpoch,
}

/// Why a store refused a checkpoint.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the checkpoint was not committed: {0}")]
pub struct CheckpointRefusal(pub String);

/// Where an owner's checkpoints are committed: a durable process's segment
/// handover, a session's worker envelope.
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Commits `checkpoint` atomically, replacing the owner's last one.
    async fn commit(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointRefusal>;

    /// The owner's last committed checkpoint, if any.
    async fn latest(&self) -> Result<Option<Checkpoint>, CheckpointRefusal>;

    /// Opens frame `frame_epoch` (F5): atomically drops the checkpoint of
    /// every earlier frame, so nothing an earlier frame left is restored
    /// into the new one.
    async fn open_frame(&self, frame_epoch: FrameEpoch) -> Result<(), CheckpointRefusal>;
}
