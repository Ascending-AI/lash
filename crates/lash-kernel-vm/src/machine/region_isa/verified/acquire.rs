//! Bind the canonical source stack before taking exclusive graph authority.
use super::super::*;
use super::{Code, Running};
use crate::compile::region_isa::{RegionOp, compile_region, verify_region};
use crate::compile::room::region::calls::region_isa::compile_calls;
use lash_kernel_state::kernel_internals::logical::{Owner, StateLease};

struct CanonicalPages(Arc<crate::compile::Executable>);
impl lash_kernel_state::kernel_internals::logical::LeaseLoader for CanonicalPages {
    fn load(
        &self,
        arena: &mut lash_kernel_state::arena::Arena,
        heap: &lash_kernel_state::kernel_internals::PagedHeap,
        handle: lash_kernel_doc::arena::ArenaHandle,
    ) -> Result<(), HeapFault> {
        crate::machine::project::materialize(arena, heap, &self.0, handle, 0)
    }
}

impl<'m> RegionLease<'m> {
    pub(in crate::machine::region_isa) fn acquire_verified(
        machine: &'m mut KernelMachine,
        code: Arc<Code>,
        limits: Limits,
    ) -> Result<Self, RegionError> {
        let task_id = machine.current.ok_or_else(|| invalid(Refused::Facts))?;
        let exe = Arc::clone(&machine.exe);
        let mut stack = machine
            .tasks
            .get_mut(task_id.0 as usize)
            .map_err(RegionError::Heap)?
            .ok_or_else(|| invalid(Refused::Facts))?
            .frames
            .region_stack()
            .map_err(RegionError::Heap)?;
        if stack
            .last()
            .is_none_or(|frame| frame.code != code.program.code())
        {
            return Err(invalid(Refused::Source));
        }
        let entry_frames = stack.len();
        let root = stack.first().ok_or_else(|| invalid(Refused::Facts))?;
        let entry_code = root.code;
        let entry_invocation = root.invocation;
        let root_code = if code.program.code() == root.code {
            code
        } else {
            let draft = compile_region(&exe, root.code).map_err(invalid)?;
            let calls = compile_calls(&exe, &draft).map_err(invalid)?;
            let program = verify_region(&exe, root.code, draft, &calls).map_err(invalid)?;
            Code::bind_executable(&exe, &program)?
        };
        let programs = super::programs::Programs::bind(&root_code)?;
        let mut positions = Vec::new();
        let mut dependencies = Vec::new();
        let mut inputs = Vec::new();
        for (depth, frame) in stack.iter_mut().enumerate() {
            frame
                .reconstruct_region_frontier(&exe, &machine.heap, &machine.logical, task_id)
                .map_err(RegionError::Heap)?;
            let code = programs.code(frame.code)?;
            let roots = frame
                .logical
                .as_ref()
                .ok_or_else(|| invalid(Refused::Facts))?;
            let document = exe
                .instances
                .get(&frame.link)
                .ok_or_else(|| invalid(Refused::Source))?;
            let kernel = lash_kernel_doc::KernelVersion::of(document.kernel())
                .ok_or_else(|| invalid(Refused::Source))?;
            let frontier = frame
                .source_frontier
                .as_ref()
                .ok_or_else(|| invalid(Refused::Facts))?;
            if !frontier.region_binding_matches(
                code.program.source(),
                frame.link,
                kernel,
                roots.invocation(),
            ) || frontier.position().occurrence.task != task_id
            {
                return Err(invalid(Refused::Source));
            }
            let position = if let Some(source) = &frame.pending.source {
                // The top frame resumes at the machine's charge; a parked
                // caller stands at its call's successor and pays nothing
                // until the callee returns into it.
                if depth + 1 == entry_frames && source.debt.paid != machine.charged {
                    return Err(invalid(Refused::Facts));
                }
                super::resume::position(code, source)?
            } else if let Some(position) = machine
                .regs
                .region_position(&code.program, frame.invocation)
                .map_err(RegionError::Heap)?
            {
                position
            } else {
                let unpaid = frontier
                    .position()
                    .unpaid
                    .as_ref()
                    .ok_or_else(|| invalid(Refused::Exit))?;
                let pc = code
                    .program
                    .at(unpaid)
                    .ok_or_else(|| invalid(Refused::Path))?;
                let pc =
                    super::resume::unpaid_pc(code, pc, |stmt| super::resume::active(frame, stmt))?;
                (
                    pc,
                    code.program
                        .step(pc)
                        .ok_or_else(|| invalid(Refused::Path))?
                        .before,
                )
            };
            positions.push(position);
            frame.each_value(&mut |value| dependencies.push(*value));
            inputs.extend(
                machine
                    .logical
                    .borrow()
                    .frame_owners(task_id, frame.invocation, machine.heap.pages())
                    .map_err(RegionError::Heap)?,
            );
        }
        if machine
            .logical
            .borrow()
            .holding(Owner::Incoming(task_id), machine.heap.pages())
            .map_err(RegionError::Heap)?
            .is_some()
        {
            inputs.push(Owner::Incoming(task_id));
        }
        // Main session cells held before entry enter as root registers.
        let mut sessions = Vec::new();
        for (index, cell) in root_code.sessions.iter().enumerate() {
            let owner = Owner::Session(cell.digest);
            if machine
                .logical
                .borrow()
                .holding(owner, machine.heap.pages())
                .map_err(RegionError::Heap)?
                .is_none()
            {
                continue;
            }
            let value = machine
                .session_get(&cell.name)
                .map_err(|halt| match halt {
                    crate::machine::Halt::Heap(fault) => RegionError::Heap(*fault),
                    _ => invalid(Refused::Facts),
                })?
                .ok_or_else(|| invalid(Refused::Facts))?;
            inputs.push(owner);
            dependencies.push(value);
            sessions.push((index, cell.register, value));
        }
        dependencies.extend(machine.regs.region_values().map(|(_, value)| value));
        // A step served outside the region enters with its answer, for the
        // step standing there to take.
        let served = machine
            .served
            .take()
            .filter(|served| served.task == task_id)
            .map(|served| (served.at, served.answer));
        let top = stack.last().ok_or_else(|| invalid(Refused::Facts))?;
        let top_code = programs.code(top.code)?;
        let (pc, prefix) = *positions.last().ok_or_else(|| invalid(Refused::Facts))?;
        super::resume::warm_next(machine, top_code, top, task_id, pc, prefix)?;
        let indexes: Vec<_> = (0..exe.occurrence_count()).collect();
        let task = machine
            .tasks
            .get_mut(task_id.0 as usize)
            .map_err(RegionError::Heap)?
            .ok_or_else(|| invalid(Refused::Facts))?;
        let counters = task
            .counters
            .lease_counters(&indexes)
            .map_err(RegionError::Heap)?;
        let source = source_counters::SourceCounters::bind(counters.entry())?;
        if machine
            .logical
            .borrow()
            .balance()
            .and_then(|balance| balance.total())
            .map_err(RegionError::Heap)?
            != limits.entry_balance
        {
            return Err(invalid(Refused::Facts));
        }
        let invocation = stack[0]
            .logical
            .as_ref()
            .ok_or_else(|| invalid(Refused::Facts))?
            .invocation()
            .clone();
        let next = machine
            .logical
            .borrow()
            .prospective_invocation(task_id)
            .id();
        let mut lease = StateLease::acquire_shared(&mut machine.heap, &machine.arena)
            .map_err(RegionError::Heap)?;
        lease
            .bind_loader(Box::new(CanonicalPages(Arc::clone(&exe))))
            .map_err(RegionError::Heap)?;
        for value in dependencies {
            lease.import(&value).map_err(RegionError::Heap)?;
        }
        let range = lease
            .reserve_invocations(&invocation, u64::MAX - next)
            .map_err(RegionError::Heap)?;
        let invocations = invocations::Invocations::bind(range)?;
        let crossing = if limits.entry_charge > limits.charge {
            stack
                .last()
                .and_then(|frame| frame.pending.source.as_ref())
                .filter(|source| source.refusal == Some(0) && source.debt.paid == machine.charged)
                .ok_or_else(|| invalid(Refused::Facts))?;
            Some(super::charge::Crossing::restore(
                limits.charge,
                limits.entry_charge,
            )?)
        } else {
            None
        };
        let root = stack.remove(0);
        let root_source = root.pending.source.clone();
        let root_values =
            super::resume::values(&root_code, &root, task_id, positions[0].0, positions[0].1)?;
        let mut execution = Running::acquire(
            root_code,
            programs,
            positions[0].0,
            positions[0].1,
            entry_invocation,
            root_values,
            source,
            limits,
            crossing,
            invocations,
            lease,
            root,
            &mut machine.limiters,
        )?;
        execution.reached = super::resume::reached(&execution.canonical, positions[0].1)?;
        for (index, register, value) in sessions {
            execution.registers.write(
                &mut execution.view,
                0,
                entry_invocation,
                register,
                operands::Operand::Immediate(value),
            )?;
            execution.sessions[index].held = true;
        }
        let mut activation = None;
        for (offset, frame) in stack.into_iter().enumerate() {
            let depth = offset + 1;
            let caller = Arc::clone(&execution.code);
            let RegionOp::Call { site, .. } = caller
                .program
                .op(execution.pc)
                .ok_or_else(|| invalid(Refused::Path))?
            else {
                return Err(invalid(Refused::Path));
            };
            let call = caller.call(*site)?;
            if !call
                .plan
                .targets()
                .iter()
                .any(|target| target.code() == frame.code)
            {
                return Err(invalid(Refused::Source));
            }
            let callee = frame
                .logical
                .as_ref()
                .ok_or_else(|| invalid(Refused::Facts))?
                .invocation()
                .clone();
            let next_activation = CallActivation {
                lease: Rc::clone(&execution.identity),
                invocation: frame.invocation,
                depth,
                plan: site.index(),
                caller_program: caller.program.code(),
            };
            execution.parked.push(super::calls::Parked {
                callee,
                code: Arc::clone(&caller),
                pc: execution.pc,
                prefix: execution.prefix,
                reached: execution.reached,
                activation,
                site: site.index(),
                target: frame.code,
            });
            execution.registers.reserve_frame(depth)?;
            let code = Arc::clone(execution.programs.code(frame.code)?);
            let (pc, prefix) = positions[depth];
            for (reg, value) in super::resume::values(&code, &frame, task_id, pc, prefix)? {
                execution.registers.write(
                    &mut execution.view,
                    depth,
                    frame.invocation,
                    super::bind::physical(&code.program, reg)?,
                    operands::Operand::Immediate(value),
                )?;
            }
            let loops = super::control::bind(&code.program, &frame)?;
            execution.loops.resize(
                (depth + 1) * execution.programs.loops,
                super::control::LoopState::default(),
            );
            execution.loops
                [depth * execution.programs.loops..depth * execution.programs.loops + loops.len()]
                .copy_from_slice(&loops);
            execution.code = code;
            execution.pc = pc;
            execution.prefix = prefix;
            execution.reached = super::resume::reached(&frame, prefix)?;
            execution.canonical_callees.push(frame);
            activation = Some(next_activation);
        }
        execution.activation = activation;
        execution.served = served;
        let saved = execution
            .canonical_callees
            .last()
            .and_then(|frame| frame.pending.source.clone())
            .or(root_source);
        if let Some(source) = saved {
            execution.restore_phase(source)?;
        }
        Ok(Self {
            authority: Authority::Machine(publication::Authority {
                frames: &mut task.frames,
                task_state: &mut task.record.state,
                counters,
                registers: &mut machine.regs,
                charged: &mut machine.charged,
                logical_memory: &machine.logical_memory,
                session: &mut machine.session,
                task: task_id,
                exe,
                entry_code,
                entry_invocation,
                entry_frames,
                inputs: inputs.into_boxed_slice(),
                limits,
            }),
            execution: Executing::Machine(execution),
        })
    }
}
