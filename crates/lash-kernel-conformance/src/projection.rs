//! Save/open exchange shared by the corpus and smith laws.
use lash_kernel_state::paged::CheckpointRoot;
use lash_kernel_vm::kernel_internals::conformance::{Checkpoint, checkpoint};
use lash_kernel_vm::{Bounds, Machine, PreparedLibrary, Program};

use crate::{HarnessError, pages};

#[derive(Clone)]
pub(crate) struct Image {
    pub(crate) root: CheckpointRoot,
    pub(crate) source: pages::Source,
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
    pub(crate) layout: lash_kernel_vm::kernel_internals::Layout,
    pub(crate) library: Option<PreparedLibrary>,
    /// The memory counter at the run's end.
    pub(crate) held: Option<u64>,
}

impl Projector<'_> {
    pub(crate) fn at<M: Machine>(
        &mut self,
        machine: &mut M,
        program: &Program,
        bounds: Bounds,
        source: &pages::Source,
    ) -> Result<Checkpoint, HarnessError> {
        let sealed = machine
            .seal(pages::BUDGET)
            .map_err(fault)?
            .ok_or_else(|| fault("unbounded seal incomplete"))?;
        let state = checkpoint(
            lash_kernel_vm::kernel_internals::sealed_root(&sealed),
            Box::new(source.clone()),
        )
        .map_err(fault)?;
        let warm_memory = machine.meters().memory;
        let image = Image {
            root: lash_kernel_vm::kernel_internals::sealed_root(&sealed).clone(),
            source: source.clone(),
        };
        let imported = match self.imports {
            Some(images) => images.get(self.images.len()).unwrap_or(&image),
            None => &image,
        };
        let mut program = program.clone();
        // Preparing another layout is expensive. The warm path retains its
        // prepared library; cold imports deliberately compile a fresh one.
        let library = self.library.get_or_insert_with(|| {
            lash_kernel_vm::kernel_internals::prepare_with_layout(
                program.library.registry().clone(),
                self.layout,
            )
        });
        program.library = library.clone();
        let witness =
            pages::open::<M>(program.clone(), bounds, &image.root, source).map_err(fault)?;
        self.memory.push((warm_memory, witness.meters().memory));
        // Even the warm baseline opens a cold witness for the independent
        // accounting verdict; only importing runs replace their live machine.
        if self.imports.is_some() {
            if self.cold {
                program.library = lash_kernel_vm::kernel_internals::prepare_with_layout(
                    program.library.registry().clone(),
                    self.layout,
                );
            }
            *machine = pages::open::<M>(program, bounds, &imported.root, &imported.source)
                .map_err(fault)?;
            let reopened = machine
                .seal(pages::BUDGET)
                .map_err(fault)?
                .ok_or_else(|| fault("reopen seal incomplete"))?;
            let reopened = checkpoint(
                lash_kernel_vm::kernel_internals::sealed_root(&reopened),
                Box::new(imported.source.clone()),
            )
            .map_err(fault)?;
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

    /// Projects the run's end `end`, its memory counter there `held`.
    pub(crate) fn end(
        &mut self,
        end: &lash_kernel_vm::End,
        source: &pages::Source,
        held: u64,
    ) -> Result<(), HarnessError> {
        self.held = Some(held);
        if let lash_kernel_vm::End::Finished(finished) = end {
            self.terminal = finished
                .carry
                .as_ref()
                .map(|carry| {
                    lash_kernel_vm::kernel_internals::conformance::session(
                        carry,
                        Box::new(source.clone()),
                    )
                })
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

#[cfg(test)]
thread_local! {
    /// The charge tape every plan's machine records to while a law runs
    /// [`taped`]: what it starts and what it opens alike.
    static TAPE: std::cell::RefCell<Option<lash_kernel_vm::ChargeTape>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs `run` with every plan's machine recording its charges, and gives
/// what it answered and the charges in order.
#[cfg(test)]
pub(crate) fn taped<T>(run: impl FnOnce() -> T) -> (T, Vec<lash_kernel_exec::charge::Weight>) {
    let (answered, steps, _) = taped_work(run);
    (answered, steps)
}

/// [`taped`], also giving the work the regions tier committed in every
/// machine the run started or opened.
#[cfg(test)]
pub(crate) fn taped_work<T>(
    run: impl FnOnce() -> T,
) -> (
    T,
    Vec<lash_kernel_exec::charge::Weight>,
    lash_kernel_vm::kernel_internals::RegionWork,
) {
    let tape = lash_kernel_vm::ChargeTape::new();
    TAPE.with(|held| *held.borrow_mut() = Some(tape.clone()));
    let answered = run();
    TAPE.with(|held| *held.borrow_mut() = None);
    let work = lash_kernel_vm::kernel_internals::region_work(&tape);
    (answered, tape.steps(), work)
}

#[cfg(test)]
fn recorded(mut machine: lash_kernel_vm::KernelMachine) -> lash_kernel_vm::KernelMachine {
    TAPE.with(|held| {
        if let Some(tape) = held.borrow().as_ref() {
            machine.record_charges(tape.clone());
        }
    });
    machine
}

#[cfg(not(test))]
fn recorded(machine: lash_kernel_vm::KernelMachine) -> lash_kernel_vm::KernelMachine {
    machine
}

/// Test plans differ only in kernel layout; no second execution engine.
pub(crate) struct Plan<const L: u64>(lash_kernel_vm::KernelMachine);
impl<const L: u64> Plan<L> {
    #[cfg(test)]
    pub(crate) fn record_charges(&mut self, tape: lash_kernel_vm::ChargeTape) {
        self.0.record_charges(tape);
    }
}
impl<const L: u64> Machine for Plan<L> {
    fn start(
        program: Program,
        bounds: Bounds,
        start: lash_kernel_vm::CellStart,
        source: Box<dyn lash_kernel_state::paged::PageSource>,
    ) -> Result<Self, lash_kernel_vm::StartError> {
        lash_kernel_vm::kernel_internals::start_with_layout(
            program,
            bounds,
            start,
            lash_kernel_vm::kernel_internals::Layout(L),
            source,
        )
        .map(|machine| Self(recorded(machine)))
    }
    fn open(
        program: Program,
        bounds: Bounds,
        root: &lash_kernel_state::paged::Checkpoint,
        source: Box<dyn lash_kernel_state::paged::PageSource>,
    ) -> Result<Self, lash_kernel_vm::OpenError> {
        lash_kernel_vm::kernel_internals::open_with_layout(
            program,
            bounds,
            lash_kernel_state::kernel_internals::checkpoint_root(root),
            source,
            lash_kernel_vm::kernel_internals::Layout(L),
        )
        .map(|machine| Self(recorded(machine)))
    }
    fn set_options(&mut self, options: lash_kernel_vm::MachineOptions) {
        self.0.set_options(options);
    }
    fn take_counts(&mut self) -> lash_vocabulary::observation::SiteCounts {
        self.0.take_counts()
    }
    fn take_path(
        &mut self,
    ) -> Result<lash_vocabulary::observation::ExecutionPath, lash_kernel_doc::heap::HeapFault> {
        self.0.take_path()
    }
    fn take_caught(&mut self) -> Vec<lash_vocabulary::sites::WaitId> {
        self.0.take_caught()
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
        wait: lash_vocabulary::sites::WaitId,
        outcome: lash_kernel_vm::Outcome,
    ) -> Result<lash_kernel_vm::Delivered, lash_kernel_vm::DeliverError> {
        self.0.deliver(wait, outcome)
    }
    fn seal(
        &mut self,
        budget: lash_kernel_state::paged::SealBudget,
    ) -> Result<Option<lash_kernel_vm::SealedRoot>, lash_kernel_vm::SealError> {
        self.0.seal(budget)
    }
    fn meters(&self) -> lash_kernel_vm::Meters {
        self.0.meters()
    }
}

/// The durable ids each checkpoint reaches, in graph order: PLAN-INDEP
/// over ids holds when two plans' lists are equal (seam C of
/// `docs/design/kernel-end-state.md`).
#[cfg(test)]
pub(crate) fn ids(states: &[Checkpoint]) -> Vec<Vec<lash_kernel_doc::heap::EntityId>> {
    states.iter().map(|state| state.ids().to_vec()).collect()
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
    if !memory(&a.end) && !memory(&b.end) {
        return a == b
            && reference.states == imported.states
            && ids(&reference.states) == ids(&imported.states)
            && reference.terminal == imported.terminal;
    }
    let prefix = reference.states.len().min(imported.states.len());
    reference.states[..prefix] == imported.states[..prefix]
        && ids(&reference.states[..prefix]) == ids(&imported.states[..prefix])
        && a.prints.iter().zip(&b.prints).all(|(a, b)| a == b)
        && a.trace.iter().zip(&b.trace).all(|(a, b)| a == b)
}
