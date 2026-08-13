use std::time::Instant;

use super::super::{
    COOPERATIVE_YIELD_INSTRUCTION_BUDGET, ExecutionBound, ExecutionHost, ExecutionMode,
    ExecutionOutcome, RuntimeError, RuntimeFailure, Value,
};
use super::effects::VmEffect;
use super::heap_plan::{SlotExport, StackExport, instruction_heap_plan};
use super::{Vm, VmRunOutcome};
use crate::lexer::Span;

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
            VmOutcome::ProcessFinished(_) => {
                Err(RuntimeError::SessionProcessAdminOutsideProcess { keyword: "finish" })
            }
            VmOutcome::ProcessFailed(_) => {
                Err(RuntimeError::SessionProcessAdminOutsideProcess { keyword: "fail" })
            }
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
                error: RuntimeError::SessionProcessAdminOutsideProcess { keyword: "finish" },
                span: None,
            }),
            VmOutcome::ProcessFailed(_) => Err(RuntimeFailure {
                error: RuntimeError::SessionProcessAdminOutsideProcess { keyword: "fail" },
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
                return Err(VmTrap {
                    error,
                    instruction_ip: self.ip.min(self.chunk.code.len().saturating_sub(1)),
                    span: None,
                });
            }
            self.heap_initialized = true;
        }
        while let Some(instruction) = self.chunk.code.get(self.ip).copied() {
            let instruction_ip = self.ip;
            if let Err(error) = self.materialize_instruction_operands(instruction) {
                return Err(VmTrap {
                    error,
                    instruction_ip,
                    span: None,
                });
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
                return Err(VmTrap {
                    error,
                    instruction_ip,
                    span: None,
                });
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
                    return self.finish_run_loop(
                        active_started,
                        Err(VmTrap {
                            error,
                            instruction_ip,
                            span,
                        }),
                        instruction_ip,
                    );
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

    fn enforce_execution_bounds(&self) -> Result<(), RuntimeError> {
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

    /// Exports whatever this instruction's heap plan says it reads.
    fn materialize_instruction_operands(
        &mut self,
        instruction: super::Instruction,
    ) -> Result<(), RuntimeError> {
        let plan = instruction_heap_plan(instruction, self.chunk);
        match plan.stack {
            StackExport::Top(window) => {
                let start = self.stack.len().saturating_sub(window);
                for index in start..self.stack.len() {
                    let exported = self.heap.export_for_instruction(&self.stack[index])?;
                    self.stack[index] = exported;
                }
            }
            StackExport::All => {
                for value in &mut self.stack {
                    *value = self.heap.export_for_instruction(value)?;
                }
            }
        }
        for (slot, export) in plan.slots.into_iter().flatten() {
            match export {
                SlotExport::Read => self.materialize_slot(slot)?,
                SlotExport::Mutate => self.materialize_mutable_slot(slot)?,
            }
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

    pub(super) fn materialize_vm_state(&mut self) -> Result<(), RuntimeError> {
        for value in &mut self.stack {
            *value = self.heap.export_for_instruction(value)?;
        }
        if let Some(value) = &mut self.last_value {
            *value = self.heap.export_for_instruction(value)?;
        }
        for value in self.slots.values.iter_mut().flatten() {
            *value = self.heap.export_for_instruction(value)?;
        }
        for entry in &mut self.slots.extras.entries {
            entry.value = self.heap.export_for_instruction(&entry.value)?;
        }
        for iterator in &mut self.iter_stack {
            if let super::IterCursor::List { values, .. } = &mut iterator.cursor {
                let materialized = values
                    .iter()
                    .map(|value| self.heap.export_for_instruction(value))
                    .collect::<Result<Vec<_>, _>>()?;
                *values = materialized.into();
            }
            if let Some(value) = &mut iterator.restore.previous {
                *value = self.heap.export_for_instruction(value)?;
            }
            iterator.heapified = false;
        }
        self.extras_heapified = false;
        Ok(())
    }

    /// Imports every inline compound left in VM state into the heap.
    ///
    /// This runs after each instruction, so its cost has to be proportional to
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
        let scan_extras = !self.extras_heapified;
        let mut values = Vec::new();
        values.extend(
            self.stack
                .iter()
                .filter(|value| needs_heap_import(value))
                .cloned(),
        );
        values.extend(
            self.last_value
                .iter()
                .filter(|value| needs_heap_import(value))
                .cloned(),
        );
        values.extend(
            self.slots
                .values
                .iter()
                .flatten()
                .filter(|value| needs_heap_import(value))
                .cloned(),
        );
        if scan_extras {
            values.extend(
                self.slots
                    .extras
                    .values()
                    .filter(|value| needs_heap_import(value))
                    .cloned(),
            );
        }
        for index in &pending_iterators {
            let iterator = &self.iter_stack[*index];
            if let super::IterCursor::List {
                values: iterator_values,
                ..
            } = &iterator.cursor
            {
                values.extend(
                    iterator_values
                        .iter()
                        .filter(|value| needs_heap_import(value))
                        .cloned(),
                );
            }
            values.extend(
                iterator
                    .restore
                    .previous
                    .iter()
                    .filter(|value| needs_heap_import(value))
                    .cloned(),
            );
        }

        if !values.is_empty() {
            let imported = self.heap.import_values(values);
            self.heap.end_allocation_scope();
            let mut imported = imported?.into_iter();
            for value in &mut self.stack {
                replace_imported_value(value, &mut imported);
            }
            if let Some(value) = &mut self.last_value {
                replace_imported_value(value, &mut imported);
            }
            for value in self.slots.values.iter_mut().flatten() {
                replace_imported_value(value, &mut imported);
            }
            if scan_extras {
                for entry in &mut self.slots.extras.entries {
                    replace_imported_value(&mut entry.value, &mut imported);
                }
            }
            for index in &pending_iterators {
                let iterator = &mut self.iter_stack[*index];
                if let super::IterCursor::List {
                    values: iterator_values,
                    ..
                } = &mut iterator.cursor
                {
                    for value in iterator_values.make_mut() {
                        replace_imported_value(value, &mut imported);
                    }
                }
                if let Some(value) = &mut iterator.restore.previous {
                    replace_imported_value(value, &mut imported);
                }
            }
            debug_assert!(imported.next().is_none());
        } else {
            self.heap.end_allocation_scope();
        }
        for index in pending_iterators {
            self.iter_stack[index].heapified = true;
        }
        self.extras_heapified = true;
        if self.heap.needs_collection() {
            let roots = self.heap_roots();
            self.heap.collect(roots.iter());
        }
        Ok(())
    }

    pub(super) fn heap_roots(&self) -> Vec<Value> {
        let mut roots = self.stack.clone();
        roots.extend(self.last_value.iter().cloned());
        roots.extend(self.slots.values.iter().flatten().cloned());
        roots.extend(self.slots.extras.values().cloned());
        for iterator in &self.iter_stack {
            if let super::IterCursor::List { values, .. } = &iterator.cursor {
                roots.push(Value::List(values.clone()));
            }
            roots.extend(iterator.restore.previous.iter().cloned());
        }
        roots
    }
}

fn needs_heap_import(value: &Value) -> bool {
    matches!(value, Value::Tuple(_) | Value::List(_) | Value::Record(_))
}

fn replace_imported_value(value: &mut Value, imported: &mut impl Iterator<Item = Value>) {
    if needs_heap_import(value) {
        *value = imported.next().expect("heap import count matches");
    }
}
