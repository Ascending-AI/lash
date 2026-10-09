//! One kernel run, driven to its end or to a park that outlives this
//! activation.
//!
//! The machine parks; the broker commits. Each time [`Machine::run`] parks
//! with new requests, the broker exports the state and commits it with the
//! admission of every one of them ([`DurableSnapshotStore::commit_park`]).
//! It then delivers every committed outcome the ledger stands on, in
//! whatever order they are read, and runs the machine again. A park that
//! requests nothing saves nothing: a save is owed only before effects are
//! admitted.
//!
//! A run that has a checkpoint resumes from it: the state is imported, and
//! the first delivery answers, from their records, every effect whose
//! outcome committed and the saved state had not consumed. Whatever ran
//! after that save runs again; no committed effect does.

use lash_durable::DurableInstant;
use lash_kernel_doc::{DocumentId, EncodeError, ErrorDatum};
use lash_kernel_vm::{
    Bound, BoundExceeded, Bounds, DeliverError, EffectRequest, End, ExportError, Host, ImportError,
    Machine, MachineError, Program, Request, RunError, Start, StartError, Step,
};
use lash_vm_protocol::EncodedPayload;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::ledger::{EffectLedger, RecordedEnd};
use super::store::{AdmitAs, EffectAdmission, EndSave, ParkSave};
use crate::effects::{MemberDraft, ParentFault};
use crate::identity::CodeCallIdentities;
use crate::ledger::QuietPointRefusal;
use crate::members::Driven;
use crate::snapshot::DurableSnapshotStore;

/// The parent's half of a kernel run: how each effect is admitted.
#[async_trait::async_trait]
pub trait KernelEffects: Send + Sync {
    /// The execution `request` is admitted as, under `call`, the identity
    /// the parent derived for it: its tool, request, policy, limit and
    /// completion wait. `Err` refuses the effect: the `perform` raises that
    /// error, which the guest may catch, and nothing is dispatched.
    async fn admit(
        &self,
        request: &EffectRequest,
        call: lash_sansio::ToolCallId,
    ) -> Result<Result<MemberDraft, ErrorDatum>, ParentFault>;

    /// The host's own state for the run at this save, opaque to the broker.
    fn host_state(&self) -> Result<Option<EncodedPayload>, ParentFault> {
        Ok(None)
    }
}

/// The most a run's bounds may allow: the broker's own ceilings on what one
/// park admits and one list `join` holds, enforced here as well as in the
/// machine (`K-BND-001`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelCeilings {
    pub requests_per_park: u32,
    pub join_members: u32,
}

impl KernelCeilings {
    /// Refuses `bounds` that allow more than the broker does.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::OverCeiling`], naming the bound.
    pub fn check(&self, bounds: &Bounds) -> Result<(), KernelFailure> {
        for (bound, stated, ceiling) in [
            (
                Bound::RequestsPerPark,
                bounds.requests_per_park,
                self.requests_per_park,
            ),
            (Bound::JoinMembers, bounds.join_members, self.join_members),
        ] {
            if stated > ceiling {
                return Err(KernelFailure::OverCeiling {
                    bound,
                    stated,
                    ceiling,
                });
            }
        }
        Ok(())
    }
}

/// The bound a park of `requests` waits passes, if it passes one: the
/// broker admits none of them, and the run ends there (`K-BND-001`).
pub fn park_bound(bounds: &Bounds, requests: usize) -> Option<BoundExceeded> {
    (requests > bounds.requests_per_park as usize).then(|| BoundExceeded {
        bound: Bound::RequestsPerPark,
        limit: u64::from(bounds.requests_per_park),
    })
}

/// How a driven run stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum KernelEnd {
    /// The run ended. Its end is committed, unless it was cancelled or
    /// never parked.
    Ended(End),
    /// The run is parked on waits that outlive this activation; its
    /// checkpoint resumes it.
    Suspended,
}

/// Why a run did not stop where [`KernelEnd`] says. Every variant is typed.
#[derive(Debug, thiserror::Error)]
pub enum KernelFailure {
    #[error("the run's {bound:?} bound of {stated} is over the broker's ceiling of {ceiling}")]
    OverCeiling {
        bound: Bound,
        stated: u32,
        ceiling: u32,
    },
    #[error("the document has no identity: {0}")]
    Document(#[from] EncodeError),
    #[error(transparent)]
    Start(#[from] StartError),
    #[error("the run's saved state is refused: {0}")]
    Import(#[from] ImportError),
    #[error("the saved checkpoint records neither a state nor an end")]
    EmptyCheckpoint,
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error("the machine refused a committed outcome: {0}")]
    Deliver(#[from] DeliverError),
    #[error("the machine parked on a wait the broker does not hold")]
    Stalled,
    #[error(transparent)]
    Checkpoint(#[from] QuietPointRefusal),
    #[error(transparent)]
    Parent(#[from] ParentFault),
}

/// One owner's broker for kernel runs.
pub struct KernelBroker<'a> {
    pub store: &'a DurableSnapshotStore,
    pub effects: &'a dyn KernelEffects,
    /// The one derivation of the run's call identities (ADR 0117).
    pub identities: &'a CodeCallIdentities,
    pub ceilings: KernelCeilings,
    /// The charge units one [`Machine::run`] call may spend before it
    /// returns to the broker.
    pub slice: u64,
}

impl KernelBroker<'_> {
    /// Runs `program` to its end, or to a park that outlives this
    /// activation, resuming from the run's checkpoint when it has one.
    /// `host` answers the machine's reads; `cancel` cancels the run's open
    /// effects.
    ///
    /// # Errors
    ///
    /// [`KernelFailure`]. The run's last checkpoint stands.
    pub async fn run<M>(
        &self,
        program: Program,
        bounds: Bounds,
        start: Start,
        host: &mut (dyn Host + Send),
        cancel: &CancellationToken,
    ) -> Result<KernelEnd, KernelFailure>
    where
        M: Machine + Send,
        M::Parked: Serialize + DeserializeOwned + Send,
    {
        self.ceilings.check(&bounds)?;
        let document = program.document.identity()?;
        // A run with a checkpoint commits its end over it; one that never
        // parked commits nothing, and runs again from its start.
        let mut saved = false;
        let (mut machine, mut ledger) = match self.store.latest_park::<M::Parked>().await? {
            Some((_, checkpoint)) => {
                saved = true;
                if let Some(end) = checkpoint.end {
                    return Ok(KernelEnd::Ended(end.into_end()));
                }
                let state = checkpoint.state.ok_or(KernelFailure::EmptyCheckpoint)?;
                (M::import(program, bounds, state)?, checkpoint.ledger)
            }
            None => (M::start(program, bounds, start)?, EffectLedger::new()),
        };
        loop {
            let park = match machine.run(host, self.slice)? {
                Step::Slice => continue,
                Step::Ended(end) => return self.ended(&document, ledger, end, saved).await,
                Step::Parked(park) => park,
            };
            let requests: Vec<Request> = park
                .requests
                .into_iter()
                .filter(|request| !park.withdrawn.contains(&wait_of(request)))
                .collect();
            for wait in park.withdrawn {
                ledger.withdraw(wait);
            }
            if let Some(exceeded) = park_bound(&bounds, requests.len()) {
                let end = End::Error(RunError::Bound(exceeded));
                return self.ended(&document, ledger, end, saved).await;
            }
            if !requests.is_empty() {
                let admit = self.admissions(&ledger, requests).await?;
                let committed = self
                    .store
                    .commit_park(ParkSave {
                        document,
                        state: machine.export()?,
                        ledger,
                        admit,
                        host: self.effects.host_state()?,
                        with: Vec::new(),
                    })
                    .await?;
                saved = true;
                ledger = committed.checkpoint.ledger;
            }
            if ledger.pending().next().is_none() {
                return Err(KernelFailure::Stalled);
            }
            let settled = match self.store.drive_effects(&ledger, cancel).await? {
                Driven::Answered(settled) => settled,
                Driven::Suspended => return Ok(KernelEnd::Suspended),
            };
            for outcome in settled {
                machine.deliver(outcome.wait, outcome.outcome)?;
                ledger.consume(&outcome.identity);
            }
        }
    }

    /// How each of a park's requests is admitted: an effect as the
    /// execution the parent drafts for it under the call the park and its
    /// place there derive, a sleep with its deadline on the store's clock.
    async fn admissions(
        &self,
        ledger: &EffectLedger,
        requests: Vec<Request>,
    ) -> Result<Vec<EffectAdmission>, KernelFailure> {
        let park = ledger.next_park();
        let mut now = None::<DurableInstant>;
        let mut admit = Vec::with_capacity(requests.len());
        let mut executions = 0_u64;
        for request in requests {
            admit.push(match request {
                Request::Effect(request) => {
                    let call = self.identities.child_call_id(park, executions);
                    let how = match self.effects.admit(&request, call).await? {
                        Ok(draft) => {
                            executions += 1;
                            AdmitAs::Execution(Box::new(draft))
                        }
                        Err(refusal) => AdmitAs::Refused(refusal),
                    };
                    EffectAdmission {
                        identity: request.identity,
                        wait: request.wait,
                        effect: Some(request.effect),
                        admit: how,
                    }
                }
                Request::Sleep(sleep) => {
                    let now = match now {
                        Some(now) => now,
                        None => *now.insert(
                            self.store
                                .context()
                                .durable_now()
                                .await
                                .map_err(|error| QuietPointRefusal(error.to_string()))?,
                        ),
                    };
                    let millis = i64::try_from(sleep.duration.as_millis()).unwrap_or(i64::MAX);
                    EffectAdmission {
                        identity: sleep.identity,
                        wait: sleep.wait,
                        effect: None,
                        admit: AdmitAs::Sleep {
                            until: DurableInstant(now.0.saturating_add(millis)),
                        },
                    }
                }
            });
        }
        Ok(admit)
    }

    /// Commits `end` over the run's checkpoint. A cancelled run records no
    /// end, and neither does a run that never parked: its stretch admitted
    /// nothing, and runs again from its start.
    async fn ended(
        &self,
        document: &DocumentId,
        ledger: EffectLedger,
        end: End,
        saved: bool,
    ) -> Result<KernelEnd, KernelFailure> {
        if let (true, Some(recorded)) = (saved, RecordedEnd::of(&end)) {
            self.store
                .commit_run_end::<()>(EndSave {
                    document: *document,
                    ledger,
                    end: recorded,
                    host: self.effects.host_state()?,
                    with: Vec::new(),
                })
                .await?;
        }
        Ok(KernelEnd::Ended(end))
    }
}

fn wait_of(request: &Request) -> lash_kernel_vm::WaitId {
    match request {
        Request::Effect(request) => request.wait,
        Request::Sleep(sleep) => sleep.wait,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(requests_per_park: u32, join_members: u32) -> Bounds {
        Bounds {
            charge: u64::MAX,
            memory: u64::MAX,
            call_depth: 64,
            live_tasks: 8,
            requests_per_park,
            join_members,
        }
    }

    /// `K-BND-001` at the broker: a run whose bounds allow more than the
    /// broker's ceilings does not start, and a park over the run's bound
    /// admits nothing and ends the run with the typed bound error.
    #[test]
    fn the_broker_holds_a_run_to_its_park_and_join_bounds() {
        let ceilings = KernelCeilings {
            requests_per_park: 4,
            join_members: 8,
        };
        assert!(ceilings.check(&bounds(4, 8)).is_ok());
        assert!(matches!(
            ceilings.check(&bounds(5, 8)),
            Err(KernelFailure::OverCeiling {
                bound: Bound::RequestsPerPark,
                stated: 5,
                ceiling: 4,
            })
        ));
        assert!(matches!(
            ceilings.check(&bounds(4, 9)),
            Err(KernelFailure::OverCeiling {
                bound: Bound::JoinMembers,
                stated: 9,
                ceiling: 8,
            })
        ));
        assert_eq!(park_bound(&bounds(4, 8), 4), None);
        assert_eq!(
            park_bound(&bounds(4, 8), 5),
            Some(BoundExceeded {
                bound: Bound::RequestsPerPark,
                limit: 4,
            })
        );
    }
}
