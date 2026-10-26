//! In-process machine adapter for broker laws; production hosts use workers.
//!
//! The machine lives on a thread of its own, as it would in a worker: its
//! heap's page port blocks there while the activation's page service, on
//! the async runtime, answers each exchange (`K-MACH-001`: a host page
//! source may block synchronously).
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};

use lash_core_store::checkpoint_graph::NodeId;
use lash_kernel_doc::ErrorDatum;
use lash_kernel_doc::heap::{PageId, PageRef};
use lash_kernel_state::paged::{Checkpoint, PageSource};
use lash_kernel_value::names::DocumentId;
use lash_kernel_vm::{
    Bounds, CellStart, Host, KernelMachine, Machine, Outcome, Program, Step, WaitId,
};
use lash_vm_broker::kernel::closure::Closure;
use lash_vm_broker::kernel::pages::{PageExchange, SharedPages};
use lash_vm_broker::kernel::state::GraphStateFailure;
use lash_vm_broker::kernel::{
    DrivenMachine, KernelFailure, MachinePark, MachineStep, Machines, ParkedState,
};
use lash_vm_client::RunStart;

use crate::host::{WirePageSource, grant_of, link_grant};

/// The pages an in-process heap keeps read.
const CLEAN_BYTES: u64 = 64 << 20;

/// What holds a simulation's virtual clock while the machine computes on
/// its thread: called as each call begins, its answer held until the call
/// returns (as a worker call's is).
pub type CallHold = Arc<dyn Fn() -> Box<dyn std::any::Any + Send> + Send + Sync>;

/// A run whose machine lives in this process and reads `host`: the kernel
/// embedder's plain loop, for hosts that choose no isolation and for laws.
pub struct InProcess<H> {
    hold: Option<CallHold>,
    program: Program,
    bounds: Bounds,
    start: RunStart,
    document: DocumentId,
    host: Arc<std::sync::Mutex<H>>,
    /// The document's execution closure over the program's registry, which
    /// the loader assembled ([`lash_vm_broker::kernel::loader::assemble`]).
    closure: OnceLock<Result<Closure, GraphStateFailure>>,
}

impl<H> InProcess<H> {
    /// Hold `hold` across each call into the machine.
    #[must_use]
    pub fn with_hold(mut self, hold: CallHold) -> Self {
        self.hold = Some(hold);
        self
    }

    /// Perform the effects `modes` binds as reads in a fresh run.
    #[must_use]
    pub fn with_modes(mut self, modes: lash_kernel_vm::EffectModes) -> Self {
        self.start.modes = modes;
        self
    }

    /// The run of `program` from `start`, held to
    /// `bounds`, reading `host`.
    ///
    /// # Errors
    ///
    /// None today: the run's identity is the document's own.
    pub fn new(
        program: Program,
        bounds: Bounds,
        start: RunStart,
        host: H,
    ) -> Result<Self, KernelFailure> {
        CellStart::admit_host_metadata(start.host.iter().map(|(name, _)| name.as_str()))?;
        Ok(Self {
            hold: None,
            document: program.document.identity(),
            program,
            bounds,
            start,
            host: Arc::new(std::sync::Mutex::new(host)),
            closure: OnceLock::new(),
        })
    }
}

/// What the broker asks of the machine's thread.
enum Command {
    Run {
        slice: u64,
        cancel: bool,
        reply: tokio::sync::oneshot::Sender<Result<MachineStep, KernelFailure>>,
    },
    Deliver {
        wait: WaitId,
        outcome: Outcome,
        reply: tokio::sync::oneshot::Sender<Result<(), KernelFailure>>,
    },
    Seal {
        reply: tokio::sync::oneshot::Sender<Result<NodeId, KernelFailure>>,
    },
}

/// An in-process machine: the thread that holds it, and the commands it
/// serves. Dropping it ends the thread.
pub struct InProcessMachine {
    commands: mpsc::Sender<Command>,
    hold: Option<CallHold>,
}

struct Cancelling<'a, H> {
    host: &'a mut H,
    cancel: bool,
}

impl<H: Host> Host for Cancelling<'_, H> {
    fn perform(
        &mut self,
        effect: &lash_kernel_value::names::OperationUse,
        args: &[lash_kernel_doc::Datum],
        site: &lash_kernel_value::sites::PositionLabel,
        origin: &lash_kernel_value::sites::Site,
    ) -> Result<Result<lash_kernel_doc::Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        self.host.perform(effect, args, site, origin)
    }
    fn print(&mut self, value: &lash_kernel_doc::Datum) {
        self.host.print(value);
    }
    fn cancel_requested(&mut self) -> bool {
        self.cancel || self.host.cancel_requested()
    }
}

/// A heap fault as the broker states it: the activation ends as
/// infrastructure.
fn fault(fault: impl std::fmt::Display) -> KernelFailure {
    KernelFailure::Worker {
        outcome: crate::PoolError::breach(
            lash_protocol_types::engine_fault::ProtocolBreach::Machine {
                detail: lash_protocol_types::engine_fault::Detail::new(fault),
            },
        )
        .into_outcome(),
    }
}

/// The page port of a machine on its own thread: each exchange blocks the
/// thread until the activation's page service answers it on `runtime`.
fn page_source(pages: SharedPages, runtime: tokio::runtime::Handle) -> WirePageSource {
    WirePageSource::over(
        Box::new(move |request| {
            let mut pages = pages.clone();
            Ok(runtime.block_on(async move { pages.serve(request).await }))
        }),
        CLEAN_BYTES,
    )
}

impl InProcessMachine {
    /// A thread that builds its machine with `build` over a page source
    /// answered by `pages`, then serves the broker's commands with `host`.
    async fn spawn<H: Host + Send + 'static>(
        hold: Option<CallHold>,
        pages: SharedPages,
        host: Arc<std::sync::Mutex<H>>,
        build: impl FnOnce(WirePageSource) -> Result<KernelMachine, KernelFailure> + Send + 'static,
    ) -> Result<Self, KernelFailure> {
        let runtime = tokio::runtime::Handle::current();
        let held = hold.as_ref().map(|hold| hold());
        let (commands, inbox) = mpsc::channel::<Command>();
        let (built, ready) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let source = page_source(pages, runtime);
            // What the machine hands the broker crosses first.
            let port = source.port();
            let mut machine = match build(source) {
                Ok(machine) => {
                    let _ = built.send(Ok(()));
                    machine
                }
                Err(failure) => {
                    let _ = built.send(Err(failure));
                    return;
                }
            };
            while let Ok(command) = inbox.recv() {
                match command {
                    Command::Run {
                        slice,
                        cancel,
                        reply,
                    } => {
                        let mut host = host
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let step = machine
                            .run(
                                &mut Cancelling {
                                    host: &mut *host,
                                    cancel,
                                },
                                slice,
                            )
                            .map(|step| match step {
                                Step::Parked(park) => MachineStep::Parked(MachinePark {
                                    requests: park.requests,
                                    withdrawn: park.withdrawn,
                                    outstanding: park.outstanding,
                                    live: park.live,
                                }),
                                Step::Slice => MachineStep::Slice,
                                Step::Ended(end) => MachineStep::Ended(end),
                            })
                            .map_err(KernelFailure::from)
                            .and_then(|step| match step {
                                MachineStep::Slice => Ok(step),
                                step => port.flush().map(|()| step).map_err(|fault| {
                                    KernelFailure::from(lash_kernel_vm::MachineError::Heap(fault))
                                }),
                            });
                        let _ = reply.send(step);
                    }
                    Command::Deliver {
                        wait,
                        outcome,
                        reply,
                    } => {
                        let _ = reply.send(
                            machine
                                .deliver(wait, outcome)
                                .map(drop)
                                .map_err(KernelFailure::from),
                        );
                    }
                    Command::Seal { reply } => {
                        let sealed = match machine.checkpoint() {
                            Ok(sealed) => port
                                .flush()
                                .and_then(|()| port.checkpointed(sealed.page))
                                .map(|()| NodeId(sealed.page.id.0))
                                .map_err(|fault| {
                                    KernelFailure::from(lash_kernel_vm::MachineError::Heap(fault))
                                }),
                            Err(lash_kernel_vm::SealError::Heap(cause)) => {
                                Err(lash_kernel_vm::MachineError::Heap(cause).into())
                            }
                            Err(error) => Err(fault(error)),
                        };
                        let _ = reply.send(sealed);
                    }
                }
            }
        });
        ready
            .await
            .map_err(|_| fault("the machine thread ended"))??;
        drop(held);
        Ok(Self { commands, hold })
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, KernelFailure>>) -> Command,
    ) -> Result<T, KernelFailure> {
        let held = self.hold.as_ref().map(|hold| hold());
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _held = held;
        self.commands
            .send(command(reply))
            .map_err(|_| fault("the machine thread ended"))?;
        answer
            .await
            .map_err(|_| fault("the machine thread ended"))?
    }
}

#[async_trait::async_trait]
impl DrivenMachine for InProcessMachine {
    async fn run(&mut self, slice: u64, cancel: bool) -> Result<MachineStep, KernelFailure> {
        self.ask(|reply| Command::Run {
            slice,
            cancel,
            reply,
        })
        .await
    }

    async fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<(), KernelFailure> {
        self.ask(|reply| Command::Deliver {
            wait,
            outcome,
            reply,
        })
        .await
    }

    async fn seal(&mut self) -> Result<NodeId, KernelFailure> {
        self.ask(|reply| Command::Seal { reply }).await
    }

    async fn release(self) -> Result<(), KernelFailure> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl<H> Machines for InProcess<H>
where
    H: Host + Send + 'static,
{
    type Machine = InProcessMachine;

    fn document(&self) -> DocumentId {
        self.document
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn closure(&self) -> Result<&Closure, KernelFailure> {
        self.closure
            .get_or_init(|| {
                let stored = self
                    .program
                    .documents
                    .values()
                    .flat_map(|document| document.functions.iter())
                    .filter(|(id, _)| self.program.library.registry().get(id).is_none())
                    .map(|(id, definition)| (*id, Arc::new(definition.clone())))
                    .collect();
                lash_vm_broker::kernel::closure::derive(
                    self.program.functions.iter().copied(),
                    self.program.library.registry(),
                    &stored,
                    self.program.helpers(),
                    &lash_kernel_doc::LibraryDependencies(
                        self.program
                            .documents
                            .values()
                            .flat_map(|document| document.library.iter().copied())
                            .collect(),
                    ),
                )
                .map_err(|refusal| GraphStateFailure::Restore(refusal.into()))
            })
            .as_ref()
            .map_err(|failure| KernelFailure::State(failure.clone()))
    }

    async fn start(
        &self,
        binding: NodeId,
        pages: SharedPages,
    ) -> Result<Self::Machine, KernelFailure> {
        let program = Program {
            closure: PageId(self.closure()?.reference().0),
            ..self.program.clone()
        };
        // A carried session crosses as a worker's would: each page under
        // the capability the activation issues for it.
        let session = match &self.start.session {
            Some(session) => {
                let service = pages.lock().await;
                let carry = lash_vm_client::wire::CarryWire::of(session, |page| {
                    lash_vm_client::wire::PageLink::Granted(service.issue(NodeId(page.id.0)))
                });
                Some(carry.session(link_grant).map_err(fault)?)
            }
            None => None,
        };
        let (bounds, start) = (self.bounds, self.start.clone());
        InProcessMachine::spawn(self.hold.clone(), pages, self.host.clone(), move |source| {
            KernelMachine::start(
                program,
                bounds,
                CellStart {
                    owner: PageId(binding.0),
                    target: start.target,
                    args: start.args,
                    session,
                    modes: start.modes,
                    host: start.host,
                },
                Box::new(source),
            )
            .map_err(|error| match error {
                error @ lash_kernel_vm::StartError::Accounting { .. } => fault(error),
                error => error.into(),
            })
        })
        .await
    }

    async fn resume(
        &self,
        parked: &ParkedState,
        _binding: NodeId,
        pages: SharedPages,
    ) -> Result<Self::Machine, KernelFailure> {
        let root = PageRef {
            id: PageId(parked.root.0),
            grant: grant_of(&pages.lock().await.issue(parked.root)),
        };
        let program = Program {
            closure: PageId(self.closure()?.reference().0),
            ..self.program.clone()
        };
        let bounds = self.bounds;
        InProcessMachine::spawn(
            self.hold.clone(),
            pages,
            self.host.clone(),
            move |mut source| {
                let answers = source.read(&[root]).map_err(fault)?;
                let header = Checkpoint::of_answer(
                    answers
                        .first()
                        .ok_or_else(|| fault("the checkpoint root was not read"))?
                        .as_ref()
                        .map_err(fault)?,
                )
                .map_err(fault)?;
                KernelMachine::open(program, bounds, &header, Box::new(source)).map_err(|error| {
                    match error {
                        error @ lash_kernel_vm::OpenError::Accounting { .. } => fault(error),
                        error => error.into(),
                    }
                })
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::heap::HeapFault;
    use lash_kernel_state::paged::{PageAnswer, PageBody};
    use lash_vm_broker::kernel::pages::{ActivationPages, CapabilityKey, PageLimits, SpoolRoot};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    struct Quiet;
    impl Host for Quiet {
        fn print(&mut self, _: &lash_kernel_doc::Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }

    struct LosingLease {
        source: WirePageSource,
        lost: Arc<AtomicBool>,
        refused: Arc<AtomicUsize>,
    }
    impl PageSource for LosingLease {
        fn read(
            &mut self,
            pages: &[PageRef],
        ) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
            self.source.read(pages)
        }
        fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
            if self.lost.load(Ordering::Relaxed) {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return Err(HeapFault::LeaseLost);
            }
            self.source.seal(body)
        }
    }

    /// K-MACH-001/007: the broker observes a refused checkpoint's heap
    /// cause through the actual threaded command/reply boundary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_seal_reaches_the_broker_with_its_typed_cause() {
        let name = "binding".repeat(100);
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nmain {{ let {name} = 7 return {name} }}"
        ))
        .unwrap();
        let program = crate::embedding::tests::linked(
            document,
            lash_kernel_vm::PreparedLibrary::new(
                Arc::new(lash_kernel_doc::FunctionRegistry::new()),
            ),
        );
        let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
        let store: Arc<dyn lash_durable::domain::pages::PageStore> =
            Arc::new(stores.durable_store());
        let directory = tempfile::tempdir().unwrap();
        let spools = SpoolRoot::open(directory.path(), 1 << 20).unwrap();
        let pages = ActivationPages::new(
            store,
            None,
            Duration::from_secs(20),
            CapabilityKey::fresh(),
            1,
            Box::new(
                spools
                    .spool(lash_vm_broker::kernel::next_stage_lease())
                    .unwrap(),
            ),
            Arc::new(lash_core_execution::runtime::SystemClock),
            Duration::from_secs(120),
            PageLimits::within(lash_vm_protocol::DecodeLimits::standard()),
        );
        let lost = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicUsize::new(0));
        let (source_lost, source_refused) = (Arc::clone(&lost), Arc::clone(&refused));
        let mut machine = InProcessMachine::spawn(
            None,
            Arc::new(tokio::sync::Mutex::new(pages)),
            Arc::new(std::sync::Mutex::new(Quiet)),
            move |source| {
                KernelMachine::start(
                    program,
                    Bounds {
                        memory_meter: lash_kernel_vm::MemoryMeter::Off,
                        charge: lash_kernel_vm::Charge::Exact,
                        charge_limit: 10_000,
                        memory: 1 << 20,
                        call_depth: 10,
                        live_tasks: 10,
                        requests_per_park: 10,
                        join_members: 10,
                    },
                    CellStart {
                        owner: PageId([9; 32]),
                        target: lash_kernel_vm::Target::Main,
                        args: Vec::new(),
                        session: None,
                        modes: Default::default(),
                        host: Vec::new(),
                    },
                    Box::new(LosingLease {
                        source,
                        lost: source_lost,
                        refused: source_refused,
                    }),
                )
                .map_err(Into::into)
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            machine.run(1, false).await.unwrap(),
            MachineStep::Slice
        ));
        lost.store(true, Ordering::Relaxed);
        let error = machine.seal().await.unwrap_err();
        assert_eq!(refused.load(Ordering::Relaxed), 1);
        assert!(
            matches!(
                error,
                KernelFailure::Machine(lash_kernel_vm::MachineError::Heap(HeapFault::LeaseLost))
            ),
            "{error:?}"
        );
        machine.release().await.unwrap();
    }
}
