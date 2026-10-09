//! One kernel run, driven to its end or to a park that outlives this
//! activation.
//!
//! The machine parks; the broker commits. Each time the machine parks with
//! new requests, the broker exports the state and commits it with the
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
//!
//! The broker does not know where the machine lives. It drives a
//! [`DrivenMachine`] that a [`Machines`] starts or resumes: lash's is a
//! machine in a resettable worker process (kernel spec §9 rule 5), and
//! [`InProcess`] is one in this process.

use lash_durable::DurableInstant;
use lash_kernel_doc::{DocumentId, EncodeError, ErrorDatum};
use lash_kernel_vm::{
    Bound, BoundExceeded, Bounds, DeliverError, Delivered, EffectRequest, End, ExportError, Host,
    ImportError, Machine, MachineError, Outcome, Program, Request, RunError, Start, StartError,
    Step, WaitId,
};
use lash_vm_protocol::{EncodedPayload, InfrastructureOutcome};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use super::ledger::{AdmittedEffect, EffectLedger, RecordedEnd};
use super::store::{AdmitAs, EffectAdmission, EndSave, ParkSave};
use crate::effects::{MemberDraft, ParentFault};
use crate::identity::CodeCallIdentities;
use crate::members::Driven;
use crate::snapshot::{DurableSnapshotStore, QuietPointRefusal};
use lash_core_execution::runtime::actor::round::SettledOutput;

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

    /// The outcome the `perform` admitted as `effect` is answered with for
    /// `output`, its execution's final record; `None` while it is not
    /// final. The default reads a completed record's payload as the result.
    /// It is called on every read of a settled wait, so it must be a
    /// function of its arguments alone: a resume reads the same records
    /// again.
    fn outcome(&self, effect: &AdmittedEffect, output: &SettledOutput) -> Option<Outcome> {
        let _ = effect;
        super::store::outcome_of(output)
    }

    /// The host's own state for the run at this save, opaque to the broker.
    fn host_state(&self) -> Result<Option<EncodedPayload>, ParentFault> {
        Ok(None)
    }
}

/// One run's machine, wherever it executes. Between calls the run is at a
/// safe point.
#[async_trait::async_trait]
pub trait DrivenMachine: Send {
    /// The parked state it writes and resumes from.
    type Parked: Serialize + DeserializeOwned + Send;

    /// Runs ready tasks until none is ready, the run ends or `slice` charge
    /// units are spent. With `cancel`, the run observes its cancel at its
    /// next safe point and ends [`End::Cancelled`].
    async fn run(&mut self, slice: u64, cancel: bool) -> Result<Step, KernelFailure>;

    /// Hands the machine one committed outcome.
    async fn deliver(&mut self, wait: WaitId, outcome: Outcome)
    -> Result<Delivered, KernelFailure>;

    /// The run's state as it stands.
    async fn export(&mut self) -> Result<Self::Parked, KernelFailure>;

    /// Gives the machine up at a clean stop: the run ended, or it stays
    /// parked beyond this activation. A machine that is only dropped was
    /// abandoned mid-run.
    async fn release(self) -> Result<(), KernelFailure>;
}

/// Where a run's machine comes from.
#[async_trait::async_trait]
pub trait Machines: Send + Sync {
    type Machine: DrivenMachine;

    /// The identity of the document the run executes.
    fn document(&self) -> DocumentId;

    /// The bounds the run is held to.
    fn bounds(&self) -> Bounds;

    /// A machine that has executed nothing.
    async fn start(&self) -> Result<Self::Machine, KernelFailure>;

    /// A machine rebuilt from the run's saved state.
    async fn resume(
        &self,
        parked: <Self::Machine as DrivenMachine>::Parked,
    ) -> Result<Self::Machine, KernelFailure>;
}

/// A run whose machine lives in this process and reads `host`: the kernel
/// embedder's plain loop, for hosts that choose no isolation and for laws.
pub struct InProcess<M, H> {
    program: Program,
    bounds: Bounds,
    start: Start,
    document: DocumentId,
    host: std::sync::Arc<std::sync::Mutex<H>>,
    machine: std::marker::PhantomData<fn() -> M>,
}

impl<M, H> InProcess<M, H> {
    /// The run of `program` from `start`, held to `bounds`, reading `host`.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::Document`] when the document has no identity.
    pub fn new(
        program: Program,
        bounds: Bounds,
        start: Start,
        host: H,
    ) -> Result<Self, KernelFailure> {
        Ok(Self {
            document: program.document.identity()?,
            program,
            bounds,
            start,
            host: std::sync::Arc::new(std::sync::Mutex::new(host)),
            machine: std::marker::PhantomData,
        })
    }
}

/// An in-process machine and the host it reads.
pub struct InProcessMachine<M, H> {
    machine: M,
    host: std::sync::Arc<std::sync::Mutex<H>>,
}

struct Cancelling<'a, H> {
    host: &'a mut H,
    cancel: bool,
}

impl<H: Host> Host for Cancelling<'_, H> {
    fn clock(&mut self) -> lash_kernel_doc::Timestamp {
        self.host.clock()
    }
    fn random(&mut self) -> u64 {
        self.host.random()
    }
    fn read(
        &mut self,
        handle: &lash_kernel_doc::Handle,
        request: &lash_kernel_doc::Datum,
    ) -> Result<lash_kernel_doc::Datum, ErrorDatum> {
        self.host.read(handle, request)
    }
    fn print(&mut self, value: &lash_kernel_doc::Datum) {
        self.host.print(value);
    }
    fn cancel_requested(&mut self) -> bool {
        self.cancel || self.host.cancel_requested()
    }
}

#[async_trait::async_trait]
impl<M, H> DrivenMachine for InProcessMachine<M, H>
where
    M: Machine + Send,
    M::Parked: Serialize + DeserializeOwned + Send,
    H: Host + Send,
{
    type Parked = M::Parked;

    async fn run(&mut self, slice: u64, cancel: bool) -> Result<Step, KernelFailure> {
        let mut host = self
            .host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(self.machine.run(
            &mut Cancelling {
                host: &mut *host,
                cancel,
            },
            slice,
        )?)
    }

    async fn deliver(
        &mut self,
        wait: WaitId,
        outcome: Outcome,
    ) -> Result<Delivered, KernelFailure> {
        Ok(self.machine.deliver(wait, outcome)?)
    }

    async fn export(&mut self) -> Result<Self::Parked, KernelFailure> {
        Ok(self.machine.export()?)
    }

    async fn release(self) -> Result<(), KernelFailure> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl<M, H> Machines for InProcess<M, H>
where
    M: Machine + Send,
    M::Parked: Serialize + DeserializeOwned + Send,
    H: Host + Send,
{
    type Machine = InProcessMachine<M, H>;

    fn document(&self) -> DocumentId {
        self.document
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    async fn start(&self) -> Result<Self::Machine, KernelFailure> {
        Ok(InProcessMachine {
            machine: M::start(self.program.clone(), self.bounds, self.start.clone())?,
            host: self.host.clone(),
        })
    }

    async fn resume(&self, parked: M::Parked) -> Result<Self::Machine, KernelFailure> {
        Ok(InProcessMachine {
            machine: M::import(self.program.clone(), self.bounds, parked)?,
            host: self.host.clone(),
        })
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
    /// The worker that hosted the machine failed, refused the run or could
    /// not be had: nothing about the guest. The run's last checkpoint
    /// stands.
    #[error("the run's worker failed: {outcome}")]
    Worker { outcome: InfrastructureOutcome },
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
    /// Drives the run `machines` starts to its end, or to a park that
    /// outlives this activation, resuming from the run's checkpoint when it
    /// has one. `cancel` cancels the run and its open effects: the machine
    /// observes it at its next safe point.
    ///
    /// # Errors
    ///
    /// [`KernelFailure`]. The run's last checkpoint stands.
    pub async fn run<S: Machines>(
        &self,
        machines: &S,
        cancel: &CancellationToken,
    ) -> Result<KernelEnd, KernelFailure> {
        let bounds = machines.bounds();
        self.ceilings.check(&bounds)?;
        let document = machines.document();
        // A run with a checkpoint commits its end over it; one that never
        // parked commits nothing, and runs again from its start.
        let mut saved = false;
        let (machine, mut ledger) = match self
            .store
            .latest_park::<<S::Machine as DrivenMachine>::Parked>()
            .await?
        {
            Some((_, checkpoint)) => {
                saved = true;
                if let Some(end) = checkpoint.end {
                    return Ok(KernelEnd::Ended(end.into_end()));
                }
                let state = checkpoint.state.ok_or(KernelFailure::EmptyCheckpoint)?;
                (machines.resume(state).await?, checkpoint.ledger)
            }
            None => (machines.start().await?, EffectLedger::new()),
        };
        let mut machine = Some(machine);
        // The saved state the machine stands at exactly, while it does: a
        // machine that only waits gives its worker back and is rebuilt
        // from this.
        let mut resting: Option<<S::Machine as DrivenMachine>::Parked> = None;
        loop {
            let running = match &mut machine {
                Some(running) => running,
                None => {
                    let state = resting.take().ok_or(KernelFailure::EmptyCheckpoint)?;
                    machine.insert(machines.resume(state).await?)
                }
            };
            let park = match running.run(self.slice, cancel.is_cancelled()).await? {
                Step::Slice => continue,
                Step::Ended(end) => {
                    if let Some(ended) = machine.take() {
                        ended.release().await?;
                    }
                    return self.ended(&document, ledger, end, saved).await;
                }
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
                if let Some(refused) = machine.take() {
                    refused.release().await?;
                }
                let end = End::Error(RunError::Bound(exceeded));
                return self.ended(&document, ledger, end, saved).await;
            }
            if !requests.is_empty() {
                let admit = self.admissions(&ledger, requests).await?;
                let committed = self
                    .store
                    .commit_park(ParkSave {
                        document,
                        state: running.export().await?,
                        ledger,
                        admit,
                        host: self.effects.host_state()?,
                        with: Vec::new(),
                    })
                    .await?;
                saved = true;
                ledger = committed.checkpoint.ledger;
                resting = committed.checkpoint.state;
            }
            if ledger.pending().next().is_none() {
                return Err(KernelFailure::Stalled);
            }
            // Once the run waits only on parked rows, with its state saved
            // as it stands, its worker is another run's to use: a process
            // this run awaits may need it (FIG-4275).
            let quiet = tokio::sync::Notify::new();
            let driven = {
                let outcome = |effect: &AdmittedEffect, output: &SettledOutput| {
                    self.effects.outcome(effect, output)
                };
                let drive = self
                    .store
                    .drive_effects_as(&ledger, cancel, &outcome, Some(&quiet));
                tokio::pin!(drive);
                loop {
                    tokio::select! {
                        driven = &mut drive => break driven?,
                        () = quiet.notified(), if machine.is_some() && resting.is_some() => {
                            if let Some(waiting) = machine.take() {
                                waiting.release().await?;
                            }
                        }
                    }
                }
            };
            let settled = match driven {
                Driven::Answered(settled) => settled,
                Driven::Suspended => {
                    if let Some(parked) = machine.take() {
                        parked.release().await?;
                    }
                    return Ok(KernelEnd::Suspended);
                }
            };
            // A machine that gave its worker back is rebuilt from its saved
            // state, and reads these outcomes again once it has parked.
            let Some(running) = machine.as_mut() else {
                continue;
            };
            resting = None;
            for outcome in settled {
                running.deliver(outcome.wait, outcome.outcome).await?;
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

fn wait_of(request: &Request) -> WaitId {
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
