//! Bytecode executor for compiled chunks, host effects, and trace/profile data.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ast::{BinaryOp, JavaScriptBinaryOp, JavaScriptUnaryOp, UnaryOp};
use crate::lexer::Span;
use crate::{
    LashlangExecutionChild, LashlangExecutionObservation, LashlangExecutionSite,
    ProcessBranchSelection,
};
use rustc_hash::FxHashMap;

mod continuation;
mod control;
mod effects;
mod exceptions;
mod heap_plan;
mod javascript;
mod javascript_codec;
mod javascript_json;
mod javascript_substrate;
mod reference_assignment;

#[cfg(test)]
use continuation::TestSuspension;
#[cfg(test)]
pub(crate) use continuation::VM_CONTINUATION_FORMAT_VERSION;
pub use continuation::{
    ContinuationError, VmContinuation, VmFinallyCompletionContinuation, VmFinallyContinuation,
    VmHandlerContinuation, VmHeapContinuation, VmIteratorContinuation, VmIteratorCursor,
    VmPendingErrorOriginContinuation, VmProfileContinuation, VmRunOutcome,
};
pub(crate) use continuation::{VmFrameContinuation, VmFrameReturnContinuation};
use control::{VmMode, VmStep};
use effects::VmEffect;
use exceptions::{ExceptionHandler, FinallyCompletion, FinallyState};

use super::host::{ExecutionMode, ProcessEventKind, SleepKind};
use super::record::{Record, record_with_capacity};
use super::schema::{
    ValidationPlan, compile_schema_value, execute_validate_builtin, execute_validation_plan,
};
use super::value::ProjectedValue;
use super::{
    Chunk, ClosureParameterModel, CompiledProgram, ExecutionHost, ExecutionOutcome,
    ExecutionScratch, Heap, HeapId, HeapObject, ImageValue, Instruction, InstructionProfileTag,
    IntrinsicOp, LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY, LASH_TYPE_KEY,
    ListValue, Name, PersistedRoots, ProfileAccumulator, ProfileReport, ProjectedBindings,
    ResourceHandle, RuntimeError, State, Value, add_assign_index_number, add_values, as_number,
    assign_path, eval_binary_values, eval_compare_values, eval_javascript_binary,
    eval_javascript_unary, eval_number_binary_values, eval_number_compare_values,
    eval_number_numeric_binary_value, execute_compiled_format, execute_compiled_format_direct,
    execute_compiled_format_one_number_compact_direct, execute_intrinsic,
    execute_push_builtin_async, is_truthy, is_truthy_async, iterable_values, javascript_join,
    javascript_split, materialize_projected_async, materialize_value, range_bounds,
    range_bounds_async, read_field_direct, read_index_direct, read_javascript_field_direct,
    read_javascript_heap_field, read_javascript_heap_index, read_javascript_index_direct_with_key,
    regexp_string, unwrap_tool_result, unwrap_type_value,
};

#[derive(Clone)]
pub(crate) struct SlotState {
    values: Vec<Option<Value>>,
    projected: Vec<bool>,
    extras: Record,
}

impl SlotState {
    pub(crate) fn from_globals(
        mut globals: Record,
        slot_names: &[Name],
        projected_bindings: &ProjectedBindings,
    ) -> Self {
        let mut values = Vec::with_capacity(slot_names.len());
        let mut projected = Vec::with_capacity(slot_names.len());
        for name in slot_names {
            if let Some(value) = projected_bindings.get_symbol(name.symbol) {
                globals.remove_symbol(name.symbol);
                values.push(Some(Value::Projected(value)));
                projected.push(true);
            } else {
                values.push(globals.remove_symbol(name.symbol));
                projected.push(false);
            }
        }
        Self {
            values,
            projected,
            extras: globals,
        }
    }

    pub(crate) fn from_globals_with_scratch(
        mut globals: Record,
        slot_names: &[Name],
        scratch: &mut ExecutionScratch,
        projected_bindings: &ProjectedBindings,
    ) -> Self {
        let mut values = std::mem::take(&mut scratch.slot_values);
        values.clear();
        if values.capacity() < slot_names.len() {
            values.reserve(slot_names.len() - values.capacity());
        }
        let mut projected = Vec::with_capacity(slot_names.len());
        for name in slot_names {
            if let Some(value) = projected_bindings.get_symbol(name.symbol) {
                globals.remove_symbol(name.symbol);
                values.push(Some(Value::Projected(value)));
                projected.push(true);
            } else {
                values.push(globals.remove_symbol(name.symbol));
                projected.push(false);
            }
        }
        Self {
            values,
            projected,
            extras: globals,
        }
    }

    pub(crate) fn get(&self, slot: usize) -> Option<&Value> {
        self.values.get(slot).and_then(Option::as_ref)
    }

    fn get_mut(&mut self, slot: usize) -> Option<&mut Value> {
        self.values.get_mut(slot).and_then(Option::as_mut)
    }

    fn assign(
        &mut self,
        slot: usize,
        value: Value,
        slot_names: &[Name],
    ) -> Result<(), RuntimeError> {
        self.ensure_assignable(slot, slot_names)?;
        self.values[slot] = Some(materialize_value(value));
        Ok(())
    }

    fn assign_loop_binding(&mut self, slot: usize, value: Value) {
        self.values[slot] = Some(materialize_value(value));
    }

    fn ensure_assignable(&self, slot: usize, slot_names: &[Name]) -> Result<(), RuntimeError> {
        if self.projected.get(slot).copied().unwrap_or(false) {
            return Err(RuntimeError::ReadOnlyProjectedBinding {
                name: slot_names[slot].text.to_string(),
            });
        }
        Ok(())
    }

    fn capture_temporary(&self, slot: usize) -> LoopRestore {
        LoopRestore {
            previous: self.values[slot].clone(),
        }
    }

    fn restore_temporary(&mut self, slot: usize, restore: LoopRestore) {
        self.values[slot] = restore.previous;
    }

    pub(crate) fn into_globals(self, slot_names: &[Name]) -> Record {
        let mut extras = self.extras;
        for ((name, value), projected) in slot_names.iter().zip(self.values).zip(self.projected) {
            if projected {
                extras.remove_symbol(name.symbol);
                continue;
            }
            match value {
                Some(value) => {
                    extras.insert_symbolized(
                        name.symbol,
                        name.text.clone(),
                        materialize_value(value),
                    );
                }
                None => {
                    extras.remove_symbol(name.symbol);
                }
            }
        }
        extras
    }

    fn recycle_into_globals(
        self,
        slot_names: &[Name],
        slot_values: &mut Vec<Option<Value>>,
    ) -> Record {
        let mut extras = self.extras;
        let mut values = self.values;
        for ((name, value), projected) in
            slot_names.iter().zip(values.iter_mut()).zip(self.projected)
        {
            if projected {
                extras.remove_symbol(name.symbol);
                continue;
            }
            match value.take() {
                Some(value) => {
                    extras.insert_symbolized(
                        name.symbol,
                        name.text.clone(),
                        materialize_value(value),
                    );
                }
                None => {
                    extras.remove_symbol(name.symbol);
                }
            }
        }
        values.clear();
        *slot_values = values;
        extras
    }
}

pub struct Vm<'a, H> {
    chunk: &'a Chunk,
    ip: usize,
    stack: Vec<Value>,
    last_value: Option<Value>,
    slots: SlotState,
    host: &'a H,
    mode: VmMode,
    iter_stack: Vec<IterState>,
    active_function: Option<usize>,
    frames: Vec<CallFrame>,
    handlers: Vec<ExceptionHandler>,
    finally_stack: Vec<FinallyState>,
    lashlang_execution_occurrences: FxHashMap<String, u64>,
    profile: Option<ProfileAccumulator>,
    validation_plans: FxHashMap<usize, (Arc<Record>, ValidationPlan)>,
    pending_error_span: Option<Span>,
    instructions_executed: u64,
    active_execution_elapsed: Duration,
    pub(crate) heap: Heap,
    heap_initialized: bool,
    /// Whether the extra-globals record has been imported into the heap. It is
    /// written once, when the VM is built or restored, and only read after that.
    extras_heapified: bool,
    pub(crate) reference_semantics: bool,
    assigned_globals: std::collections::BTreeSet<String>,
    #[cfg(test)]
    test_suspension: TestSuspension,
}

#[derive(Clone)]
pub(super) struct ActiveLashlangExecutionNode {
    pub(super) site: LashlangExecutionSite,
    pub(super) occurrence: u64,
}

fn validation_plan_cache_entry(schema: &Value) -> Option<(usize, Arc<Record>)> {
    match schema {
        Value::Record(record) => Some((Arc::as_ptr(record) as usize, record.clone())),
        _ => None,
    }
}

impl<'a, H: ExecutionHost> Vm<'a, H> {
    #[cfg(test)]
    pub(crate) fn suspend_after_instructions(&mut self, count: usize) {
        assert!(count > 0, "suspension budget must be positive");
        self.test_suspension = TestSuspension::AfterInstructions(count);
    }

    #[cfg(test)]
    pub(crate) fn suspend_after_effects(&mut self, count: usize) {
        assert!(count > 0, "suspension budget must be positive");
        self.test_suspension = TestSuspension::AfterEffects(count);
    }

    pub(crate) fn enable_profile(&mut self) {
        self.profile = Some(ProfileAccumulator::default());
    }

    fn current_instruction_ip(&self) -> usize {
        self.ip.saturating_sub(1)
    }

    fn execute_dynamic_validate(
        &mut self,
        value: Value,
        schema: &Value,
    ) -> Result<Value, RuntimeError> {
        let Some(schema) = unwrap_type_value(schema) else {
            return execute_validate_builtin(value, schema);
        };
        let Some((key, schema_record)) = validation_plan_cache_entry(schema) else {
            let plan = compile_schema_value(schema);
            return execute_validation_plan(value, &plan);
        };
        let plan = self
            .validation_plans
            .entry(key)
            .or_insert_with(|| (schema_record, compile_schema_value(schema)));
        execute_validation_plan(value, &plan.1)
    }

    #[inline(always)]
    fn step_instruction_fast(
        &mut self,
        instruction: Instruction,
    ) -> Result<Option<VmStep>, RuntimeError> {
        match instruction {
            Instruction::PushConst(index) => {
                self.stack.push(self.chunk.constants[index].clone());
            }
            Instruction::PushNull => self.stack.push(Value::Null),
            Instruction::PushUndefined => self.stack.push(Value::Undefined),
            Instruction::PushBool(value) => self.stack.push(Value::Bool(value)),
            Instruction::PushNumber(value) => self.stack.push(Value::Number(value)),
            Instruction::LoadName(name) => {
                let value = self.load_slot(name)?.clone();
                self.stack.push(value);
            }
            Instruction::Duplicate => {
                let value = self
                    .stack
                    .last()
                    .cloned()
                    .ok_or(RuntimeError::VmStackUnderflow)?;
                self.stack.push(value);
            }
            Instruction::DeepCopy => {
                let value = self.pop_stack()?;
                self.stack.push(self.heap.isolate_value(&value)?);
            }
            Instruction::StoreName(name) => {
                let value = self.pop_stack()?;
                self.slots.assign(
                    name,
                    value.clone(),
                    slot_names_for(self.chunk, self.active_function),
                )?;
                self.record_assignment(name);
                self.last_value = Some(value);
            }
            Instruction::StoreConst { slot, constant } => {
                let value = self.chunk.constants[constant].clone();
                self.slots.assign(
                    slot,
                    value.clone(),
                    slot_names_for(self.chunk, self.active_function),
                )?;
                self.record_assignment(slot);
                self.last_value = Some(value);
            }
            Instruction::BuildTuple(len) => {
                let values = self.pop_n(len)?;
                self.stack.push(Value::Tuple(values.into()));
            }
            Instruction::BuildList(len) => {
                let values = self.pop_n(len)?;
                self.stack.push(Value::List(values.into()));
            }
            Instruction::BuildHeapList(len) => {
                let values = self.pop_n(len)?;
                self.stack.push(self.heap.allocate_list(values)?);
            }
            Instruction::ListAppend => {
                if self.stack.len() < 2 {
                    return Err(RuntimeError::VmStackUnderflow);
                }
                let list_index = self.stack.len() - 2;
                if self.stack[list_index..]
                    .iter()
                    .any(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                // This opcode only ever appends to a comprehension's own
                // accumulator, which nothing outside the comprehension can
                // reach, so appending into its object is unobservable — and it
                // keeps the accumulation linear instead of rebuilding the list
                // on every element.
                match &self.stack[list_index] {
                    Value::Ref(id) if matches!(self.heap.get(*id), Ok(HeapObject::List(_))) => {
                        let target = Value::Ref(*id);
                        let item = self.pop_stack()?;
                        self.heap.push_list(&target, item)?;
                    }
                    Value::List(_) => {
                        let item = self.pop_stack()?;
                        let Some(Value::List(items)) = self.stack.last_mut() else {
                            unreachable!("list append target was checked above");
                        };
                        let values = items.make_mut();
                        if values.len() == values.capacity() {
                            values.reserve(1);
                        }
                        values.push(item);
                    }
                    _ => return Ok(None),
                }
            }
            Instruction::BuildRecord(keys) => {
                let record = self.drain_record_from_stack(keys)?;
                self.stack.push(Value::Record(Arc::new(record)));
            }
            Instruction::BuildHeapRecord(keys) => {
                let record = self.drain_record_from_stack(keys)?;
                self.stack.push(self.heap.allocate_record(record)?);
            }
            Instruction::Field(field) => {
                if self
                    .stack
                    .last()
                    .is_some_and(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let target = self.pop_stack()?;
                let field = self.chunk.names[field].clone();
                let value = self.read_dialect_field(target, &field)?;
                self.stack.push(value);
            }
            Instruction::Index => {
                if self.stack.len() < 2
                    || self.stack[self.stack.len() - 2..]
                        .iter()
                        .any(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let index = self.pop_stack()?;
                let target = self.pop_stack()?;
                let value = self.read_dialect_index(target, index)?;
                self.stack.push(value);
            }
            Instruction::ResultUnwrap => {
                let value = self.pop_stack()?;
                self.stack.push(unwrap_tool_result(value)?);
            }
            Instruction::MakeClosure { function, captures } => {
                let captures = self.pop_n(captures)?;
                let closure = self.heap.allocate_closure(function, captures)?;
                self.stack.push(closure);
            }
            Instruction::Call { argc } => {
                let start = self.stack_drain_start(argc + 1)?;
                let mut values = self.stack.drain(start..).collect::<Vec<_>>();
                let function = values.remove(0);
                let active = self.begin_lashlang_execution(self.current_instruction_ip());
                match self.begin_function_call(function, values, ReturnTarget::Direct) {
                    Ok(()) => {
                        if let Some(active) = &active {
                            self.complete_lashlang_execution(active);
                        }
                    }
                    Err(error) => {
                        if let Some(active) = &active {
                            self.fail_lashlang_execution(active, error.to_string());
                        }
                        return Err(error);
                    }
                }
            }
            Instruction::CallDynamic => self.execute_dynamic_call()?,
            Instruction::Map => {
                let function = self.pop_stack()?;
                let items = self.pop_stack()?;
                let items = self.heap.export_for_instruction(&items)?;
                let items = match items {
                    Value::List(values) | Value::Tuple(values) => values
                        .iter()
                        // This is the one isolation boundary: each callback
                        // borrows its already-independent item.
                        .map(|value| self.heap.isolate_value(value))
                        .collect::<Result<Vec<_>, _>>()?,
                    value => {
                        return Err(RuntimeError::ShapingListRequired {
                            builtin: "map".into(),
                            actual: super::value_type_name(&value).to_string(),
                        });
                    }
                };
                if items.is_empty() {
                    self.stack.push(Value::List(Vec::new().into()));
                } else {
                    self.begin_callback_driver(
                        function,
                        items.into_iter().map(|item| vec![item]).collect(),
                        true,
                        false,
                    )?;
                }
            }
            Instruction::AsyncMap => self.execute_async_map()?,
            Instruction::Return => self.return_from_function()?,
            Instruction::PushHandler {
                handler,
                finally,
                catches,
            } => self.push_exception_handler(handler, finally, catches),
            Instruction::PopHandler => self.pop_exception_handler()?,
            Instruction::EnterFinally { finally, resume } => {
                self.enter_finally(finally, resume);
            }
            Instruction::AbandonFinally => self.abandon_finally()?,
            Instruction::AbandonFinallyKeepValue => self.abandon_finally_keep_value()?,
            Instruction::EndFinally => {
                if let Some(escape) = self.finish_finally()? {
                    // A cleanup chain that ends with nothing catching it
                    // re-raises what it was unwinding. Only a value thrown by
                    // an explicit `throw` becomes an `UncaughtException`; a
                    // routed runtime failure keeps its variant, and its
                    // attribution is restored so the trap still points at the
                    // failing expression rather than the cleanup block.
                    return Err(match escape.origin {
                        Some(origin) => {
                            self.pending_error_span = origin.span.or_else(|| {
                                self.chunk
                                    .spans
                                    .get(origin.instruction_ip)
                                    .copied()
                                    .flatten()
                            });
                            origin.error
                        }
                        None => RuntimeError::UncaughtException {
                            value: self.heap.export(&escape.value)?,
                        },
                    });
                }
            }
            Instruction::Throw => {
                let value = self.pop_stack()?;
                if !self.throw_value(value.clone(), None)? {
                    return Err(RuntimeError::UncaughtException {
                        value: self.heap.export(&value)?,
                    });
                }
            }
            Instruction::Binary(op) => {
                if self.stack.len() < 2
                    || self.stack[self.stack.len() - 2..]
                        .iter()
                        .any(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let right = self.pop_stack()?;
                let left = self.pop_stack()?;
                let value = if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) {
                    let equal = self.heap.structural_eq(&left, &right)?;
                    Value::Bool(if op == BinaryOp::Equal { equal } else { !equal })
                } else {
                    match (left, right) {
                        (Value::Number(left), Value::Number(right)) if op != BinaryOp::In => {
                            eval_number_binary_values(left, op, right)
                        }
                        (left, right) => eval_binary_values(left, op, right)?,
                    }
                };
                self.stack.push(value);
            }
            Instruction::JavaScriptUnary(op) => self.execute_javascript_unary(op)?,
            Instruction::JavaScriptBinary(op) => self.execute_javascript_binary(op)?,
            Instruction::IsNullish => {
                let value = self.pop_stack()?;
                self.stack
                    .push(Value::Bool(matches!(value, Value::Null | Value::Undefined)));
            }
            Instruction::SlotNumberBinary { slot, op, right } => {
                let value = match self.load_slot(slot)? {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        Value::Number(eval_number_numeric_binary_value(*left, op, right))
                    }
                    left => eval_binary_values(left.clone(), op, Value::Number(right))?,
                };
                self.stack.push(value);
            }
            Instruction::SlotNumberCompare { slot, op, right } => {
                let value = match self.load_slot(slot)? {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        Value::Bool(eval_number_compare_values(*left, op, right))
                    }
                    left => {
                        Value::Bool(eval_compare_values(left.clone(), op, Value::Number(right))?)
                    }
                };
                self.stack.push(value);
            }
            Instruction::SlotNumberBinaryCompare {
                slot,
                binary_op,
                binary_right,
                compare_op,
                compare_right,
            } => {
                let truthy = match self.load_slot(slot)? {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        let value =
                            eval_number_numeric_binary_value(*left, binary_op, binary_right);
                        eval_number_compare_values(value, compare_op, compare_right)
                    }
                    left => {
                        let value = eval_binary_values(
                            left.clone(),
                            binary_op,
                            Value::Number(binary_right),
                        )?;
                        eval_compare_values(value, compare_op, Value::Number(compare_right))?
                    }
                };
                self.stack.push(Value::Bool(truthy));
            }
            Instruction::ToBool => {
                if self
                    .stack
                    .last()
                    .is_some_and(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let value = self.pop_stack()?;
                self.stack
                    .push(Value::Bool(self.is_truthy_for_dialect(&value)?));
            }
            Instruction::Jump(target) => self.ip = target,
            Instruction::JumpIfFalse(target) => {
                if self
                    .stack
                    .last()
                    .is_some_and(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let value = self.pop_stack()?;
                if !self.is_truthy_for_dialect(&value)? {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                }
            }
            Instruction::JumpIfCompareFalse { op, target } => {
                if self.stack.len() < 2
                    || self.stack[self.stack.len() - 2..]
                        .iter()
                        .any(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let right = self.pop_stack()?;
                let left = self.pop_stack()?;
                if !eval_compare_values(left, op, right)? {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                }
            }
            Instruction::JumpIfSlotNumberCompareFalse {
                slot,
                op,
                right,
                target,
            } => {
                let truthy = match self.load_slot(slot)? {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => eval_number_compare_values(*left, op, right),
                    value => eval_compare_values(value.clone(), op, Value::Number(right))?,
                };
                if !truthy {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                }
            }
            Instruction::JumpIfSlotNumberBinaryCompareFalse {
                slot,
                binary_op,
                binary_right,
                compare_op,
                compare_right,
                target,
            } => {
                let truthy = match self.load_slot(slot)? {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        let value =
                            eval_number_numeric_binary_value(*left, binary_op, binary_right);
                        eval_number_compare_values(value, compare_op, compare_right)
                    }
                    value => {
                        let value = eval_binary_values(
                            value.clone(),
                            binary_op,
                            Value::Number(binary_right),
                        )?;
                        eval_compare_values(value, compare_op, Value::Number(compare_right))?
                    }
                };
                if !truthy {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                }
            }
            Instruction::JumpIfTrue(target) => {
                if self
                    .stack
                    .last()
                    .is_some_and(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let value = self.pop_stack()?;
                if self.is_truthy_for_dialect(&value)? {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                }
            }
            Instruction::AddAssign(slot) => {
                let right = self.pop_stack()?;
                self.add_assign_value(slot, right)?;
            }
            Instruction::AddAssignNumber { slot, right } => {
                self.add_assign_number(slot, right)?;
            }
            Instruction::AddAssignSlot { slot, right } => {
                self.add_assign_slot(slot, right)?;
            }
            Instruction::AddAssignIndexNumber { slot, right } => {
                let index = self.pop_stack()?;
                if let Some(Value::Ref(id)) = self.slots.get(slot) {
                    let target = Value::Ref(*id);
                    let index = self.heap.export(&index)?;
                    let value = self.heap.add_assign_index_number(&target, &index, right)?;
                    self.record_assignment(slot);
                    self.last_value = Some(value);
                } else {
                    self.add_assign_index_number(slot, &index, right)?;
                }
            }
            Instruction::AddAssignIndexSlotNumber { slot, index, right } => {
                let index = self.load_slot(index)?.clone();
                if let Some(Value::Ref(id)) = self.slots.get(slot) {
                    let target = Value::Ref(*id);
                    let index = self.heap.export(&index)?;
                    let value = self.heap.add_assign_index_number(&target, &index, right)?;
                    self.record_assignment(slot);
                    self.last_value = Some(value);
                } else {
                    self.add_assign_index_number(slot, &index, right)?;
                }
            }
            Instruction::AppendAssign(slot) => self.append_assign(slot)?,
            Instruction::Finish => return Ok(Some(VmStep::Effect(VmEffect::Finish))),
            Instruction::SleepFor => {
                return Ok(Some(VmStep::Effect(VmEffect::Sleep(SleepKind::For))));
            }
            Instruction::SleepUntil => {
                return Ok(Some(VmStep::Effect(VmEffect::Sleep(SleepKind::Until))));
            }
            Instruction::ProcessWaitSignal { name } => {
                if self.mode != VmMode::Process {
                    return Err(RuntimeError::SessionProcessAdminOutsideProcess {
                        keyword: "wait_signal".into(),
                    });
                }
                return Ok(Some(VmStep::Effect(VmEffect::WaitSignal { name })));
            }
            Instruction::ProcessSignalRun { name } => {
                // `signal_run` (sending) is allowed from the foreground turn as
                // well as inside a process body, mirroring `await` / `cancel`.
                // Only `wait_signal` (receiving) is gated to `VmMode::Process`.
                return Ok(Some(VmStep::Effect(VmEffect::SignalRun { name })));
            }
            Instruction::ProcessYield => {
                if self.mode != VmMode::Process {
                    return Err(RuntimeError::SessionProcessAdminOutsideProcess {
                        keyword: "yield".into(),
                    });
                }
                return Ok(Some(VmStep::Effect(VmEffect::ProcessEvent(
                    ProcessEventKind::Yield,
                ))));
            }
            Instruction::ProcessWake => {
                if self.mode != VmMode::Process {
                    return Err(RuntimeError::SessionProcessAdminOutsideProcess {
                        keyword: "wake".into(),
                    });
                }
                return Ok(Some(VmStep::Effect(VmEffect::ProcessEvent(
                    ProcessEventKind::Wake,
                ))));
            }
            Instruction::ProcessFail => {
                if self.mode != VmMode::Process {
                    return Err(RuntimeError::SessionProcessAdminOutsideProcess {
                        keyword: "fail".into(),
                    });
                }
                return Ok(Some(VmStep::Effect(VmEffect::Fail)));
            }
            Instruction::ObserveStep => {
                if let Some(active) = self.begin_lashlang_execution(self.current_instruction_ip()) {
                    self.complete_lashlang_execution(&active);
                }
            }
            Instruction::Pop => {
                self.last_value = Some(self.pop_stack()?);
            }
            Instruction::BeginRangeIter { binding, argc } => {
                let start_index = self.stack_drain_start(argc)?;
                if self.stack[start_index..]
                    .iter()
                    .any(|value| matches!(value, Value::Projected(_)))
                {
                    return Ok(None);
                }
                let (start, end, step) = range_bounds(&self.stack[start_index..])?;
                self.stack.truncate(start_index);
                if range_has_next(start, end, step) {
                    self.slots.ensure_assignable(
                        binding,
                        slot_names_for(self.chunk, self.active_function),
                    )?;
                }
                self.iter_stack.push(IterState {
                    cursor: IterCursor::Range {
                        next: start,
                        end,
                        step,
                    },
                    binding,
                    restore: self.slots.capture_temporary(binding),
                    heapified: false,
                });
            }
            Instruction::IterNext { jump_to } => {
                let Some(iter_state) = self.iter_stack.last_mut() else {
                    return Err(RuntimeError::MissingLoopState);
                };
                let Some(value) = iter_state.cursor.next_value() else {
                    self.ip = jump_to;
                    return Ok(Some(VmStep::Continue));
                };
                self.slots.assign_loop_binding(iter_state.binding, value);
            }
            Instruction::DeepCopyLoopBinding(binding) => self.deep_copy_loop_binding(binding)?,
            Instruction::EndIter => {
                if let Some(iter_state) = self.iter_stack.pop() {
                    self.slots
                        .restore_temporary(iter_state.binding, iter_state.restore);
                }
            }
            _ => return Ok(None),
        }
        Ok(Some(VmStep::Continue))
    }

    /// Re-run `step_instruction_fast` after replacing the two projected stack
    /// operands the opcode consumes with their materialized values.
    ///
    /// `step_instruction_fast` only routes the pure-arithmetic stack opcodes
    /// (`Binary`, `JumpIfCompareFalse`) to the async path when an operand is
    /// `Value::Projected`. Materializing the top two operands in place — in the
    /// same right-then-left order the opcode pops them — and re-dispatching is
    /// exactly equivalent to the inline `materialize_projected_async` the async
    /// arm would have done, but without duplicating the eval logic. The retry
    /// always completes because both operands are now concrete.
    async fn redispatch_with_materialized_stack_pair(
        &mut self,
        instruction: Instruction,
    ) -> Result<VmStep, RuntimeError> {
        let right = materialize_projected_async(self.pop_stack()?).await;
        let left = materialize_projected_async(self.pop_stack()?).await;
        self.stack.push(left);
        self.stack.push(right);
        self.redispatch_fast(instruction)
    }

    /// Re-run `step_instruction_fast` after temporarily resolving a projected
    /// slot operand to its materialized value.
    ///
    /// The fused slot arithmetic opcodes (`SlotNumberBinary`,
    /// `SlotNumberCompare`, `SlotNumberBinaryCompare`,
    /// `JumpIfSlotNumberCompareFalse`, `JumpIfSlotNumberBinaryCompareFalse`)
    /// only read `slot` and never write it, so we materialize the projected
    /// value, swap it into the slot for the (fully synchronous) re-dispatch,
    /// then restore the original projected value. The projected binding is
    /// re-materialized on every touch, matching the old async arm's
    /// per-touch `materialize_projected_async(left.clone())`.
    async fn redispatch_with_materialized_slot(
        &mut self,
        slot: usize,
        instruction: Instruction,
    ) -> Result<VmStep, RuntimeError> {
        let original = self.load_slot(slot)?.clone();
        let materialized = materialize_projected_async(original.clone()).await;
        self.slots.values[slot] = Some(materialized);
        let result = self.redispatch_fast(instruction);
        self.slots.values[slot] = Some(original);
        result
    }

    #[inline(always)]
    fn redispatch_fast(&mut self, instruction: Instruction) -> Result<VmStep, RuntimeError> {
        match self.step_instruction_fast(instruction)? {
            Some(step) => Ok(step),
            None => unreachable!(
                "fast path re-dispatch with resolved operands must complete the opcode"
            ),
        }
    }

    /// Async slow path for the run loop. The synchronous `step_instruction_fast`
    /// fully handles every pure-compute and effect-producing opcode on
    /// non-projected operands; it only yields `Ok(None)` (routing here) when an
    /// operand is `Value::Projected` or the opcode inherently needs a host
    /// `.await` (field/index projected reads, tool/process effects, intrinsics,
    /// type literals).
    ///
    /// The pure-arithmetic opcodes (`Binary`, `JumpIfCompareFalse`, and the
    /// fused `SlotNumber*` / `JumpIfSlotNumber*` ops) do not duplicate the
    /// fast-path eval here: they resolve the blocking projected operand and
    /// re-dispatch through `step_instruction_fast` (see
    /// `redispatch_with_materialized_stack_pair` /
    /// `redispatch_with_materialized_slot`). Only the genuinely-async opcodes
    /// keep bespoke arms — lazy projected field/index propagation, the
    /// `truthy`-hook bool ops, `Unary`, intrinsics, iteration, type literals,
    /// and effects. The opcodes the fast path always completes are unreachable
    /// here.
    async fn step_instruction(&mut self, instruction: Instruction) -> Result<VmStep, RuntimeError> {
        match instruction {
            Instruction::LoadField { slot, field } => {
                let value = self.load_slot(slot)?.clone();
                let field = self.chunk.names[field].clone();
                let value = match value {
                    Value::Projected(projected) => {
                        let parent_name = projected.name().to_string();
                        let inner = projected.get_field(&field).await?;
                        ProjectedValue::propagate_field(&parent_name, &field.text, inner)
                    }
                    value => self.read_dialect_field(value, &field)?,
                };
                self.stack.push(value);
            }
            Instruction::LoadFieldUnwrap { slot, field } => {
                let value = self.load_slot(slot)?.clone();
                let field = self.chunk.names[field].clone();
                let value = match value {
                    Value::Projected(projected) => {
                        let parent_name = projected.name().to_string();
                        let inner = projected.get_field(&field).await?;
                        ProjectedValue::propagate_field(&parent_name, &field.text, inner)
                    }
                    value => self.read_dialect_field(value, &field)?,
                };
                self.stack.push(unwrap_tool_result(value)?);
            }
            Instruction::Field(field) => {
                let target = self.pop_stack()?;
                let field = self.chunk.names[field].clone();
                let value = match target {
                    Value::Projected(projected) => {
                        let parent_name = projected.name().to_string();
                        let inner = projected.get_field(&field).await?;
                        ProjectedValue::propagate_field(&parent_name, &field.text, inner)
                    }
                    target => self.read_dialect_field(target, &field)?,
                };
                self.stack.push(value);
            }
            Instruction::Index => {
                let index = materialize_projected_async(self.pop_stack()?).await;
                let target = self.pop_stack()?;
                let value = match target {
                    Value::Projected(projected) => {
                        let parent_name = projected.name().to_string();
                        let inner = projected.get_index(&index).await?;
                        ProjectedValue::propagate_index(&parent_name, &index, inner)
                    }
                    target => self.read_dialect_index(target, index)?,
                };
                self.stack.push(value);
            }
            Instruction::PathAssign { slot, path } => {
                let value = self.pop_stack()?;
                let last_value = value.clone();
                let path = &self.chunk.assign_paths[path];
                let index_start = self.stack_drain_start(path.dynamic_index_count)?;
                let indexes = &self.stack[index_start..];
                let root_name = &slot_names_for(self.chunk, self.active_function)[slot];
                self.slots
                    .ensure_assignable(slot, slot_names_for(self.chunk, self.active_function))?;
                let root =
                    self.slots
                        .get_mut(slot)
                        .ok_or_else(|| RuntimeError::UndefinedVariable {
                            name: root_name.text.to_string(),
                        })?;
                assign_path(root, path, indexes, value, &self.chunk.names)?;
                self.stack.truncate(index_start);
                self.record_assignment(slot);
                self.last_value = Some(last_value);
            }
            Instruction::HeapPathAssign { slot, path } => {
                self.execute_reference_path_assignment(slot, path)?;
            }
            Instruction::ListAppend => {
                let item = materialize_projected_async(self.pop_stack()?).await;
                let list = self.pop_stack()?;
                let value = match &list {
                    Value::Ref(id) if matches!(self.heap.get(*id), Ok(HeapObject::List(_))) => {
                        self.heap.push_list(&list, item)?
                    }
                    _ => execute_push_builtin_async(list, item).await?,
                };
                self.stack.push(value);
            }
            Instruction::Unary(op) => {
                let value = self.pop_stack()?;
                let value = match op {
                    UnaryOp::Negate => {
                        let value = materialize_projected_async(value).await;
                        Value::Number(-as_number(&value)?)
                    }
                    UnaryOp::Not => Value::Bool(match &value {
                        Value::Projected(_) => !is_truthy_async(&value).await,
                        _ => !self.is_truthy_for_dialect(&value)?,
                    }),
                };
                self.stack.push(value);
            }
            Instruction::Binary(_) => {
                return self
                    .redispatch_with_materialized_stack_pair(instruction)
                    .await;
            }
            Instruction::SlotNumberBinary { slot, .. }
            | Instruction::SlotNumberCompare { slot, .. }
            | Instruction::SlotNumberBinaryCompare { slot, .. } => {
                return self
                    .redispatch_with_materialized_slot(slot, instruction)
                    .await;
            }
            Instruction::ToBool => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy_async(&value).await,
                    _ => self.is_truthy_for_dialect(&value)?,
                };
                self.stack.push(Value::Bool(truthy));
            }
            Instruction::JumpIfFalse(target) => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy_async(&value).await,
                    _ => self.is_truthy_for_dialect(&value)?,
                };
                if !truthy {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                }
            }
            Instruction::JumpIfCompareFalse { .. } => {
                return self
                    .redispatch_with_materialized_stack_pair(instruction)
                    .await;
            }
            Instruction::JumpIfSlotNumberCompareFalse { slot, .. }
            | Instruction::JumpIfSlotNumberBinaryCompareFalse { slot, .. } => {
                return self
                    .redispatch_with_materialized_slot(slot, instruction)
                    .await;
            }
            Instruction::JumpIfTrue(target) => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy_async(&value).await,
                    _ => self.is_truthy_for_dialect(&value)?,
                };
                if truthy {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Then,
                    );
                    self.ip = target;
                } else {
                    self.observe_branch_selection(
                        self.current_instruction_ip(),
                        ProcessBranchSelection::Else,
                    );
                }
            }
            Instruction::ResourceCall { operation, argc } => {
                return Ok(VmStep::Effect(VmEffect::ResourceCall { operation, argc }));
            }
            Instruction::ResourceCallUnwrap { operation, argc } => {
                return Ok(VmStep::Effect(VmEffect::ResourceCallUnwrap {
                    operation,
                    argc,
                }));
            }
            Instruction::ResourceOperationBatch(batch) => {
                return Ok(VmStep::Effect(VmEffect::ResourceOperationBatch(batch)));
            }
            Instruction::StartProcess { process, keys } => {
                return Ok(VmStep::Effect(VmEffect::StartProcess { process, keys }));
            }
            Instruction::AwaitHandle => {
                return Ok(VmStep::Effect(VmEffect::AwaitHandle));
            }
            Instruction::AwaitHandleUnwrap => {
                return Ok(VmStep::Effect(VmEffect::AwaitHandleUnwrap));
            }
            Instruction::CancelHandle => {
                return Ok(VmStep::Effect(VmEffect::CancelHandle));
            }
            Instruction::Intrinsic(op) => {
                self.execute_intrinsic_instruction(op).await?;
            }
            Instruction::Print => {
                if self.mode == VmMode::Process {
                    return Err(RuntimeError::ForegroundControlInsideProcess {
                        keyword: "print".into(),
                    });
                }
                return Ok(VmStep::Effect(VmEffect::Print));
            }
            Instruction::BeginIter(binding) => {
                let iterable = self.pop_stack()?;
                let values = self.iterable_values_for_dialect(iterable).await?;
                if !values.is_empty() {
                    self.slots.ensure_assignable(
                        binding,
                        slot_names_for(self.chunk, self.active_function),
                    )?;
                }
                self.iter_stack.push(IterState {
                    cursor: IterCursor::List { values, index: 0 },
                    binding,
                    restore: self.slots.capture_temporary(binding),
                    heapified: false,
                });
            }
            Instruction::BeginRangeIter { binding, argc } => {
                let start_index = self.stack_drain_start(argc)?;
                let (start, end, step) = range_bounds_async(&self.stack[start_index..]).await?;
                self.stack.truncate(start_index);
                if range_has_next(start, end, step) {
                    self.slots.ensure_assignable(
                        binding,
                        slot_names_for(self.chunk, self.active_function),
                    )?;
                }
                self.iter_stack.push(IterState {
                    cursor: IterCursor::Range {
                        next: start,
                        end,
                        step,
                    },
                    binding,
                    restore: self.slots.capture_temporary(binding),
                    heapified: false,
                });
            }
            Instruction::ResolveTypeRef(slot) => {
                let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
                let value = self.slots.get(slot).cloned().ok_or_else(|| {
                    RuntimeError::UndefinedVariable {
                        name: slot_name.text.to_string(),
                    }
                })?;
                let schema = unwrap_type_value(&value).cloned().ok_or_else(|| {
                    RuntimeError::NotTypeValue {
                        name: slot_name.text.to_string(),
                    }
                })?;
                self.stack.push(schema);
            }
            Instruction::WrapTypeLiteral => {
                let schema = self.pop_stack()?;
                let mut wrapper = record_with_capacity(1);
                wrapper.insert(LASH_TYPE_KEY.to_string(), schema);
                self.stack.push(Value::Record(Arc::new(wrapper)));
            }
            Instruction::WrapHostDescriptor(type_name) => {
                let value = self.pop_stack()?;
                let mut wrapper = record_with_capacity(2);
                wrapper.insert(
                    LASH_HOST_DESCRIPTOR_TYPE_KEY.to_string(),
                    Value::String(self.chunk.names[type_name].text.as_ref().into()),
                );
                wrapper.insert(LASH_HOST_DESCRIPTOR_VALUE_KEY.to_string(), value);
                self.stack.push(Value::Record(Arc::new(wrapper)));
            }
            // Every remaining opcode is fully handled by `step_instruction_fast`
            // for any operand shape (it never returns `Ok(None)` for them), so
            // the run loop never routes them here.
            Instruction::PushConst(_)
            | Instruction::PushNull
            | Instruction::PushUndefined
            | Instruction::PushBool(_)
            | Instruction::PushNumber(_)
            | Instruction::LoadName(_)
            | Instruction::Duplicate
            | Instruction::DeepCopy
            | Instruction::StoreName(_)
            | Instruction::StoreConst { .. }
            | Instruction::BuildTuple(_)
            | Instruction::BuildList(_)
            | Instruction::BuildHeapList(_)
            | Instruction::BuildRecord(_)
            | Instruction::BuildHeapRecord(_)
            | Instruction::ResultUnwrap
            | Instruction::AddAssign(_)
            | Instruction::AddAssignNumber { .. }
            | Instruction::AddAssignSlot { .. }
            | Instruction::AddAssignIndexNumber { .. }
            | Instruction::AddAssignIndexSlotNumber { .. }
            | Instruction::AppendAssign(_)
            | Instruction::Finish
            | Instruction::SleepFor
            | Instruction::SleepUntil
            | Instruction::ProcessWaitSignal { .. }
            | Instruction::ProcessSignalRun { .. }
            | Instruction::ProcessYield
            | Instruction::ProcessWake
            | Instruction::ProcessFail
            | Instruction::ObserveStep
            | Instruction::Pop
            | Instruction::Jump(_)
            | Instruction::IterNext { .. }
            | Instruction::DeepCopyLoopBinding(_)
            | Instruction::EndIter
            | Instruction::MakeClosure { .. }
            | Instruction::Call { .. }
            | Instruction::CallDynamic
            | Instruction::Map
            | Instruction::AsyncMap
            | Instruction::Return
            | Instruction::PushHandler { .. }
            | Instruction::PopHandler
            | Instruction::EnterFinally { .. }
            | Instruction::EndFinally
            | Instruction::JavaScriptUnary(_)
            | Instruction::JavaScriptBinary(_)
            | Instruction::IsNullish
            | Instruction::AbandonFinally
            | Instruction::AbandonFinallyKeepValue
            | Instruction::Throw => {
                unreachable!("opcode is always completed by step_instruction_fast")
            }
        }
        Ok(VmStep::Continue)
    }

    async fn execute_intrinsic_instruction(&mut self, op: IntrinsicOp) -> Result<(), RuntimeError> {
        let start = self.profile.as_ref().map(|_| Instant::now());
        match op {
            IntrinsicOp::JavaScriptSplit => self.execute_javascript_split()?,
            IntrinsicOp::JavaScriptJoin => self.execute_javascript_join()?,
            IntrinsicOp::JavaScriptStdlib(argc) => self.execute_javascript_stdlib(argc)?,
            IntrinsicOp::JavaScriptHeapNew(argc) => self.execute_javascript_heap_new(argc)?,
            IntrinsicOp::JavaScriptHeapInstanceOf => self.execute_javascript_instanceof()?,
            IntrinsicOp::JavaScriptHeapDeleteMember => {
                self.execute_javascript_heap_delete_member()?
            }
            IntrinsicOp::JavaScriptGlobalDelete => self.execute_javascript_global_delete()?,
            IntrinsicOp::JavaScriptGlobalHas => self.execute_javascript_global_has()?,
            IntrinsicOp::JavaScriptGlobalSet => self.execute_javascript_global_set()?,
            IntrinsicOp::JavaScriptUriCodec(codec) => self.execute_javascript_uri_codec(codec)?,
            IntrinsicOp::Validate => {
                let schema = self.pop_stack()?;
                let value = self.pop_stack()?;
                let schema = materialize_projected_async(schema).await;
                let value = self
                    .execute_dynamic_validate(materialize_projected_async(value).await, &schema)?;
                self.stack.push(value);
            }
            IntrinsicOp::ValidateCompiled(schema) => {
                let value = self.pop_stack()?;
                let value = execute_validation_plan(
                    materialize_projected_async(value).await,
                    &self.chunk.compiled_schemas[schema],
                )?;
                self.stack.push(value);
            }
            IntrinsicOp::PushAssign(slot) => {
                let mut item = Some(materialize_projected_async(self.pop_stack()?).await);
                let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
                self.slots
                    .ensure_assignable(slot, slot_names_for(self.chunk, self.active_function))?;
                if let Some(Value::Ref(id)) = self.slots.get(slot) {
                    let target = Value::Ref(*id);
                    let value = self
                        .heap
                        .push_list(&target, item.take().expect("push item should be available"))?;
                    self.record_assignment(slot);
                    self.last_value = Some(value);
                } else {
                    let fast_value = {
                        let current = self.slots.get_mut(slot).ok_or_else(|| {
                            RuntimeError::UndefinedVariable {
                                name: slot_name.text.to_string(),
                            }
                        })?;
                        if let Value::List(items) = current {
                            let values = items.make_mut();
                            if values.len() == values.capacity() {
                                values.reserve(1);
                            }
                            values.push(item.take().expect("push item should be available"));
                            Some(Value::List(items.clone()))
                        } else {
                            None
                        }
                    };
                    if let Some(value) = fast_value {
                        self.record_assignment(slot);
                        self.last_value = Some(value);
                    } else {
                        let item = item.expect("push item should be available");
                        let current = self.slots.get_mut(slot).ok_or_else(|| {
                            RuntimeError::UndefinedVariable {
                                name: slot_name.text.to_string(),
                            }
                        })?;
                        let value = execute_push_builtin_async(current.clone(), item).await?;
                        self.slots.assign(
                            slot,
                            value.clone(),
                            slot_names_for(self.chunk, self.active_function),
                        )?;
                        self.record_assignment(slot);
                        self.last_value = Some(value);
                    }
                }
            }
            IntrinsicOp::FormatCompiled(template) => {
                let template = &self.chunk.format_templates[template];
                let argc = template.argc;
                let values = self.stack_tail(argc)?;
                let value = if values
                    .iter()
                    .any(|value| matches!(value, Value::Projected(_)))
                {
                    execute_compiled_format(template, values).await?
                } else {
                    execute_compiled_format_direct(template, values)?
                };
                self.stack.truncate(self.stack.len() - argc);
                self.stack.push(Value::String(value.into()));
            }
            IntrinsicOp::FormatCompiledSlotNumber { template, slot } => {
                let template = &self.chunk.format_templates[template];
                let value = match self.load_slot(slot)? {
                    Value::Number(value) => Value::String(
                        execute_compiled_format_one_number_compact_direct(template, *value)?,
                    ),
                    value => {
                        let value = if matches!(value, Value::Projected(_)) {
                            execute_compiled_format(template, std::slice::from_ref(value)).await?
                        } else {
                            execute_compiled_format_direct(template, std::slice::from_ref(value))?
                        };
                        Value::String(value.into())
                    }
                };
                self.stack.push(value);
            }
            IntrinsicOp::FormatCompiledSlotNumberBinary {
                template,
                slot,
                op,
                right,
            } => {
                let template = &self.chunk.format_templates[template];
                let value = match self.load_slot(slot)? {
                    Value::Number(left) => {
                        Value::String(execute_compiled_format_one_number_compact_direct(
                            template,
                            eval_number_numeric_binary_value(*left, op, right),
                        )?)
                    }
                    left => {
                        let left = materialize_projected_async(left.clone()).await;
                        let value = eval_binary_values(left, op, Value::Number(right))?;
                        let value = if matches!(value, Value::Projected(_)) {
                            execute_compiled_format(template, &[value]).await?
                        } else {
                            execute_compiled_format_direct(template, &[value])?
                        };
                        Value::String(value.into())
                    }
                };
                self.stack.push(value);
            }
            _ => {
                let argc =
                    op.fixed_argc()
                        .ok_or(RuntimeError::ContextDependentIntrinsicMisdispatch {
                            context: "intrinsic dispatch".into(),
                        })?;
                let start = self
                    .stack
                    .len()
                    .checked_sub(argc)
                    .ok_or(RuntimeError::VmStackUnderflow)?;
                let values = &self.stack[start..];
                let value = execute_intrinsic(
                    op,
                    &self.chunk.names,
                    values,
                    &mut self.instructions_executed,
                )
                .await?;
                self.stack.truncate(self.stack.len() - argc);
                self.stack.push(value);
            }
        }
        if let Some(start) = start {
            self.record_builtin_profile(op, start.elapsed().as_nanos());
        }
        Ok(())
    }

    fn pop_stack(&mut self) -> Result<Value, RuntimeError> {
        self.stack.pop().ok_or(RuntimeError::VmStackUnderflow)
    }

    fn load_slot(&self, slot: usize) -> Result<&Value, RuntimeError> {
        self.slots
            .get(slot)
            .ok_or_else(|| RuntimeError::UndefinedVariable {
                name: slot_names_for(self.chunk, self.active_function)[slot]
                    .text
                    .to_string(),
            })
    }

    fn drain_record_from_stack(&mut self, keys: usize) -> Result<Record, RuntimeError> {
        let key_indices = &self.chunk.key_lists[keys];
        let start = self.stack_drain_start(key_indices.len())?;
        let mut record = record_with_capacity(key_indices.len());
        for (key, value) in key_indices.iter().zip(self.stack.drain(start..)) {
            let name_entry = &self.chunk.names[*key];
            record.insert_symbolized(name_entry.symbol, name_entry.text.clone(), value);
        }
        Ok(record)
    }

    fn drain_receiver_call(&mut self, argc: usize) -> Result<(Value, Vec<Value>), RuntimeError> {
        let start = self.stack_drain_start(argc + 1)?;
        let mut values = self.stack.drain(start..).collect::<Vec<_>>();
        let receiver = values.remove(0);
        Ok((receiver, values))
    }

    fn pop_n(&mut self, len: usize) -> Result<Vec<Value>, RuntimeError> {
        if self.stack.len() < len {
            return Err(RuntimeError::VmStackUnderflow);
        }
        let start = self.stack.len() - len;
        Ok(self.stack.split_off(start))
    }

    fn stack_tail(&self, len: usize) -> Result<&[Value], RuntimeError> {
        if self.stack.len() < len {
            return Err(RuntimeError::VmStackUnderflow);
        }
        Ok(&self.stack[self.stack.len() - len..])
    }

    fn stack_drain_start(&self, len: usize) -> Result<usize, RuntimeError> {
        self.stack
            .len()
            .checked_sub(len)
            .ok_or(RuntimeError::VmStackUnderflow)
    }

    fn unwind_iterators(&mut self) {
        while let Some(iter_state) = self.iter_stack.pop() {
            self.slots
                .restore_temporary(iter_state.binding, iter_state.restore);
        }
    }

    fn record_assignment(&mut self, slot: usize) {
        if self.active_function.is_some() {
            return;
        }
        let name = &self.chunk.slot_names[slot].text;
        if !self.assigned_globals.contains(name.as_ref()) {
            self.assigned_globals.insert(name.to_string());
        }
    }

    /// Materializes host-visible globals, omitting any entire binding that
    /// contains a function value at any depth.
    pub fn into_globals(mut self) -> Result<Record, RuntimeError> {
        let runtime_globals = self.slots.into_globals(&self.chunk.slot_names);
        super::state::materialize_runtime_globals(&runtime_globals, &mut self.heap)
    }

    pub(crate) fn into_state_parts(self) -> Result<(Record, Heap), RuntimeError> {
        let globals = self.slots.into_globals(&self.chunk.slot_names);
        Ok((globals, self.heap))
    }

    pub(crate) fn recycle_into_state_parts(
        mut self,
        scratch: &mut ExecutionScratch,
    ) -> Result<(Record, Heap), RuntimeError> {
        self.stack.clear();
        self.iter_stack.clear();
        scratch.stack = std::mem::take(&mut self.stack);
        scratch.iter_stack = std::mem::take(&mut self.iter_stack);
        scratch.assigned_globals = std::mem::take(&mut self.assigned_globals);
        let globals = self
            .slots
            .recycle_into_globals(&self.chunk.slot_names, &mut scratch.slot_values);
        Ok((globals, self.heap))
    }

    fn record_instruction_profile(&mut self, tag: InstructionProfileTag, elapsed_ns: u128) {
        let Some(profile) = &mut self.profile else {
            return;
        };
        let index = tag as usize;
        profile.instruction_counts[index] += 1;
        profile.instruction_times[index] += elapsed_ns;
    }

    fn record_builtin_profile(&mut self, builtin: IntrinsicOp, elapsed_ns: u128) {
        let Some(profile) = &mut self.profile else {
            return;
        };
        let index = builtin.profile_tag() as usize;
        profile.builtin_counts[index] += 1;
        profile.builtin_times[index] += elapsed_ns;
    }

    pub(crate) fn take_profile(&mut self) -> ProfileReport {
        let Some(profile) = self.profile.take() else {
            return ProfileReport::default();
        };
        profile.finish()
    }
}
include!("functions.rs");
include!("assignment.rs");
include!("iteration.rs");
include!("observations.rs");
include!("roots.rs");
include!("state_boundary.rs");
