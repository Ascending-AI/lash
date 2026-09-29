//! Bytecode executor for compiled chunks, host effects, and trace/profile data.

use lash_sansio::profile::ProfileMark;
use std::sync::Arc;

use crate::ast::{JavaScriptBinaryOp, JavaScriptUnaryOp};
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
mod javascript_array_like;
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
use super::host::{ExecutionMode, SleepKind};
use super::record::{Record, record_with_capacity};
use super::schema::{
    ValidationPlan, compile_schema_value, execute_validate_builtin, execute_validation_plan,
};
use super::value::ProjectedValue;
use super::{
    BuiltinFunction, Chunk, ClosureParameterModel, CompiledProgram,
    DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, ExecutionHost, ExecutionOutcome, ExecutionScratch, Heap,
    HeapId, HeapObject, ImageValue, Instruction, InstructionProfileTag, IntrinsicOp,
    LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY, ListValue, Name, PersistedRoots,
    ProfileAccumulator, ProfileReport, ProjectedBindings, RegExpMatchObject, ResourceHandle,
    RuntimeError, State, Value, assign_path, charge_collection_work, deep_proportional_units,
    eval_javascript_binary, eval_javascript_unary, execute_compiled_format,
    execute_compiled_format_direct, execute_compiled_format_one_number_compact_direct,
    execute_intrinsic, execute_push_builtin, heap_inherited_builtin, inline_inherited_builtin,
    is_truthy, iterable_values, javascript_join, javascript_split, materialize_value,
    proportional_units, range_bounds, range_bounds_projected, read_javascript_field_direct,
    read_javascript_heap_field, read_javascript_heap_index, read_javascript_index_direct_with_key,
    regexp_string, sorting_work, unwrap_tool_result, unwrap_type_value,
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
    /// `chunk`'s program identity, stamped on and required back from a continuation.
    executable: &'a super::ExecutableIdentity,
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
            Instruction::BuildHeapList(len) => {
                let values = self.pop_n(len)?;
                self.stack.push(self.heap.allocate_list(values)?);
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
                        CallbackCompletion::Collect,
                        false,
                        Value::Undefined,
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
            Instruction::JavaScriptUnary(op) => {
                if self.javascript_unary_needs_slow_path(op)? {
                    return Ok(None);
                }
                self.execute_javascript_unary(op)?;
            }
            Instruction::JavaScriptBinary(op) => {
                if self.javascript_binary_needs_slow_path(op)? {
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
            Instruction::JavaScriptAddAssign(slot) => {
                if self.javascript_binary_needs_slow_path(JavaScriptBinaryOp::Add)? {
                    return Ok(None);
                }
                self.javascript_add_assign(slot)?;
            }
            Instruction::Finish => return Ok(Some(VmStep::Effect(VmEffect::Finish))),
            Instruction::SleepFor => {
                return Ok(Some(VmStep::Effect(VmEffect::Sleep(SleepKind::For))));
            }
            Instruction::ProcessWaitSignal { name } => {
                if self.mode != VmMode::Process {
                    return Err(RuntimeError::SessionProcessAdminOutsideProcess {
                        keyword: "wait_signal".into(),
                    });
                }
                return Ok(Some(VmStep::Effect(VmEffect::WaitSignal { name })));
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
    /// `step_instruction_fast` routes the stack-pair opcodes
    /// (`JavaScriptBinary`, `JavaScriptAddAssign`) to the slow path when an
    /// operand is `Value::Projected`. Materializing the top two operands in
    /// place — in the
    /// same right-then-left order the opcode pops them — and re-dispatching is
    /// exactly equivalent to what the slow arm would have done, but without
    /// duplicating the eval logic. The retry always completes because both
    /// operands are now concrete.
    fn redispatch_with_materialized_stack_pair(
        &mut self,
        instruction: Instruction,
    ) -> Result<VmStep, RuntimeError> {
        let right = materialize_value(self.pop_stack()?)?;
        let left = materialize_value(self.pop_stack()?)?;
        let op = match &instruction {
            Instruction::JavaScriptBinary(op) => Some(*op),
            Instruction::JavaScriptAddAssign(_) => Some(JavaScriptBinaryOp::Add),
            _ => None,
        };
        let (left, right) = match op {
            Some(op) => self.prepare_javascript_binary_operands(op, left, right)?,
            None => (left, right),
        };
        self.stack.push(left);
        self.stack.push(right);
        self.redispatch_fast(instruction)
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

    /// Slow path for the run loop. The synchronous `step_instruction_fast`
    /// fully handles every pure-compute and effect-producing opcode on
    /// non-projected operands; it only yields `Ok(None)` (routing here) when an
    /// operand is `Value::Projected` or the opcode inherently can suspend —
    /// guest-coercion paths (`JavaScriptUnary`/`JavaScriptBinary` operand
    /// ToPrimitive), tool/process effects, intrinsics, type literals.
    /// Projected reads themselves are synchronous descriptor calls; nothing
    /// here awaits on behalf of a `Value::Projected` alone.
    ///
    /// The stack-pair opcodes do not duplicate the fast-path eval here: they
    /// resolve the blocking projected operand and re-dispatch through
    /// `step_instruction_fast` (see
    /// `redispatch_with_materialized_stack_pair`). Only the genuinely
    /// suspending or projected-reading opcodes keep bespoke arms — lazy
    /// projected field/index propagation, the `truthy`-hook bool ops,
    /// intrinsics, iteration, and effects. The opcodes the fast path always
    /// completes are unreachable here.
    fn step_instruction(&mut self, instruction: Instruction) -> Result<VmStep, RuntimeError> {
        match instruction {
            Instruction::LoadField { slot, field } => {
                let value = self.load_slot(slot)?.clone();
                let field = self.chunk.names[field].clone();
                let value = match value {
                    Value::Projected(projected) => self.read_projected_field(&projected, &field)?,
                    value => self.read_dialect_field(value, &field)?,
                };
                self.stack.push(value);
            }
            Instruction::LoadFieldUnwrap { slot, field } => {
                let value = self.load_slot(slot)?.clone();
                let field = self.chunk.names[field].clone();
                let value = match value {
                    Value::Projected(projected) => self.read_projected_field(&projected, &field)?,
                    value => self.read_dialect_field(value, &field)?,
                };
                self.stack.push(unwrap_tool_result(value)?);
            }
            Instruction::Field(field) => {
                let target = self.pop_stack()?;
                let field = self.chunk.names[field].clone();
                let value = match target {
                    Value::Projected(projected) => self.read_projected_field(&projected, &field)?,
                    target => self.read_dialect_field(target, &field)?,
                };
                self.stack.push(value);
            }
            Instruction::Index => {
                let index = materialize_value(self.pop_stack()?)?;
                let target = self.pop_stack()?;
                let value = match target {
                    Value::Projected(projected) => self.read_projected_index(&projected, &index)?,
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
            Instruction::JavaScriptUnary(op) => {
                return self.redispatch_javascript_unary(op);
            }
            Instruction::JavaScriptBinary(_) => {
                return self.redispatch_with_materialized_stack_pair(instruction);
            }
            Instruction::JavaScriptAddAssign(_) => {
                return self.redispatch_with_materialized_stack_pair(instruction);
            }
            Instruction::ToBool => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy(&value)?,
                    _ => self.is_truthy_for_dialect(&value)?,
                };
                self.stack.push(Value::Bool(truthy));
            }
            Instruction::JumpIfFalse(target) => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy(&value)?,
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
            Instruction::JumpIfTrue(target) => {
                let value = self.pop_stack()?;
                let truthy = match &value {
                    Value::Projected(_) => is_truthy(&value)?,
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
            Instruction::AwaitHandle => {
                return Ok(VmStep::Effect(VmEffect::AwaitHandle));
            }
            Instruction::AwaitHandleUnwrap => {
                return Ok(VmStep::Effect(VmEffect::AwaitHandleUnwrap));
            }
            Instruction::Intrinsic(op) => {
                self.execute_intrinsic_instruction(op)?;
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
                let cursor = self.iteration_cursor(iterable)?;
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
                let (start, end, step) = range_bounds_projected(&self.stack[start_index..])?;
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
            | Instruction::BuildHeapList(_)
            | Instruction::BuildHeapRecord(_)
            | Instruction::ResultUnwrap
            | Instruction::Finish
            | Instruction::SleepFor
            | Instruction::ProcessWaitSignal { .. }
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
    fn execute_intrinsic_instruction(&mut self, op: IntrinsicOp) -> Result<(), RuntimeError> {
        let start = self.profile.as_ref().map(|_| ProfileMark::now());
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
                let schema = materialize_value(schema)?;
                let value = materialize_value(value)?;
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
                let value = materialize_value(value)?;
                // A validation walks the schema and the value's members once;
                // the compiled schema's size is fixed at compile time.
                self.charge_intrinsic_work(deep_proportional_units(&value));
                let value = execute_validation_plan(value, &self.chunk.compiled_schemas[schema])?;
                self.stack.push(value);
            }
            IntrinsicOp::PushAssign(slot) => {
                let mut item = Some(materialize_value(self.pop_stack()?)?);
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
                        let value = execute_push_builtin(current.clone(), item)?;
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
                    execute_compiled_format(template, values)?
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
                            execute_compiled_format(template, std::slice::from_ref(value))?
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
                )?;
                self.stack.truncate(self.stack.len() - argc);
                self.stack.push(value);
            }
        }
        if let Some(start) = start {
            self.record_builtin_profile(op, start.elapsed_nanos());
        }
        Ok(())
    }
}
pub(crate) mod functions;
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
