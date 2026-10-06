//! The parent's ledger of one VM execution, and the checkpoint that commits
//! it with the VM state it matches.
//!
//! The parent issues every operation: a request it admits takes the
//! execution's next admission, and every identity the request's calls take
//! derives from it (ADR 0117). A worker cannot choose, skip or repeat one.
//! The operation's [`OperationId`] is minted when its admission commits, and
//! is stored in the snapshot that commits with it (ADR 0132 §8). It is never
//! a journal position. The ledger also holds the handles the parent granted.
//!
//! A [`Checkpoint`] is the unit of durability: VM bytes, the ledger that
//! matches them and the host's own state for the execution, committed
//! together or not at all. The ledger only ever reaches a store inside a
//! checkpoint, so no store holds counters advanced past the VM state they
//! belong to. A run starts from its latest committed checkpoint or from the
//! start; nothing rebuilds a VM by running code again against what an
//! earlier run recorded.

use std::collections::BTreeMap;

use lash_vm_protocol::{EncodedPayload, FrameEpoch, OpaqueVmState};
use serde::{Deserialize, Serialize};

use crate::authority::{
    AdmittedContext, HandleGrant, RequestFingerprint, ResolvedCall, ResolvedRequest,
};
use crate::snapshot::{BrokerLedger, OperationId, PendingOperation};

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

/// A request the parent issued: the admission it takes, the fingerprint of
/// what it asked, the calls it resolved to under their identities, and, once
/// its admission committed, its identity.
#[derive(Clone, Debug, PartialEq)]
pub struct AdmittedOperation {
    /// The execution's admission this operation takes.
    pub run: u64,
    pub fingerprint: RequestFingerprint,
    pub kind: AdmittedKind,
    pub request: Option<lash_vm_protocol::EncodedPayload>,
    /// Its identity, minted when its admission committed; `None` for an
    /// operation the host admits as no execution (a wait it performs again).
    pub operation: Option<OperationId>,
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

    /// The identity the operation is admitted and addressed under: its call,
    /// an aggregate's own command id, or the command id of a wait.
    pub fn command_id(&self, context: &AdmittedContext) -> lash_sansio::ToolCallId {
        context.identities.call_id(self.run)
    }
}

/// One execution's ledger, live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentLedger {
    state: BrokerLedger,
}

impl ParentLedger {
    /// A fresh execution's ledger, in `frame_epoch`.
    pub fn start(frame_epoch: FrameEpoch) -> Self {
        Self {
            state: BrokerLedger {
                operations: BTreeMap::new(),
                next_admission: 0,
                frame_epoch,
                grants: BTreeMap::new(),
                pending: None,
            },
        }
    }

    /// The ledger its committed checkpoint carried.
    pub fn restore(ledger: BrokerLedger) -> Self {
        Self { state: ledger }
    }

    pub fn snapshot(&self) -> BrokerLedger {
        self.state.clone()
    }

    pub fn grants(&self) -> &BTreeMap<String, HandleGrant> {
        &self.state.grants
    }

    /// The operation the VM stands on, if it stands on one.
    pub fn pending(&self) -> Option<&PendingOperation> {
        self.state.pending.as_ref()
    }

    /// Issues `resolved`, resolved from `request`, as the execution's next
    /// admission, deriving its calls' identities. The VM stands on it until
    /// it is answered.
    pub fn issue(
        &mut self,
        context: &AdmittedContext,
        resolved: ResolvedRequest,
        request: &lash_vm_protocol::EffectRequest,
    ) -> AdmittedOperation {
        let run = self.state.next_admission;
        self.state.next_admission += 1;
        let operation = admitted(context, run, resolved, None);
        self.state.pending = Some(PendingOperation {
            run,
            kind: request.kind,
            request: request.payload.clone(),
            fingerprint: operation.fingerprint,
            operation: None,
            waits: Vec::new(),
        });
        AdmittedOperation {
            request: Some(request.payload.clone()),
            ..operation
        }
    }

    /// The operation a restored VM stands on, resolved again from the
    /// request `pending` kept: the same admission and identity.
    pub fn reissue(
        &self,
        context: &AdmittedContext,
        resolved: ResolvedRequest,
        pending: &PendingOperation,
    ) -> AdmittedOperation {
        AdmittedOperation {
            request: Some(pending.request.clone()),
            ..admitted(context, pending.run, resolved, pending.operation)
        }
    }

    /// The VM was answered: it no longer stands on an operation, and an
    /// identity no grant names is no longer reachable from any snapshot.
    pub fn answered(&mut self) {
        self.state.pending = None;
        let granted: std::collections::BTreeSet<u64> =
            self.state.grants.values().map(|grant| grant.run).collect();
        self.state
            .operations
            .retain(|operation, _| granted.contains(&operation.run));
    }

    /// Records a handle an admitted call's outcome granted, scoped to the
    /// frame it was granted in.
    pub fn grant(
        &mut self,
        handle: String,
        call: &AdmittedCall,
        run: u64,
        frame_epoch: FrameEpoch,
    ) {
        self.state.grants.insert(
            handle,
            HandleGrant {
                run,
                call_id: call.call_id.clone(),
                frame_epoch,
            },
        );
    }
}

/// `resolved` as admission `run`, its calls' identities derived from it.
fn admitted(
    context: &AdmittedContext,
    run: u64,
    resolved: ResolvedRequest,
    operation: Option<OperationId>,
) -> AdmittedOperation {
    let fingerprint = RequestFingerprint::of(&resolved);
    let kind = match resolved {
        ResolvedRequest::Invoke(call) => AdmittedKind::Invoke(AdmittedCall {
            call_id: context.identities.call_id(run),
            call,
        }),
        ResolvedRequest::Aggregate(calls) => AdmittedKind::Aggregate(
            calls
                .into_iter()
                .zip(0_u64..)
                .map(|(call, leaf)| AdmittedCall {
                    call_id: context.identities.child_call_id(run, leaf),
                    call,
                })
                .collect(),
        ),
        ResolvedRequest::Await { handle, grant } => AdmittedKind::Await { handle, grant },
        ResolvedRequest::Sleep { millis } => AdmittedKind::Sleep { millis },
        ResolvedRequest::Control { kind, payload } => AdmittedKind::Control { kind, payload },
    };
    AdmittedOperation {
        run,
        fingerprint,
        kind,
        request: None,
        operation,
    }
}

/// How a run ended, as its last checkpoint records it: a restore answers it
/// without starting the VM.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedEnd {
    /// The program finished with `value`.
    Complete { value: EncodedPayload },
    /// The guest failed with `error`.
    GuestError { error: EncodedPayload },
}

/// VM bytes, the ledger that matches them and the host's state for the
/// execution: committed together or not at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub vm: OpaqueVmState,
    pub ledger: BrokerLedger,
    /// The host's own state for the execution at this quiet point (a cell's
    /// envelope: its prints, the calls it counted, what it linked against),
    /// opaque to the broker.
    pub host: Option<EncodedPayload>,
    /// How the run ended, once it ended.
    pub end: Option<RecordedEnd>,
}

impl Checkpoint {
    /// The frame the state belongs to.
    pub fn frame_epoch(&self) -> FrameEpoch {
        self.ledger.frame_epoch
    }
}

/// Why a store refused a quiet point.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the quiet point was not committed: {0}")]
pub struct QuietPointRefusal(pub String);
