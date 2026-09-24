use std::time::Instant;

use super::super::{
    COOPERATIVE_YIELD_INSTRUCTION_BUDGET, ExecutionBound, ExecutionHost, ExecutionMode,
    ExecutionOutcome, RuntimeError, RuntimeFailure, Value,
};
use super::effects::VmEffect;
use super::heap_plan::{SlotExport, StackExport, instruction_heap_plan};
use super::{IterCursor, Vm, VmRunOutcome};
use crate::span::Span;

pub(super) enum VmStep {
    Continue,
    Effect(VmEffect),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VmMode {
    Foreground,
    Process,
}

impl From<ExecutionMode> for VmMode {
    fn from(mode: ExecutionMode) -> Self {
        match mode {
            ExecutionMode::Foreground => Self::Foreground,
            ExecutionMode::Process => Self::Process,
        }
    }
}

impl From<VmMode> for ExecutionMode {
    fn from(mode: VmMode) -> Self {
        match mode {
            VmMode::Foreground => Self::Foreground,
            VmMode::Process => Self::Process,
        }
    }
}

pub(super) enum VmOutcome {
    Continued,
    EffectCompleted,
    Finished(Value),
    ProcessFinished(Value),
    ProcessFailed(Value),
    #[cfg(test)]
    Suspended,
}

struct VmTrap {
    error: RuntimeError,
    instruction_ip: usize,
    span: Option<Span>,
}

impl<H: ExecutionHost> Vm<'_, H> {
    pub(crate) async fn run(&mut self) -> Result<ExecutionOutcome, RuntimeError> {
        match self.run_raw().await? {
            VmOutcome::Continued => Ok(ExecutionOutcome::Continued),
            VmOutcome::EffectCompleted => Ok(ExecutionOutcome::Continued),
            VmOutcome::Finished(value) => Ok(ExecutionOutcome::Finished(value)),
            #[cfg(test)]
            VmOutcome::Suspended => Ok(ExecutionOutcome::Continued),
            VmOutcome::ProcessFinished(_) => Err(RuntimeError::SessionProcessAdminOutsideProcess {
                keyword: "finish".into(),
            }),
            VmOutcome::ProcessFailed(_) => Err(RuntimeError::SessionProcessAdminOutsideProcess {
                keyword: "fail".into(),
            }),
        }
    }

    pub(crate) async fn run_process(&mut self) -> Result<ExecutionOutcome, RuntimeError> {
        match self.run_raw().await? {
            VmOutcome::Continued => Ok(ExecutionOutcome::Finished(Value::Null)),
            VmOutcome::EffectCompleted => Ok(ExecutionOutcome::Continued),
            VmOutcome::ProcessFinished(value) => Ok(ExecutionOutcome::Finished(value)),
            VmOutcome::ProcessFailed(value) => Ok(ExecutionOutcome::Failed(value)),
            VmOutcome::Finished(value) => Ok(ExecutionOutcome::Finished(value)),
            #[cfg(test)]
            VmOutcome::Suspended => Ok(ExecutionOutcome::Continued),
        }
    }

    pub async fn run_for_mode(&mut self) -> Result<ExecutionOutcome, RuntimeError> {
        match self.mode {
            VmMode::Foreground => self.run().await,
            VmMode::Process => self.run_process().await,
        }
    }

    pub async fn run_process_until_effect(&mut self) -> Result<VmRunOutcome, RuntimeError> {
        match self.run_raw_until_effect().await? {
            VmOutcome::EffectCompleted => Ok(VmRunOutcome::EffectCompleted),
            VmOutcome::Continued => Ok(VmRunOutcome::Complete(ExecutionOutcome::Finished(
                Value::Null,
            ))),
            VmOutcome::ProcessFinished(value) | VmOutcome::Finished(value) => {
                Ok(VmRunOutcome::Complete(ExecutionOutcome::Finished(value)))
            }
            VmOutcome::ProcessFailed(value) => {
                Ok(VmRunOutcome::Complete(ExecutionOutcome::Failed(value)))
            }
            #[cfg(test)]
            VmOutcome::Suspended => Ok(VmRunOutcome::EffectCompleted),
        }
    }

    pub async fn run_process_traced_until_effect(
        &mut self,
    ) -> Result<VmRunOutcome, RuntimeFailure> {
        match self.run_raw_traced_until_effect().await? {
            VmOutcome::EffectCompleted => Ok(VmRunOutcome::EffectCompleted),
            VmOutcome::Continued => Ok(VmRunOutcome::Complete(ExecutionOutcome::Finished(
                Value::Null,
            ))),
            VmOutcome::ProcessFinished(value) | VmOutcome::Finished(value) => {
                Ok(VmRunOutcome::Complete(ExecutionOutcome::Finished(value)))
            }
            VmOutcome::ProcessFailed(value) => {
                Ok(VmRunOutcome::Complete(ExecutionOutcome::Failed(value)))
            }
            #[cfg(test)]
            VmOutcome::Suspended => Ok(VmRunOutcome::EffectCompleted),
        }
    }

    pub(crate) async fn run_traced(&mut self) -> Result<ExecutionOutcome, RuntimeFailure> {
        let result = self.run_raw_traced().await?;
        match result {
            VmOutcome::Continued => Ok(ExecutionOutcome::Continued),
            VmOutcome::EffectCompleted => Ok(ExecutionOutcome::Continued),
            VmOutcome::Finished(value) => Ok(ExecutionOutcome::Finished(value)),
            #[cfg(test)]
            VmOutcome::Suspended => Ok(ExecutionOutcome::Continued),
            VmOutcome::ProcessFinished(_) => Err(RuntimeFailure {
                error: RuntimeError::SessionProcessAdminOutsideProcess {
                    keyword: "finish".into(),
                },
                span: None,
            }),
            VmOutcome::ProcessFailed(_) => Err(RuntimeFailure {
                error: RuntimeError::SessionProcessAdminOutsideProcess {
                    keyword: "fail".into(),
                },
                span: None,
            }),
        }
    }

    pub(crate) async fn run_process_traced(&mut self) -> Result<ExecutionOutcome, RuntimeFailure> {
        let result = self.run_raw_traced().await?;
        match result {
            VmOutcome::Continued => Ok(ExecutionOutcome::Finished(Value::Null)),
            VmOutcome::EffectCompleted => Ok(ExecutionOutcome::Continued),
            VmOutcome::ProcessFinished(value) => Ok(ExecutionOutcome::Finished(value)),
            VmOutcome::ProcessFailed(value) => Ok(ExecutionOutcome::Failed(value)),
            VmOutcome::Finished(value) => Ok(ExecutionOutcome::Finished(value)),
            #[cfg(test)]
            VmOutcome::Suspended => Ok(ExecutionOutcome::Continued),
        }
    }

    pub(crate) async fn run_traced_for_mode(&mut self) -> Result<ExecutionOutcome, RuntimeFailure> {
        match self.mode {
            VmMode::Foreground => self.run_traced().await,
            VmMode::Process => self.run_process_traced().await,
        }
    }

    async fn run_raw(&mut self) -> Result<VmOutcome, RuntimeError> {
        let result = self.run_loop(false).await.map_err(|trap| trap.error);
        #[cfg(test)]
        let suspended = matches!(result, Ok(VmOutcome::Suspended));
        #[cfg(not(test))]
        let suspended = false;
        if !suspended {
            self.unwind_iterators();
        }
        result
    }

    async fn run_raw_until_effect(&mut self) -> Result<VmOutcome, RuntimeError> {
        self.run_loop(true).await.map_err(|trap| trap.error)
    }

    async fn run_raw_traced(&mut self) -> Result<VmOutcome, RuntimeFailure> {
        let result = self.run_loop(false).await.map_err(|trap| RuntimeFailure {
            error: trap.error,
            span: trap
                .span
                .or_else(|| self.chunk.spans.get(trap.instruction_ip).copied().flatten()),
        });
        #[cfg(test)]
        let suspended = matches!(result, Ok(VmOutcome::Suspended));
        #[cfg(not(test))]
        let suspended = false;
        if !suspended {
            self.unwind_iterators();
        }
        result
    }

    async fn run_raw_traced_until_effect(&mut self) -> Result<VmOutcome, RuntimeFailure> {
        self.run_loop(true).await.map_err(|trap| RuntimeFailure {
            error: trap.error,
            span: trap
                .span
                .or_else(|| self.chunk.spans.get(trap.instruction_ip).copied().flatten()),
        })
    }

    async fn run_loop(&mut self, stop_after_effect: bool) -> Result<VmOutcome, VmTrap> {
        let mut budget = COOPERATIVE_YIELD_INSTRUCTION_BUDGET;
        let mut active_started = Instant::now();
        if let Err(error) = self.enforce_execution_bounds() {
            return Err(VmTrap {
                error,
                instruction_ip: self.ip.min(self.chunk.code.len().saturating_sub(1)),
                span: None,
            });
        }
        if !self.heap_initialized {
            if let Err(error) = self.heapify_vm_state() {
                let instruction_ip = self.ip.min(self.chunk.code.len().saturating_sub(1));
                self.route_runtime_error(error, instruction_ip, None)?;
            }
            self.heap_initialized = true;
        }
        while self.active_function.is_some() || self.ip < self.chunk.root_code_len {
            if let Some(function) = self.active_function
                && self.ip == self.chunk.functions[function].end_ip
            {
                if let Err(error) = self.return_from_function() {
                    self.route_runtime_error(error, self.ip.saturating_sub(1), None)?;
                }
                continue;
            }
            let Some(instruction) = self.chunk.code.get(self.ip).copied() else {
                break;
            };
            let instruction_ip = self.ip;
            if let Err(error) = self.materialize_instruction_operands(instruction) {
                self.route_runtime_error(error, instruction_ip, None)?;
                continue;
            }
            // Under stress collection the scope opens before every instruction,
            // not just before the ones a list said could allocate. Any
            // instruction can reach an allocation — a general concat isolates
            // its result — and an allocation that commits outside an open scope
            // collects against empty pins and sweeps live objects.
            if self.heap.allocation_scope_needs_roots() {
                let roots = self.heap_roots();
                self.heap.begin_allocation_scope(roots);
            }
            active_started = Instant::now();
            self.ip += 1;
            self.instructions_executed = self.instructions_executed.saturating_add(1);
            let profile = self
                .profile
                .as_ref()
                .map(|_| (instruction.profile_tag(), Instant::now()));
            let step = match self.step_instruction_fast(instruction) {
                Ok(Some(step)) => Ok(step),
                Ok(None) => Box::pin(self.step_instruction(instruction)).await,
                Err(error) => Err(error),
            };
            let completed_effect = matches!(&step, Ok(VmStep::Effect(_)));
            let result = match step {
                Ok(VmStep::Continue) => Ok(None),
                Ok(VmStep::Effect(effect)) => {
                    if self.frames.iter().any(|frame| {
                        matches!(
                            &frame.return_target,
                            super::ReturnTarget::Callback(callback)
                                if !callback.allow_effects
                        )
                    }) {
                        self.route_runtime_error(
                            RuntimeError::EffectInBuiltinCallback,
                            instruction_ip,
                            None,
                        )?;
                        continue;
                    }
                    self.active_execution_elapsed += active_started.elapsed();
                    if let Err(error) = self.enforce_execution_bounds() {
                        return Err(VmTrap {
                            error,
                            instruction_ip,
                            span: None,
                        });
                    }
                    let result = self.resolve_effect(effect, instruction_ip).await;
                    active_started = Instant::now();
                    result
                }
                Err(error) => Err(error),
            };
            if result.is_ok()
                && let Err(error) = self.heapify_vm_state()
            {
                self.route_runtime_error(error, instruction_ip, None)?;
                continue;
            }
            if let Some((tag, start)) = profile {
                self.record_instruction_profile(tag, start.elapsed().as_nanos());
            }
            if result.is_ok() && matches!(instruction, super::Instruction::Intrinsic(_)) {
                self.active_execution_elapsed += active_started.elapsed();
                if let Err(error) = self.enforce_execution_bounds() {
                    return Err(VmTrap {
                        error,
                        instruction_ip,
                        span: None,
                    });
                }
                active_started = Instant::now();
            }
            match result {
                Ok(Some(outcome)) => {
                    return self.finish_run_loop(active_started, Ok(outcome), instruction_ip);
                }
                Ok(None) => {}
                Err(error) => {
                    let span = self.pending_error_span.take();
                    match self.route_runtime_error(error, instruction_ip, span) {
                        Ok(()) => continue,
                        Err(trap) => {
                            return self.finish_run_loop(active_started, Err(trap), instruction_ip);
                        }
                    }
                }
            }
            if stop_after_effect && completed_effect {
                return self.finish_run_loop(
                    active_started,
                    Ok(VmOutcome::EffectCompleted),
                    instruction_ip,
                );
            }
            #[cfg(test)]
            if self.test_suspension.should_suspend(completed_effect) {
                return self.finish_run_loop(
                    active_started,
                    Ok(VmOutcome::Suspended),
                    instruction_ip,
                );
            }
            budget -= 1;
            if budget == 0 {
                self.active_execution_elapsed += active_started.elapsed();
                if let Err(error) = self.enforce_execution_bounds() {
                    return Err(VmTrap {
                        error,
                        instruction_ip,
                        span: None,
                    });
                }
                self.host.yield_now().await;
                active_started = Instant::now();
                budget = COOPERATIVE_YIELD_INSTRUCTION_BUDGET;
            }
        }
        self.finish_run_loop(
            active_started,
            Ok(VmOutcome::Continued),
            self.ip.saturating_sub(1),
        )
    }

    fn finish_run_loop(
        &mut self,
        active_started: Instant,
        result: Result<VmOutcome, VmTrap>,
        instruction_ip: usize,
    ) -> Result<VmOutcome, VmTrap> {
        self.active_execution_elapsed += active_started.elapsed();
        if matches!(
            &result,
            Ok(VmOutcome::Continued | VmOutcome::Finished(_) | VmOutcome::ProcessFinished(_))
        ) {
            self.ensure_no_pending_tools().map_err(|error| VmTrap {
                error,
                instruction_ip,
                span: None,
            })?;
        }
        if result.is_ok()
            && let Err(error) = self.enforce_execution_bounds()
        {
            return Err(VmTrap {
                error,
                instruction_ip,
                span: None,
            });
        }
        result
    }

    /// Charges an intrinsic's proportional work to the instruction budget.
    ///
    /// An intrinsic's dispatch counts as one instruction, but its work can be
    /// proportional to what it reads or writes: `JSON.parse` over a megabyte is
    /// a megabyte of work behind one bytecode. Every such intrinsic charges one
    /// instruction per unit of that work — a byte of text, a UTF-16 unit, an
    /// element — so the instruction budget alone bounds what a cell can do,
    /// with no wall-clock guard behind it. The units are a function of the
    /// values alone, so the charge, and the instruction a budget runs out on,
    /// is the same on every run and every replay. The bounds check that
    /// follows every intrinsic's dispatch enforces it.
    ///
    /// The collection shaping builtins (`charge_collection_work` in `ops.rs`)
    /// and the regexp engine (`grant_regexp_fuel`) charge the same budget.
    pub(super) fn charge_intrinsic_work(&mut self, units: usize) {
        self.instructions_executed = self.instructions_executed.saturating_add(units as u64);
    }

    fn enforce_execution_bounds(&self) -> Result<(), RuntimeError> {
        // This is the structural taxonomy boundary: every error returned from
        // here is an uncatchable execution terminal, and callers return it
        // directly without consulting the handler stack.
        if self.host.is_cancelled() {
            return Err(RuntimeError::HostCancelled);
        }
        let bounds = self.host.execution_bounds();
        if let ExecutionBound::Bounded(limit) = bounds.instruction_budget
            && self.instructions_executed > limit.get()
        {
            return Err(RuntimeError::InstructionBudgetExceeded { limit: limit.get() });
        }
        if let ExecutionBound::Bounded(limit) = bounds.deadline
            && self.active_execution_elapsed > limit
        {
            return Err(RuntimeError::ExecutionDeadlineExceeded {
                limit_ms: limit.as_millis(),
            });
        }
        if let ExecutionBound::Bounded(limit) = bounds.memory_limit
            && self.heap.live_logical_bytes() > limit.get()
        {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: limit.get(),
                attempted: self.heap.live_logical_bytes(),
            });
        }
        Ok(())
    }

    fn route_runtime_error(
        &mut self,
        error: RuntimeError,
        instruction_ip: usize,
        span: Option<Span>,
    ) -> Result<(), VmTrap> {
        let trap = |error| VmTrap {
            error,
            instruction_ip,
            span,
        };
        let error = self.ecma_throw(error).map_err(trap)?;
        if !error.is_uncatchable_terminal() && self.has_exception_scope() {
            match self.throw_runtime_error(&error, instruction_ip, span) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(terminal) => return Err(trap(terminal)),
            }
        }
        // An uncaught thrown value leaves the VM detached, as an explicit
        // `throw` does: the host never sees a reference into this heap.
        let error = match error {
            RuntimeError::UncaughtException { value } => RuntimeError::UncaughtException {
                value: self.heap.export(&value).map_err(trap)?,
            },
            error => error,
        };
        Err(trap(error))
    }

    /// Exports whatever this instruction's heap plan says it reads.
    fn materialize_instruction_operands(
        &mut self,
        instruction: super::Instruction,
    ) -> Result<(), RuntimeError> {
        if matches!(
            instruction,
            super::Instruction::LoadField { .. }
                | super::Instruction::LoadFieldUnwrap { .. }
                | super::Instruction::Field(_)
                | super::Instruction::Index
        ) {
            return Ok(());
        }
        let plan = instruction_heap_plan(instruction, self.chunk)?;
        match plan.stack {
            StackExport::Top(window) => {
                let start = self.stack.len().saturating_sub(window);
                for index in start..self.stack.len() {
                    let exported = if matches!(
                        instruction,
                        super::Instruction::Pop
                            | super::Instruction::ToBool
                            | super::Instruction::JumpIfFalse(_)
                            | super::Instruction::JumpIfTrue(_)
                            | super::Instruction::Unary(_)
                            | super::Instruction::Binary(_)
                            | super::Instruction::BeginIter(_)
                    ) && matches!(
                        &self.stack[index],
                        Value::Ref(id) if self.heap.is_javascript_vm_object(*id)?
                            // A loop over an array reads the array itself, so
                            // it sees what its body changes (FIG-3625).
                            || matches!(instruction, super::Instruction::BeginIter(_))
                                && matches!(self.heap.get(*id)?, crate::runtime::heap::HeapObject::List(_))
                    ) {
                        self.stack[index].clone()
                    } else {
                        self.heap.export_for_instruction(&self.stack[index])?
                    };
                    self.stack[index] = exported;
                }
            }
        }
        match plan.slots {
            SlotExport::None => {}
            SlotExport::Read(slot) => self.materialize_slot(slot)?,
            SlotExport::Mutate(slot) => self.materialize_mutable_slot(slot)?,
        }
        Ok(())
    }

    fn materialize_slot(&mut self, slot: usize) -> Result<(), RuntimeError> {
        if let Some(value) = self.slots.values.get_mut(slot).and_then(Option::as_mut) {
            *value = self.heap.export_for_instruction(value)?;
        }
        Ok(())
    }

    pub(super) fn materialize_mutable_slot(&mut self, slot: usize) -> Result<(), RuntimeError> {
        if let Some(value) = self.slots.values.get_mut(slot).and_then(Option::as_mut) {
            *value = self.heap.export_for_mutation(value)?;
        }
        Ok(())
    }

    /// Imports every inline compound left in VM state into the heap.
    ///
    /// This pass is load-bearing for exotic members: constructor and mutation
    /// APIs also import defensively, while this guarantees values produced by
    /// ordinary instructions are heap references before a later exotic call
    /// can store them. It runs after each instruction, so its cost has to be proportional to
    /// what the instruction could have changed, not to the data the VM happens
    /// to hold. The operand stack and the slot table are bounded by the
    /// program's shape, but iterator cursors and the extra-globals record are
    /// not: both are written once and then only read, so each carries a flag and
    /// is scanned exactly once.
    fn heapify_vm_state(&mut self) -> Result<(), RuntimeError> {
        if self.heap.allocation_scope_needs_roots() {
            self.heap.begin_allocation_scope(self.heap_roots());
        }
        let pending_iterators = self
            .iter_stack
            .iter()
            .enumerate()
            .filter_map(|(index, iterator)| (!iterator.heapified).then_some(index))
            .collect::<Vec<_>>();
        let scan_extras = !self.slots.extras_heapified;
        // One enumeration drives both halves: staging clones each holder's
        // value in this order, and write-back resolves the same holders to
        // the imported objects in this order, so order and the
        // `needs_heap_import` filter hold by construction rather than by two
        // positional walks agreeing.
        let (holders, durable_len) = self.heap_import_holders(&pending_iterators, scan_extras);
        if !holders.is_empty() {
            let values = holders
                .iter()
                .map(|holder| self.heap_import_value(*holder).clone())
                .collect();
            let imported = self.heap.import_values(values, durable_len);
            self.heap.end_allocation_scope();
            let imported = imported?;
            debug_assert_eq!(imported.len(), holders.len());
            for (holder, value) in holders.iter().copied().zip(imported) {
                *self.heap_import_value_mut(holder) = value;
            }
        } else {
            self.heap.end_allocation_scope();
        }
        for index in pending_iterators {
            self.iter_stack[index].heapified = true;
        }
        self.slots.extras_heapified = true;
        if self.heap.needs_collection() {
            let roots = self.heap_roots();
            self.heap.collect(roots.iter());
        }
        Ok(())
    }

    /// The mutable value holders `heapify_vm_state` imports, in canonical
    /// order, filtered to the ones holding an inline compound.
    ///
    /// Returns the holders plus the length of the durable prefix: slot, extra
    /// and iterator-restore holders enumerate first so `import_values` sees
    /// exactly the durable-then-transient split the old pair of positional
    /// walks produced — VM slot values are durable unconditionally here, even
    /// where `visit_vm_roots` would call the same holder transient.
    fn heap_import_holders(
        &self,
        pending_iterators: &[usize],
        scan_extras: bool,
    ) -> (Vec<VmValueHolder>, usize) {
        let mut holders = Vec::new();
        for (index, value) in self.slots.values.iter().enumerate() {
            if value.as_ref().is_some_and(needs_heap_import) {
                holders.push(VmValueHolder::Slot(index));
            }
        }
        if scan_extras {
            for (index, entry) in self.slots.extras.entries.iter().enumerate() {
                if needs_heap_import(&entry.value) {
                    holders.push(VmValueHolder::Extra(index));
                }
            }
        }
        for &index in pending_iterators {
            if let Some(value) = &self.iter_stack[index].restore.previous
                && needs_heap_import(value)
            {
                holders.push(VmValueHolder::IteratorRestore(index));
            }
        }
        let durable_len = holders.len();
        for (index, value) in self.stack.iter().enumerate() {
            if needs_heap_import(value) {
                holders.push(VmValueHolder::Operand(index));
            }
        }
        if let Some(value) = &self.last_value
            && needs_heap_import(value)
        {
            holders.push(VmValueHolder::LastValue);
        }
        for &index in pending_iterators {
            if let IterCursor::List { values, .. } = &self.iter_stack[index].cursor {
                for (member, value) in values.iter().enumerate() {
                    if needs_heap_import(value) {
                        holders.push(VmValueHolder::IteratorCursor {
                            iterator: index,
                            member,
                        });
                    }
                }
            }
        }
        (holders, durable_len)
    }

    /// The `expect` stays: each holder was enumerated from this same state by
    /// `heap_import_holders` and nothing between staging and write-back can
    /// move it (each site's message states it).
    #[expect(
        clippy::expect_used,
        reason = "holder was enumerated from this same state"
    )]
    fn heap_import_value(&self, holder: VmValueHolder) -> &Value {
        match holder {
            VmValueHolder::Slot(index) => self.slots.values[index]
                .as_ref()
                .expect("enumerated slot holder"),
            VmValueHolder::Extra(index) => &self.slots.extras.entries[index].value,
            VmValueHolder::IteratorRestore(index) => self.iter_stack[index]
                .restore
                .previous
                .as_ref()
                .expect("enumerated iterator restore holder"),
            VmValueHolder::Operand(index) => &self.stack[index],
            VmValueHolder::LastValue => self
                .last_value
                .as_ref()
                .expect("enumerated last-value holder"),
            VmValueHolder::IteratorCursor { iterator, member } => {
                match &self.iter_stack[iterator].cursor {
                    IterCursor::List { values, .. } => &values[member],
                    IterCursor::Live { .. } | IterCursor::Range { .. } => {
                        unreachable!("iterator cursor holders only enumerate List cursors")
                    }
                }
            }
        }
    }

    /// The `expect` stays: see `heap_import_value`.
    #[expect(
        clippy::expect_used,
        reason = "holder was enumerated from this same state"
    )]
    fn heap_import_value_mut(&mut self, holder: VmValueHolder) -> &mut Value {
        match holder {
            VmValueHolder::Slot(index) => self.slots.values[index]
                .as_mut()
                .expect("enumerated slot holder"),
            VmValueHolder::Extra(index) => &mut self.slots.extras.entries[index].value,
            VmValueHolder::IteratorRestore(index) => self.iter_stack[index]
                .restore
                .previous
                .as_mut()
                .expect("enumerated iterator restore holder"),
            VmValueHolder::Operand(index) => &mut self.stack[index],
            VmValueHolder::LastValue => self
                .last_value
                .as_mut()
                .expect("enumerated last-value holder"),
            VmValueHolder::IteratorCursor { iterator, member } => {
                match &mut self.iter_stack[iterator].cursor {
                    IterCursor::List { values, .. } => &mut values.make_mut()[member],
                    IterCursor::Live { .. } | IterCursor::Range { .. } => {
                        unreachable!("iterator cursor holders only enumerate List cursors")
                    }
                }
            }
        }
    }

    pub(super) fn heap_roots(&self) -> Vec<Value> {
        let mut roots = Vec::new();
        super::visit_vm_roots(self, &mut roots);
        roots
    }
}

/// One mutable `Value` location the post-instruction heapify pass imports
/// through, in the order `heap_import_holders` enumerates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VmValueHolder {
    Slot(usize),
    Extra(usize),
    IteratorRestore(usize),
    Operand(usize),
    LastValue,
    IteratorCursor { iterator: usize, member: usize },
}

fn needs_heap_import(value: &Value) -> bool {
    matches!(value, Value::Tuple(_) | Value::List(_) | Value::Record(_))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{ProjectedBindings, Record, SlotState};
    use super::*;
    use crate::runtime::Compiler;
    use crate::runtime::vm::{IterCursor, IterState, LoopRestore};

    fn holder_test_vm<'a>(
        chunk: &'a super::super::Chunk,
        host: &'a crate::testing::harness::EchoHost,
    ) -> Vm<'a, crate::testing::harness::EchoHost> {
        Vm::new(
            chunk,
            SlotState::from_globals(
                Record::new(),
                &chunk.slot_names,
                &chunk.private_slots,
                &ProjectedBindings::new(),
                Vec::new(),
            ),
            host,
            None,
            ExecutionMode::Foreground,
        )
    }

    fn test_chunk() -> super::super::Chunk {
        Compiler::compile_program(&crate::testing::ast_builders::program(vec![
            crate::testing::ast_builders::assign("x", crate::testing::ast_builders::num(0.0)),
            crate::testing::ast_builders::assign("y", crate::testing::ast_builders::num(0.0)),
            crate::testing::ast_builders::finish(crate::testing::ast_builders::num(0.0)),
        ]))
        .0
    }

    /// The enumeration's durable prefix is exactly the sequence the old collect
    /// pass walked: slots, then extra globals, then pending iterator restore
    /// values — each in position order, each filtered to inline compounds —
    /// followed by the transient operand, last-value and cursor members.
    #[test]
    fn heap_import_holders_pin_the_durable_prefix_order() {
        let host = crate::testing::harness::EchoHost;
        let chunk = test_chunk();
        let mut vm = holder_test_vm(&chunk, &host);

        let slot_value = Value::List(vec![Value::Number(1.0)].into());
        let extra_value = Value::Tuple(vec![Value::Number(2.0)].into());
        let restore_value = Value::Record(Arc::new(Record::new()));
        let operand_value = Value::List(vec![Value::Number(3.0)].into());
        let last = Value::Tuple(vec![Value::Number(4.0)].into());
        let cursor_member = Value::Record(Arc::new(Record::new()));

        vm.slots.values[0] = Some(slot_value.clone());
        vm.slots.values[1] = Some(Value::Number(9.0));
        vm.slots.extras.insert("g".to_string(), extra_value.clone());
        vm.iter_stack.push(IterState {
            cursor: IterCursor::List {
                values: vec![cursor_member.clone(), Value::Number(5.0)].into(),
                index: 0,
                collection: None,
            },
            binding: 0,
            restore: LoopRestore {
                previous: Some(restore_value.clone()),
            },
            heapified: false,
        });
        // An already-heapified iterator holds compounds but is not pending, so
        // it never enters the enumeration.
        vm.iter_stack.push(IterState {
            cursor: IterCursor::List {
                values: vec![Value::List(vec![].into())].into(),
                index: 0,
                collection: None,
            },
            binding: 1,
            restore: LoopRestore { previous: None },
            heapified: true,
        });
        vm.stack.push(Value::Bool(true));
        vm.stack.push(operand_value.clone());
        vm.last_value = Some(last.clone());

        let (holders, durable_len) = vm.heap_import_holders(&[0], true);
        assert_eq!(
            holders,
            vec![
                VmValueHolder::Slot(0),
                VmValueHolder::Extra(0),
                VmValueHolder::IteratorRestore(0),
                VmValueHolder::Operand(1),
                VmValueHolder::LastValue,
                VmValueHolder::IteratorCursor {
                    iterator: 0,
                    member: 0
                },
            ]
        );
        assert_eq!(
            durable_len, 3,
            "the durable prefix is slots + extras + iterator restores"
        );
        let staged = holders
            .iter()
            .map(|holder| vm.heap_import_value(*holder).clone())
            .collect::<Vec<_>>();
        assert_eq!(
            staged,
            vec![
                slot_value,
                extra_value,
                restore_value,
                operand_value,
                last,
                cursor_member
            ]
        );

        // A VM whose extras were already imported keeps them out of the
        // durable prefix entirely, as before.
        let (holders, durable_len) = vm.heap_import_holders(&[0], false);
        assert_eq!(durable_len, 2);
        assert!(!holders.contains(&VmValueHolder::Extra(0)));
    }

    /// One inline record held by a slot and by the operand stack imports to a
    /// single heap object: the durable slot import wins, and the transient
    /// holder reuses it by tree identity.
    #[test]
    fn a_slot_and_an_operand_sharing_one_record_import_to_one_object() {
        let host = crate::testing::harness::EchoHost;
        let chunk = test_chunk();
        let mut vm = holder_test_vm(&chunk, &host);

        let shared = Value::Record(Arc::new(Record::from_iter([(
            "k".to_string(),
            Value::Number(1.0),
        )])));
        vm.slots.values[0] = Some(shared.clone());
        vm.stack.push(shared);

        vm.heapify_vm_state().expect("heapify");

        let (Value::Ref(slot_id), Value::Ref(operand_id)) = (
            vm.slots.values[0].as_ref().expect("slot keeps its value"),
            &vm.stack[0],
        ) else {
            panic!("slot and operand must both hold heap references")
        };
        assert_eq!(slot_id, operand_id);
        assert_eq!(vm.heap.objects_in_id_order().count(), 1);
    }
}
