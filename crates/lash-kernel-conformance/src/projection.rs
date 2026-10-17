//! Save/open exchange shared by the corpus and smith laws.
use lash_kernel_state::paged::CheckpointRoot;
use lash_kernel_state::{AdmittedRequest, AdmittedWait};
use lash_kernel_vm::conformance::{Checkpoint, checkpoint};
use lash_kernel_vm::{Bounds, Machine, PreparedLibrary, Program, Request};

use crate::{HarnessError, pages};

pub(crate) fn admissions(requests: &[Request]) -> Vec<AdmittedWait> {
    requests
        .iter()
        .map(|request| match request {
            Request::Effect(e) => AdmittedWait {
                identity: e.identity.clone(),
                request: AdmittedRequest::Effect {
                    effect: e.effect.clone(),
                    args: e.args.clone(),
                },
            },
            Request::Sleep(s) => AdmittedWait {
                identity: s.identity.clone(),
                request: AdmittedRequest::Sleep {
                    duration: s.duration.into(),
                },
            },
        })
        .collect()
}

#[derive(Clone)]
pub(crate) struct Image {
    pub(crate) root: CheckpointRoot,
    pub(crate) source: pages::Source,
    admissions: Vec<AdmittedWait>,
}

#[derive(Default)]
pub(crate) struct Projector<'a> {
    pub(crate) images: Vec<Image>,
    pub(crate) states: Vec<Checkpoint>,
    pub(crate) memory: Vec<(u64, u64)>,
    pub(crate) terminal: Option<Checkpoint>,
    /// A's images opened by B at every projection, rather than B's own images.
    pub(crate) imports: Option<&'a [Image]>,
    /// Lose prepared-code caches too, as a new worker does.
    pub(crate) cold: bool,
    pub(crate) layout: lash_kernel_vm::Layout,
    pub(crate) library: Option<PreparedLibrary>,
}

impl Projector<'_> {
    pub(crate) fn at<M: Machine>(
        &mut self,
        machine: &mut M,
        program: &Program,
        bounds: Bounds,
        source: &pages::Source,
        admissions: Vec<AdmittedWait>,
    ) -> Result<Checkpoint, HarnessError> {
        let sealed = machine
            .seal(pages::BUDGET)
            .map_err(fault)?
            .ok_or_else(|| fault("unbounded seal incomplete"))?;
        let state = checkpoint(&sealed.root, Box::new(source.clone())).map_err(fault)?;
        let warm_memory = machine.meters().memory;
        let image = Image {
            root: sealed.root,
            source: source.clone(),
            admissions,
        };
        let imported = match self.imports {
            Some(images) => images.get(self.images.len()).unwrap_or(&image),
            None => &image,
        };
        let mut program = program.clone();
        // Preparing another layout is expensive. The warm path retains its
        // prepared library; cold imports deliberately compile a fresh one.
        let library = self.library.get_or_insert_with(|| {
            PreparedLibrary::with_layout(program.library.registry().clone(), self.layout)
        });
        program.library = library.clone();
        let witness = pages::open::<M>(
            program.clone(),
            bounds,
            &image.root,
            source,
            image.admissions.clone(),
        )
        .map_err(fault)?;
        self.memory.push((warm_memory, witness.meters().memory));
        // Even the warm baseline opens a cold witness for the independent
        // accounting verdict; only importing runs replace their live machine.
        if self.imports.is_some() {
            if self.cold {
                program.library =
                    PreparedLibrary::with_layout(program.library.registry().clone(), self.layout);
            }
            *machine = pages::open::<M>(
                program,
                bounds,
                &imported.root,
                &imported.source,
                imported.admissions.clone(),
            )
            .map_err(fault)?;
            let reopened = machine
                .seal(pages::BUDGET)
                .map_err(fault)?
                .ok_or_else(|| fault("reopen seal incomplete"))?;
            let reopened =
                checkpoint(&reopened.root, Box::new(imported.source.clone())).map_err(fault)?;
            let expected =
                checkpoint(&imported.root, Box::new(imported.source.clone())).map_err(fault)?;
            if reopened != expected {
                return Err(fault("cross-resume changed semantic checkpoint"));
            }
        }
        self.states.push(state.clone());
        self.images.push(image);
        Ok(state)
    }

    pub(crate) fn end(
        &mut self,
        end: &lash_kernel_vm::End,
        source: &pages::Source,
    ) -> Result<(), HarnessError> {
        if let lash_kernel_vm::End::Finished(finished) = end {
            self.terminal = finished
                .carry
                .as_ref()
                .map(|carry| lash_kernel_vm::conformance::session(carry, Box::new(source.clone())))
                .transpose()
                .map_err(fault)?;
        }
        Ok(())
    }

    pub(crate) fn check_memory(&self) -> Result<(), HarnessError> {
        for (index, (warm, cold)) in self.memory.iter().enumerate() {
            if warm != cold {
                return Err(fault(format!(
                    "memory after seal {index}: warm={warm}, cold={cold}"
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn fault(error: impl std::fmt::Display) -> HarnessError {
    HarnessError(error.to_string())
}

/// Test plans differ only in kernel layout; no second execution engine.
pub(crate) struct Plan<const L: u64>(lash_kernel_vm::KernelMachine);
impl<const L: u64> Machine for Plan<L> {
    fn start(
        program: Program,
        bounds: Bounds,
        start: lash_kernel_vm::CellStart,
        source: Box<dyn lash_kernel_state::paged::PageSource>,
        census: Box<dyn lash_kernel_state::paged::AllocationCensus>,
    ) -> Result<Self, lash_kernel_vm::StartError> {
        lash_kernel_vm::KernelMachine::start_with_layout(
            program,
            bounds,
            start,
            lash_kernel_vm::Layout(L),
            source,
            census,
        )
        .map(Self)
    }
    fn open(
        program: Program,
        bounds: Bounds,
        root: &CheckpointRoot,
        admissions: Vec<AdmittedWait>,
        source: Box<dyn lash_kernel_state::paged::PageSource>,
        census: Box<dyn lash_kernel_state::paged::AllocationCensus>,
    ) -> Result<Self, lash_kernel_vm::OpenError> {
        lash_kernel_vm::KernelMachine::open_with_layout(
            program,
            bounds,
            root,
            admissions,
            source,
            census,
            lash_kernel_vm::Layout(L),
        )
        .map(Self)
    }
    fn run(
        &mut self,
        host: &mut dyn lash_kernel_vm::Host,
        slice: u64,
    ) -> Result<lash_kernel_vm::Step, lash_kernel_vm::MachineError> {
        self.0.run(host, slice)
    }
    fn deliver(
        &mut self,
        wait: lash_kernel_vm::WaitId,
        outcome: lash_kernel_vm::Outcome,
    ) -> Result<lash_kernel_vm::Delivered, lash_kernel_vm::DeliverError> {
        self.0.deliver(wait, outcome)
    }
    fn seal(
        &mut self,
        budget: lash_kernel_state::paged::SealBudget,
    ) -> Result<Option<lash_kernel_vm::paged::SealedRoot>, lash_kernel_vm::SealError> {
        self.0.seal(budget)
    }
    fn meters(&self) -> lash_kernel_vm::Meters {
        self.0.meters()
    }
    fn task_identity(
        &self,
        identity: lash_kernel_state::paged::TaskIdentityRef,
    ) -> Result<lash_kernel_doc::TaskIdentity, lash_kernel_doc::heap::HeapFault> {
        self.0.task_identity(identity)
    }
    fn tasks(
        &self,
        root: lash_kernel_state::paged::KeyedRoot,
        filter: lash_kernel_vm::TaskFilter,
        after: Option<lash_kernel_doc::TaskId>,
        limit: u32,
    ) -> Result<lash_kernel_vm::TaskPage, lash_kernel_doc::heap::HeapFault> {
        self.0.tasks(root, filter, after, limit)
    }
}

/// Compare semantics through the earlier memory refusal; memory verdict and
/// seal counters are checked independently, never as exact refusal locations.
#[cfg(test)]
pub(crate) fn corpus_agrees(
    a: &crate::Observations,
    b: &crate::Observations,
    reference: &Projector<'_>,
    imported: &Projector<'_>,
) -> bool {
    let memory = |end: &crate::ExpectedEnd| matches!(end, crate::ExpectedEnd::Bound { name, .. } if name == "memory");
    if memory(&a.end) != memory(&b.end) {
        return false;
    }
    if !memory(&a.end) {
        return a == b
            && reference.states == imported.states
            && reference.terminal == imported.terminal;
    }
    let prefix = reference.states.len().min(imported.states.len());
    reference.states[..prefix] == imported.states[..prefix]
        && a.prints.iter().zip(&b.prints).all(|(a, b)| a == b)
        && a.trace.iter().zip(&b.trace).all(|(a, b)| a == b)
}
