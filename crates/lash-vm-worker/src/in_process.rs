//! In-process machine adapter for broker laws; production hosts use workers.
use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
use lash_kernel_doc::graph_tokens::ChildRef;
use lash_kernel_doc::{DocumentId, ErrorDatum};
use lash_kernel_vm::{Bounds, Host, KernelMachine, Machine, Outcome, Program, Start, Step, WaitId};
use lash_vm_broker::kernel::{DrivenMachine, KernelFailure, Machines, ParkedState};
use lash_vm_client::graph::{GraphStateFailure, ReleaseGraph, WholeState};
use std::sync::OnceLock;

/// The build's helper releases, staged once.
fn held_releases(
    releases: &OnceLock<Result<ReleaseGraph, GraphStateFailure>>,
) -> Result<&ReleaseGraph, KernelFailure> {
    releases
        .get_or_init(crate::graph::release_manifests)
        .as_ref()
        .map_err(|failure| KernelFailure::State(failure.clone()))
}

/// A run whose machine lives in this process and reads `host`: the kernel
/// embedder's plain loop, for hosts that choose no isolation and for laws.
pub struct InProcess<H> {
    read_policy: lash_core_store::checkpoint_graph::GraphReadPolicy,
    /// The effects a fresh run performs as reads.
    modes: lash_kernel_vm::EffectModes,
    program: Program,
    /// The dialect the run's document is written in: a new run pins that
    /// dialect's release.
    dialect: String,
    bounds: Bounds,
    start: Start,
    document: DocumentId,
    host: std::sync::Arc<std::sync::Mutex<H>>,
    releases: OnceLock<Result<ReleaseGraph, GraphStateFailure>>,
}

impl<H> InProcess<H> {
    /// Select deployment state-read and recursive data admission before driving.
    pub fn with_read_policy(
        mut self,
        policy: lash_core_store::checkpoint_graph::GraphReadPolicy,
    ) -> Self {
        self.read_policy = policy;
        self
    }

    /// Perform the effects `modes` binds as reads in a fresh run.
    #[must_use]
    pub fn with_modes(mut self, modes: lash_kernel_vm::EffectModes) -> Self {
        self.modes = modes;
        self
    }

    /// The run of `program`, written in `dialect`, from `start`, held to
    /// `bounds`, reading `host`.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::Document`] when the document has no identity.
    pub fn new(
        program: Program,
        dialect: &str,
        bounds: Bounds,
        start: Start,
        host: H,
    ) -> Result<Self, KernelFailure> {
        Ok(Self {
            read_policy: lash_core_store::checkpoint_graph::GraphReadPolicy::standard(),
            modes: lash_kernel_vm::EffectModes::recorded(),
            document: program.document.identity()?,
            program,
            dialect: dialect.to_owned(),
            bounds,
            start,
            host: std::sync::Arc::new(std::sync::Mutex::new(host)),
            releases: OnceLock::new(),
        })
    }
}

/// An in-process machine, the release it pins and the host it reads.
pub struct InProcessMachine<H> {
    machine: KernelMachine,
    release: ChildRef,
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
    fn perform(
        &mut self,
        effect: &lash_kernel_doc::EffectName,
        args: &[lash_kernel_doc::Datum],
        site: &lash_kernel_doc::Site,
    ) -> Result<Result<lash_kernel_doc::Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        self.host.perform(effect, args, site)
    }
    fn print(&mut self, value: &lash_kernel_doc::Datum) {
        self.host.print(value);
    }
    fn cancel_requested(&mut self) -> bool {
        self.cancel || self.host.cancel_requested()
    }
}

#[async_trait::async_trait]
impl<H> DrivenMachine for InProcessMachine<H>
where
    H: Host + Send,
{
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

    async fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<(), KernelFailure> {
        self.machine.deliver(wait, outcome)?;
        Ok(())
    }

    async fn export(&mut self, binding: &OwnerBinding) -> Result<WholeState, KernelFailure> {
        crate::graph::export_whole_state(&mut self.machine, binding, self.release, u64::MAX)
            .map_err(KernelFailure::State)
    }

    async fn release(self) -> Result<(), KernelFailure> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl<H> Machines for InProcess<H>
where
    H: Host + Send,
{
    type Machine = InProcessMachine<H>;

    fn document(&self) -> DocumentId {
        self.document
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn releases(&self) -> Result<&ReleaseGraph, KernelFailure> {
        held_releases(&self.releases)
    }

    async fn start(&self, _binding: &OwnerBinding) -> Result<Self::Machine, KernelFailure> {
        let release = self
            .releases()?
            .manifests
            .current(&self.dialect)
            .ok_or_else(|| {
                KernelFailure::State(GraphStateFailure::Unwritable(format!(
                    "the build holds no helper release of the dialect `{}`",
                    self.dialect
                )))
            })?;
        Ok(InProcessMachine {
            machine: KernelMachine::start_with_data_limits(
                self.program.clone(),
                self.bounds,
                self.start.clone(),
                self.read_policy.execution().kernel_data,
                self.modes.clone(),
            )?,
            release,
            host: self.host.clone(),
        })
    }

    async fn resume(
        &self,
        parked: &ParkedState,
        _binding: &OwnerBinding,
    ) -> Result<Self::Machine, KernelFailure> {
        let registry = std::sync::Arc::clone(self.program.library.registry());
        let (machine, release) = crate::graph::import_whole_state(
            self.program.clone(),
            self.bounds,
            parked.whole.root,
            &parked.whole.nodes,
            parked.admissions.clone(),
            self.releases()?,
            &registry,
            self.read_policy.execution(),
        )
        .map_err(KernelFailure::State)?;
        Ok(InProcessMachine {
            machine,
            release,
            host: self.host.clone(),
        })
    }
}
