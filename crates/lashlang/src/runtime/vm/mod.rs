//! Bytecode executor for compiled chunks, host effects, and trace/profile data.

use std::sync::Arc;
use std::time::Instant;

use crate::ast::{BinaryOp, JavaScriptBinaryOp, JavaScriptUnaryOp, UnaryOp};
use crate::span::Span;
use crate::{LashlangExecutionObservation, LashlangExecutionSite, ProcessBranchSelection};
use rustc_hash::FxHashMap;

mod builtin_functions;
mod continuation;
mod control;
mod effects;
mod exceptions;
mod heap_plan;
mod javascript;
mod javascript_array;
mod javascript_codec;
pub(crate) mod javascript_date;
mod javascript_json;
mod javascript_number;
mod javascript_operators;
pub(crate) mod javascript_regexp;
mod javascript_static;
mod javascript_stdlib;
mod javascript_string_regexp;
mod javascript_substrate;
mod javascript_url;
mod pending_tools;
mod projected_paths;
mod reference_assignment;

#[cfg(test)]
use continuation::TestSuspension;
pub use continuation::VM_CONTINUATION_FORMAT_VERSION;
pub use continuation::{
    ContinuationError, VmContinuation, VmFinallyCompletionContinuation, VmFinallyContinuation,
    VmHandlerContinuation, VmHeapContinuation, VmIteratorContinuation, VmIteratorCursor,
    VmPendingErrorOriginContinuation, VmProfileContinuation, VmRunOutcome,
};
pub(crate) use continuation::{VmFrameContinuation, VmFrameReturnContinuation};
use control::{VmMode, VmStep};
use effects::VmEffect;
use exceptions::{ExceptionHandler, FinallyCompletion, FinallyState};
pub use javascript_regexp::{
    TYPESCRIPT_REGEXP_EXECUTION_FUEL, TYPESCRIPT_REGEXP_FUEL_PER_INSTRUCTION,
    TYPESCRIPT_REGEXP_MAX_NESTING, TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS,
    TypeScriptRegExpValidationError, validate_typescript_regexp, validate_typescript_regexp_shape,
};

use super::heap::same_value_zero;
use super::host::{ExecutionMode, ProcessEventKind, SleepKind};
use super::record::{Record, record_with_capacity};
use super::schema::{
    ValidationPlan, compile_schema_value, execute_validate_builtin, execute_validation_plan,
};
use super::value::ProjectedValue;
use super::{
    BuiltinFunction, Chunk, ClosureParameterModel, CompiledProgram,
    DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, ExecutionHost, ExecutionOutcome, ExecutionScratch, Heap,
    HeapId, HeapObject, ImageValue, Instruction, InstructionProfileTag, IntrinsicOp,
    LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY, LASH_TYPE_KEY, ListValue, Name,
    PersistedRoots, ProfileAccumulator, ProfileReport, ProjectedBindings, RegExpMatchObject,
    ResourceHandle, RuntimeError, State, Value, add_assign_index_number, add_values, as_number,
    assign_path, binary_op_work_units, charge_collection_work, deep_proportional_units,
    eval_binary_values, eval_compare_values, eval_javascript_binary, eval_javascript_unary,
    eval_number_binary_values, eval_number_compare_values, eval_number_numeric_binary_value,
    execute_compiled_format, execute_compiled_format_direct,
    execute_compiled_format_one_number_compact_direct, execute_intrinsic,
    execute_push_builtin_async, heap_inherited_builtin, inline_inherited_builtin, is_truthy,
    is_truthy_async, iterable_values, javascript_join, javascript_split,
    materialize_projected_async, materialize_value, proportional_units, range_bounds,
    range_bounds_async, read_javascript_field_direct, read_javascript_heap_field,
    read_javascript_heap_index, read_javascript_index_direct_with_key, regexp_string, sorting_work,
    unwrap_tool_result, unwrap_type_value,
};

#[derive(Clone)]
pub(crate) struct SlotState {
    values: Vec<Option<Value>>,
    extras: Record,
    /// Written once, when the VM is built or restored, and read by `heapify_vm_state` — it
    /// lives here so the flag always describes the record it travels with, and the frame swap
    /// carries it for free.
    extras_heapified: bool,
}

impl SlotState {
    /// `values` is the buffer to build the slot table in — `Vec::new()` on a
    /// cold start, or the recycled `ExecutionScratch::slot_values` buffer.
    ///
    /// A private slot (`private_slots`) starts empty: the session's globals
    /// never reach a front end's own slot.
    pub(crate) fn from_globals(
        mut globals: Record,
        slot_names: &[Name],
        private_slots: &[bool],
        projected_bindings: &ProjectedBindings,
        values: Vec<Option<Value>>,
    ) -> Self {
        let mut values = values;
        values.clear();
        if values.capacity() < slot_names.len() {
            values.reserve(slot_names.len() - values.capacity());
        }
        for (index, name) in slot_names.iter().enumerate() {
            if private_slots.get(index).copied().unwrap_or(false) {
                values.push(None);
            } else if let Some(value) = projected_bindings.get_symbol(name.symbol) {
                globals.remove_symbol(name.symbol);
                values.push(Some(Value::Projected(value)));
            } else {
                values.push(globals.remove_symbol(name.symbol));
            }
        }
        Self {
            values,
            extras: globals,
            extras_heapified: false,
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
        projected_bindings: Option<&ProjectedBindings>,
    ) -> Result<(), RuntimeError> {
        self.ensure_assignable(slot, slot_names, projected_bindings)?;
        self.values[slot] = Some(materialize_value(value)?);
        Ok(())
    }

    fn assign_loop_binding(&mut self, slot: usize, value: Value) -> Result<(), RuntimeError> {
        self.values[slot] = Some(materialize_value(value)?);
        Ok(())
    }

    /// `projected_bindings` is the host's declaration, passed only when `self`
    /// is the root slot state — a function frame's locals are never projected
    /// bindings, and their names may collide with one.
    fn ensure_assignable(
        &self,
        slot: usize,
        slot_names: &[Name],
        projected_bindings: Option<&ProjectedBindings>,
    ) -> Result<(), RuntimeError> {
        let is_binding = projected_bindings
            .zip(slot_names.get(slot))
            .is_some_and(|(bindings, name)| bindings.get_symbol(name.symbol).is_some());
        if is_binding {
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

    /// `reclaim` receives the drained values buffer for recycling —
    /// `ExecutionScratch::slot_values` when the caller reuses scratch.
    ///
    /// `projected_bindings` is the host's declaration — a slot whose name is
    /// a projected binding is read-only and leaves no global behind, while a
    /// slot that merely holds a projected *value* materializes into globals
    /// like any other binding (FIG-2865 lets the two coexist).
    ///
    /// A private slot (`private_slots`) is dropped: a front end's own slot
    /// never becomes a session global.
    pub(crate) fn into_globals(
        self,
        slot_names: &[Name],
        private_slots: &[bool],
        projected_bindings: &ProjectedBindings,
        reclaim: Option<&mut Vec<Option<Value>>>,
    ) -> Result<Record, RuntimeError> {
        let mut extras = self.extras;
        let mut values = self.values;
        for (index, (name, value)) in slot_names.iter().zip(values.iter_mut()).enumerate() {
            if private_slots.get(index).copied().unwrap_or(false) {
                value.take();
                continue;
            }
            if projected_bindings.get_symbol(name.symbol).is_some() {
                extras.remove_symbol(name.symbol);
                continue;
            }
            match value.take() {
                Some(value) => {
                    extras.insert_symbolized(
                        name.symbol,
                        name.text.clone(),
                        materialize_value(value)?,
                    );
                }
                None => {
                    extras.remove_symbol(name.symbol);
                }
            }
        }
        if let Some(reclaim) = reclaim {
            values.clear();
            *reclaim = values;
        }
        Ok(extras)
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
    /// The most recently released frame's slot state, kept for the next call.
    /// A callback-driven loop (`Array.map`, `Map`/`Set.forEach`, async map)
    /// otherwise allocates a fresh values vector per visited element. One slot
    /// is all such a loop needs: every element returns before the next one is
    /// called.
    slot_scratch: Option<SlotState>,
    /// The answers each instruction suspended for a guest `valueOf`/`toString`
    /// has collected, innermost last (FIG-3652). Empty outside a coercion.
    guest_coercions: Vec<GuestCoercionLog>,
    /// The host's projected-binding declaration, captured once at build or
    /// resume — the same map `SlotState::from_globals` and
    /// `refresh_projected` seed from. A root slot whose name is in it is
    /// read-only; nothing per-slot duplicates that.
    projected_bindings: ProjectedBindings,
    handlers: Vec<ExceptionHandler>,
    finally_stack: Vec<FinallyState>,
    lashlang_execution_occurrences: FxHashMap<String, u64>,
    profile: Option<ProfileAccumulator>,
    validation_plans: FxHashMap<usize, (Arc<Record>, ValidationPlan)>,
    pending_error_span: Option<Span>,
    instructions_executed: u64,
    pub(crate) heap: Heap,
    heap_initialized: bool,
    pending_tools: std::collections::BTreeMap<lash_sansio::handle::HandleId, Option<Value>>,
    /// Identity of this execution, stamped into every pending-tool handle it
    /// mints and required back at await, so a handle kept from an earlier
    /// execution (or written by hand) cannot alias this execution's requests.
    /// Restored with the continuation: a resumed process is the same execution.
    execution_nonce: u64,
    #[cfg(test)]
    test_suspension: TestSuspension,
    /// How many post-instruction import passes ran (FIG-3730): the law in
    /// `control.rs` holds a plain-instruction loop to a constant count.
    #[cfg(test)]
    heapify_passes: u64,
}

#[derive(Clone)]
pub(super) struct ActiveLashlangExecutionNode {
    pub(super) site: LashlangExecutionSite,
    pub(super) occurrence: u64,
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
            Instruction::StoreName(name) => {
                let value = self.pop_stack()?;
                self.slots.assign(
                    name,
                    value.clone(),
                    slot_names_for(self.chunk, self.active_function),
                    self.active_function
                        .is_none()
                        .then_some(&self.projected_bindings),
                )?;
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
                let definition =
                    self.chunk
                        .functions
                        .get(function)
                        .ok_or(RuntimeError::UnknownFunction {
                            index: function as u32,
                        })?;
                let closure = self.heap.allocate_closure(
                    function,
                    captures,
                    &definition.js_name,
                    definition.expected_argument_count(),
                )?;
                self.stack.push(closure);
            }
            Instruction::Call { argc } | Instruction::CallMethod { argc } => {
                let with_receiver = matches!(instruction, Instruction::CallMethod { .. });
                let start = self.stack_drain_start(argc + 1 + usize::from(with_receiver))?;
                let mut values = self.stack.drain(start..).collect::<Vec<_>>();
                let receiver = if with_receiver {
                    values.remove(0)
                } else {
                    Value::Undefined
                };
                let function = values.remove(0);
                let active = self.begin_lashlang_call(self.current_instruction_ip());
                match self.begin_function_call(
                    function,
                    receiver,
                    CallArguments::Owned(values),
                    ReturnTarget::Direct,
                ) {
                    Ok(()) => {
                        if let Some(active) = &active {
                            self.complete_lashlang_execution(active);
                        }
                    }
                    Err(error) => {
                        if let Some(active) = &active {
                            self.fail_lashlang_execution(active, &error);
                        }
                        return Err(error);
                    }
                }
            }
            Instruction::CallDynamic => self.execute_dynamic_call(false)?,
            Instruction::CallMethodDynamic => self.execute_dynamic_call(true)?,
            Instruction::Map => {
                let function = self.pop_stack()?;
                let items = self.pop_stack()?;
                let items = match &items {
                    Value::Ref(id) => match self.heap.get(*id)? {
                        HeapObject::List(values) | HeapObject::Tuple(values) => values.clone(),
                        HeapObject::RegExpMatch(result) => result.items.clone(),
                        object => {
                            return Err(RuntimeError::ShapingListRequired {
                                builtin: "map".into(),
                                actual: object.kind_name().to_string(),
                            });
                        }
                    },
                    Value::List(values) | Value::Tuple(values) => values.to_vec(),
                    value => {
                        let exported = self.heap.export_for_instruction(value)?;
                        // Exporting reads the value's whole graph once.
                        self.charge_intrinsic_work(deep_proportional_units(&exported));
                        let (Value::List(values) | Value::Tuple(values)) = exported else {
                            return Err(RuntimeError::ShapingListRequired {
                                builtin: "map".into(),
                                actual: super::value_type_name(value).to_string(),
                            });
                        };
                        values.to_vec()
                    }
                };
                // Each callback receives the item itself: a heap
                // reference stays the object it names (ECMA identity), and
                // only an inline compound is given its own object.
                let mut staged = 0usize;
                let mut isolated_items = Vec::with_capacity(items.len());
                for value in &items {
                    match value {
                        Value::Ref(_) => isolated_items.push(value.clone()),
                        value => {
                            let (value, work) = self.heap.isolate_value_with_work(value)?;
                            staged = staged.saturating_add(work);
                            isolated_items.push(value);
                        }
                    }
                }
                // Each isolation copy walks the graph it reaches once.
                self.charge_intrinsic_work(staged);
                let items = isolated_items;
                if items.is_empty() {
                    self.stack.push(Value::List(Vec::new().into()));
                } else {
                    // The driver queues one call per element.
                    self.charge_intrinsic_work(items.len());
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
                    let (equal, work) = self.heap.structural_eq_with_work(&left, &right)?;
                    self.charge_intrinsic_work(work);
                    Value::Bool(if op == BinaryOp::Equal { equal } else { !equal })
                } else {
                    match (left, right) {
                        (Value::Number(left), Value::Number(right)) if op != BinaryOp::In => {
                            eval_number_binary_values(left, op, right)
                        }
                        (left, right) => {
                            // `+` copies its result, `in` scans the haystack:
                            // the operand sizes are the work.
                            self.charge_intrinsic_work(binary_op_work_units(&left, op, &right));
                            eval_binary_values(left, op, right)?
                        }
                    }
                };
                self.stack.push(value);
            }
            Instruction::JavaScriptUnary(op) => {
                if self.javascript_unary_needs_async(op)? {
                    return Ok(None);
                }
                self.execute_javascript_unary(op)?;
            }
            Instruction::JavaScriptBinary(op) => {
                if self.javascript_binary_needs_async(op)? {
                    return Ok(None);
                }
                self.execute_javascript_binary(op)?;
            }
            // `a ?? b` asks whether the *value* is absent, and a projected handle
            // is a host-side view of one, so testing the wrapper made every
            // projected binding look present. `ProjectedValue::is_nullish` settles
            // it without asking the host — see there for why reading would be both
            // wrong and expensive. Only the duplicate the `??` lowering pushes is
            // consumed, so a present projection stays projected in the result.
            Instruction::IsNullish => {
                let nullish = match self.pop_stack()? {
                    Value::Projected(projected) => projected.is_nullish(),
                    value => matches!(value, Value::Null | Value::Undefined),
                };
                self.stack.push(Value::Bool(nullish));
            }
            Instruction::SlotNumberBinary { slot, op, right } => {
                let value = match self.load_slot(slot)?.clone() {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        Value::Number(eval_number_numeric_binary_value(left, op, right))
                    }
                    left => {
                        self.charge_intrinsic_work(binary_op_work_units(
                            &left,
                            op,
                            &Value::Number(right),
                        ));
                        eval_binary_values(left, op, Value::Number(right))?
                    }
                };
                self.stack.push(value);
            }
            Instruction::SlotNumberCompare { slot, op, right } => {
                let value = match self.load_slot(slot)?.clone() {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => Value::Bool(eval_number_compare_values(left, op, right)),
                    left => {
                        self.charge_intrinsic_work(binary_op_work_units(
                            &left,
                            op,
                            &Value::Number(right),
                        ));
                        Value::Bool(eval_compare_values(left, op, Value::Number(right))?)
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
                let truthy = match self.load_slot(slot)?.clone() {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        let value = eval_number_numeric_binary_value(left, binary_op, binary_right);
                        eval_number_compare_values(value, compare_op, compare_right)
                    }
                    left => {
                        self.charge_intrinsic_work(binary_op_work_units(
                            &left,
                            binary_op,
                            &Value::Number(binary_right),
                        ));
                        let value =
                            eval_binary_values(left, binary_op, Value::Number(binary_right))?;
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
                self.charge_intrinsic_work(binary_op_work_units(&left, op, &right));
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
                let truthy = match self.load_slot(slot)?.clone() {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => eval_number_compare_values(left, op, right),
                    value => {
                        self.charge_intrinsic_work(binary_op_work_units(
                            &value,
                            op,
                            &Value::Number(right),
                        ));
                        eval_compare_values(value, op, Value::Number(right))?
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
            Instruction::JumpIfSlotNumberBinaryCompareFalse {
                slot,
                binary_op,
                binary_right,
                compare_op,
                compare_right,
                target,
            } => {
                let truthy = match self.load_slot(slot)?.clone() {
                    Value::Projected(_) => return Ok(None),
                    Value::Number(left) => {
                        let value = eval_number_numeric_binary_value(left, binary_op, binary_right);
                        eval_number_compare_values(value, compare_op, compare_right)
                    }
                    value => {
                        self.charge_intrinsic_work(binary_op_work_units(
                            &value,
                            binary_op,
                            &Value::Number(binary_right),
                        ));
                        let value =
                            eval_binary_values(value, binary_op, Value::Number(binary_right))?;
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
            Instruction::JavaScriptAddAssign(slot) => {
                if self.javascript_binary_needs_async(JavaScriptBinaryOp::Add)? {
                    return Ok(None);
                }
                self.javascript_add_assign(slot)?;
            }
            Instruction::AddAssignIndexNumber { slot, right } => {
                let index = self.pop_stack()?;
                if let Some(Value::Ref(id)) = self.slots.get(slot) {
                    let target = Value::Ref(*id);
                    let index = self.heap.export(&index)?;
                    let value = self.heap.add_assign_index_number(&target, &index, right)?;
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
            Instruction::ProcessFail => {
                if self.mode != VmMode::Process {
                    return Err(RuntimeError::SessionProcessAdminOutsideProcess {
                        keyword: "fail".into(),
                    });
                }
                return Ok(Some(VmStep::Effect(VmEffect::Fail)));
            }
            Instruction::ObserveStep => {
                self.observe_lashlang_execution_step(self.current_instruction_ip());
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
                        self.active_projected_bindings(),
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
                let binding = iter_state.binding;
                let mut work = 0usize;
                let next = iter_state.cursor.next_value(&self.heap, &mut work)?;
                self.charge_intrinsic_work(work);
                let Some(value) = next else {
                    self.ip = jump_to;
                    return Ok(Some(VmStep::Continue));
                };
                self.slots.assign_loop_binding(binding, value)?;
            }
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
        let right = materialize_projected_async(self.pop_stack()?).await?;
        let left = materialize_projected_async(self.pop_stack()?).await?;
        let op = match &instruction {
            Instruction::JavaScriptBinary(op) => Some(*op),
            Instruction::JavaScriptAddAssign(_) => Some(JavaScriptBinaryOp::Add),
            _ => None,
        };
        let (left, right) = match op {
            Some(op) => {
                self.prepare_javascript_binary_operands(op, left, right)
                    .await?
            }
            None => (left, right),
        };
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
        let materialized = materialize_projected_async(original.clone()).await?;
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
                        self.read_projected_field(&projected, &field).await?
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
                        self.read_projected_field(&projected, &field).await?
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
                        self.read_projected_field(&projected, &field).await?
                    }
                    target => self.read_dialect_field(target, &field)?,
                };
                self.stack.push(value);
            }
            Instruction::Index => {
                let index = materialize_projected_async(self.pop_stack()?).await?;
                let target = self.pop_stack()?;
                let value = match target {
                    Value::Projected(projected) => {
                        self.read_projected_index(&projected, &index).await?
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
                self.slots.ensure_assignable(
                    slot,
                    slot_names_for(self.chunk, self.active_function),
                    self.active_projected_bindings(),
                )?;
                let root =
                    self.slots
                        .get_mut(slot)
                        .ok_or_else(|| RuntimeError::UndefinedVariable {
                            name: root_name.text.to_string(),
                        })?;
                assign_path(root, path, indexes, value, &self.chunk.names)?;
                self.stack.truncate(index_start);
                self.last_value = Some(last_value);
            }
            Instruction::HeapPathAssign { slot, path } => {
                self.execute_reference_path_assignment(slot, path)?;
            }
            Instruction::ListAppend => {
                let item = materialize_projected_async(self.pop_stack()?).await?;
                let list = self.pop_stack()?;
                let value = match &list {
                    Value::Ref(id) if matches!(self.heap.get(*id), Ok(HeapObject::List(_))) => {
                        self.heap.push_list(&list, item)?
                    }
                    _ => {
                        // The fallback copies every element the list holds.
                        self.charge_intrinsic_work(proportional_units(&list).saturating_add(1));
                        execute_push_builtin_async(list, item).await?
                    }
                };
                self.stack.push(value);
            }
            Instruction::Unary(op) => {
                let value = self.pop_stack()?;
                let value = match op {
                    UnaryOp::Negate => {
                        let value = materialize_projected_async(value).await?;
                        Value::Number(-as_number(&value)?)
                    }
                    UnaryOp::Not => Value::Bool(match &value {
                        Value::Projected(_) => !is_truthy_async(&value).await?,
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
            Instruction::JavaScriptUnary(op) => {
                return self.redispatch_javascript_unary(op).await;
            }
            Instruction::JavaScriptBinary(_) => {
                return self
                    .redispatch_with_materialized_stack_pair(instruction)
                    .await;
            }
            Instruction::JavaScriptAddAssign(_) => {
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
                    Value::Projected(_) => is_truthy_async(&value).await?,
                    _ => self.is_truthy_for_dialect(&value)?,
                };
                self.stack.push(Value::Bool(truthy));
            }
            Instruction::JumpIfFalse(target) => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy_async(&value).await?,
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
                    Value::Projected(_) => is_truthy_async(&value).await?,
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
            Instruction::PendingTool { operation, argc } => {
                self.create_pending_tool(operation, argc)?;
            }
            Instruction::PendingTimer => {
                self.create_pending_timer()?;
            }
            Instruction::AwaitArray { consumer } => {
                return Ok(VmStep::Effect(VmEffect::AwaitArray { consumer }));
            }
            Instruction::AwaitPending => return Ok(VmStep::Effect(VmEffect::AwaitPending)),
            Instruction::ResourceOperationBatch(batch) => {
                return Ok(VmStep::Effect(VmEffect::ResourceOperationBatch(batch)));
            }
            Instruction::ResourceOperationListBatch(batch) => {
                return Ok(VmStep::Effect(VmEffect::ResourceOperationListBatch(batch)));
            }
            Instruction::AwaitHandle => {
                return Ok(VmStep::Effect(VmEffect::AwaitHandle));
            }
            Instruction::AwaitHandleUnwrap => {
                return Ok(VmStep::Effect(VmEffect::AwaitHandleUnwrap));
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
                let cursor = self.iteration_cursor(iterable).await?;
                if cursor.has_next(&self.heap)? {
                    self.slots.ensure_assignable(
                        binding,
                        slot_names_for(self.chunk, self.active_function),
                        self.active_projected_bindings(),
                    )?;
                }
                self.iter_stack.push(IterState {
                    cursor,
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
                        self.active_projected_bindings(),
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
            | Instruction::StoreName(_)
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
            | Instruction::ProcessYield
            | Instruction::ProcessFail
            | Instruction::ObserveStep
            | Instruction::Pop
            | Instruction::Jump(_)
            | Instruction::IterNext { .. }
            | Instruction::EndIter
            | Instruction::MakeClosure { .. }
            | Instruction::Call { .. }
            | Instruction::CallMethod { .. }
            | Instruction::CallMethodDynamic
            | Instruction::CallDynamic
            | Instruction::Map
            | Instruction::AsyncMap
            | Instruction::Return
            | Instruction::PushHandler { .. }
            | Instruction::PopHandler
            | Instruction::EnterFinally { .. }
            | Instruction::EndFinally
            | Instruction::IsNullish
            | Instruction::AbandonFinally
            | Instruction::AbandonFinallyKeepValue
            | Instruction::Throw => {
                unreachable!("opcode is always completed by step_instruction_fast")
            }
        }
        Ok(VmStep::Continue)
    }

    #[expect(
        clippy::expect_used,
        reason = "the push item was reserved in this same slot walk above, per the thrice-repeated message"
    )]
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
            IntrinsicOp::JavaScriptRegExp(argc) => self.execute_javascript_regexp(argc)?,
            IntrinsicOp::JavaScriptGlobalDelete => self.execute_javascript_global_delete()?,
            IntrinsicOp::JavaScriptGlobalGet => self.execute_javascript_global_get()?,
            IntrinsicOp::JavaScriptGlobalHas => self.execute_javascript_global_has()?,
            IntrinsicOp::JavaScriptGlobalSet => self.execute_javascript_global_set()?,
            IntrinsicOp::BindingCellNew
            | IntrinsicOp::BindingCellGet
            | IntrinsicOp::BindingCellSet => self.execute_binding_cell(op)?,
            IntrinsicOp::JavaScriptUriCodec(codec) => self.execute_javascript_uri_codec(codec)?,
            IntrinsicOp::Validate => {
                let schema = self.pop_stack()?;
                let value = self.pop_stack()?;
                let schema = materialize_projected_async(schema).await?;
                let value = materialize_projected_async(value).await?;
                // A validation walks the schema and the value's members once.
                self.charge_intrinsic_work(
                    deep_proportional_units(&value)
                        .saturating_add(deep_proportional_units(&schema)),
                );
                let value = self.execute_dynamic_validate(value, &schema)?;
                self.stack.push(value);
            }
            IntrinsicOp::ValidateCompiled(schema) => {
                let value = self.pop_stack()?;
                let value = materialize_projected_async(value).await?;
                // A validation walks the schema and the value's members once;
                // the compiled schema's size is fixed at compile time.
                self.charge_intrinsic_work(deep_proportional_units(&value));
                let value = execute_validation_plan(value, &self.chunk.compiled_schemas[schema])?;
                self.stack.push(value);
            }
            IntrinsicOp::PushAssign(slot) => {
                let mut item = Some(materialize_projected_async(self.pop_stack()?).await?);
                let slot_name = &slot_names_for(self.chunk, self.active_function)[slot];
                self.slots.ensure_assignable(
                    slot,
                    slot_names_for(self.chunk, self.active_function),
                    self.active_projected_bindings(),
                )?;
                if let Some(Value::Ref(id)) = self.slots.get(slot) {
                    let target = Value::Ref(*id);
                    let value = self
                        .heap
                        .push_list(&target, item.take().expect("push item should be available"))?;
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
                        self.last_value = Some(value);
                    } else {
                        let item = item.expect("push item should be available");
                        // The fallback copies every element the list holds.
                        self.charge_intrinsic_work(
                            self.slots.get(slot).map_or(0, proportional_units),
                        );
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
                            self.active_function
                                .is_none()
                                .then_some(&self.projected_bindings),
                        )?;
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
                // Rendering writes each output byte once.
                self.charge_intrinsic_work(value.len());
                self.stack.push(Value::String(value.into()));
            }
            IntrinsicOp::FormatCompiledSlotNumber { template, slot } => {
                let template = &self.chunk.format_templates[template];
                let value = match self.load_slot(slot)? {
                    Value::Number(value) => Value::String(
                        execute_compiled_format_one_number_compact_direct(template, *value)?.into(),
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
                if let Value::String(text) = &value {
                    self.charge_intrinsic_work(text.len());
                }
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
                    Value::Number(left) => Value::String(
                        execute_compiled_format_one_number_compact_direct(
                            template,
                            eval_number_numeric_binary_value(*left, op, right),
                        )?
                        .into(),
                    ),
                    left => {
                        let left = materialize_projected_async(left.clone()).await?;
                        self.charge_intrinsic_work(binary_op_work_units(
                            &left,
                            op,
                            &Value::Number(right),
                        ));
                        let value = eval_binary_values(left, op, Value::Number(right))?;
                        let value = if matches!(value, Value::Projected(_)) {
                            execute_compiled_format(template, &[value]).await?
                        } else {
                            execute_compiled_format_direct(template, &[value])?
                        };
                        Value::String(value.into())
                    }
                };
                if let Value::String(text) = &value {
                    self.charge_intrinsic_work(text.len());
                }
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
}
mod functions;
use functions::*;
mod guest_coercion;
use guest_coercion::*;
mod assignment;
mod iteration;
mod projected_restore;
pub(crate) use iteration::*;
mod observations;
mod roots;
use roots::*;
mod stack;
mod state_boundary;
use state_boundary::*;
