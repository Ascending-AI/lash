//! Guest ToPrimitive (FIG-3652): an object's own `valueOf`/`toString` runs
//! through the VM's one call path, then the instruction that needed it reruns.
//!
//! ECMA-262's OrdinaryToPrimitive calls an object's methods in hint order and
//! takes the first primitive one returns. A coercion runs inside one
//! instruction and cannot call guest code, so it asks for the answer instead
//! ([`RuntimeError::GuestCoercionPending`]). The VM then:
//!
//! 1. restores the instruction's operands and points `ip` back at it;
//! 2. calls the hook with the object as its receiver, through
//!    [`Vm::begin_function_call`], under the ordinary frame-depth limit and
//!    instruction budget;
//! 3. on return, records the primitive in the instruction's log (or tries the
//!    next method), and resumes at the instruction, which reruns and replays
//!    each recorded answer to its coercions in order.
//!
//! The rerun is the same pure computation over the same operands, so the
//! answers line up. A hook may not perform an effect (ADR 0062 register
//! entry 2, `EffectInBuiltinCallback`), so no effect ever runs inside a
//! coercion, no continuation is ever captured mid-coercion, and a replayed
//! program performs the same effects in the same order.

use super::*;
use crate::runtime::heap::ErrorKind;
use crate::runtime::heap::guest_coercion::{GuestPrimitive, PrimitiveHint};

/// The answers one instruction's guest hooks have given so far. `depth` is the
/// frame depth the instruction runs at.
pub(super) struct GuestCoercionLog {
    ip: usize,
    depth: usize,
    answers: Vec<GuestPrimitive>,
}

/// The return target of a guest hook's frame: the object being converted, the
/// hint, and the index of the next method to try if this one answers with an
/// object.
#[derive(Clone)]
pub(super) struct CoercionDriver {
    pub(super) object: Value,
    pub(super) hint: PrimitiveHint,
    pub(super) next: usize,
}

/// An instruction's operands, kept while the instruction may still need a hook.
pub(super) struct CoercionOperands {
    base: usize,
    values: Vec<Value>,
}

/// How many stack operands an instruction that can coerce an object consumes.
/// An instruction absent here reaches no conversion of an object operand.
fn coercing_operand_count(chunk: &Chunk, instruction: Instruction) -> Option<usize> {
    Some(match instruction {
        Instruction::PathAssign { path, .. } | Instruction::HeapPathAssign { path, .. } => {
            chunk.assign_paths[path].dynamic_index_count + 1
        }
        Instruction::Intrinsic(IntrinsicOp::JavaScriptHeapDeleteMember) => 2,
        Instruction::JavaScriptUnary(_) => 1,
        Instruction::JavaScriptBinary(_) | Instruction::Index => 2,
        Instruction::Intrinsic(
            IntrinsicOp::JavaScriptStdlib(argc)
            | IntrinsicOp::JavaScriptHeapNew(argc)
            | IntrinsicOp::JavaScriptRegExp(argc),
        ) => argc,
        Instruction::Intrinsic(IntrinsicOp::JavaScriptUriCodec(_)) => 1,
        Instruction::Intrinsic(IntrinsicOp::JavaScriptSplit | IntrinsicOp::JavaScriptJoin) => 2,
        // A call suspends only through a built-in callee answering without a
        // frame: `detached_builtin_result` converts an argument (a
        // `hasOwnProperty` key) before its receiver check, and that
        // conversion can be a guest hook. A closure callee suspends inside
        // its own frame's instructions instead.
        Instruction::Call { argc } => argc + 1,
        Instruction::CallMethod { argc } => argc + 2,
        Instruction::CallDynamic => 2,
        Instruction::CallMethodDynamic => 3,
        _ => return None,
    })
}

/// The callee operand of a call instruction's operand window, when the
/// instruction is a call: `[function, args..]`, `[receiver, function,
/// args..]`, `[function, arguments]` or `[receiver, function, arguments]`.
fn call_coercion_callee(instruction: Instruction, window: &[Value]) -> Option<&Value> {
    match instruction {
        Instruction::Call { .. } | Instruction::CallDynamic => window.first(),
        Instruction::CallMethod { .. } | Instruction::CallMethodDynamic => window.get(1),
        _ => None,
    }
}

/// Whether an operand can carry an object a hook might convert.
fn may_hold_object(value: &Value) -> bool {
    matches!(
        value,
        Value::Ref(_) | Value::Record(_) | Value::List(_) | Value::Tuple(_)
    )
}

fn is_primitive(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Undefined | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

enum OwnMember {
    Callable(Value),
    Present,
    Absent,
}

impl<H: ExecutionHost> Vm<'_, H> {
    /// Loads the answers an earlier run of the instruction at `ip` collected,
    /// and forgets the logs of instructions that are gone. Returns the depth
    /// whose log it loaded.
    pub(super) fn begin_instruction_coercions(&mut self, ip: usize) -> Option<usize> {
        if self.guest_coercions.is_empty() {
            return None;
        }
        let depth = self.frames.len();
        while let Some(top) = self.guest_coercions.last() {
            if top.depth > depth || top.depth == depth && top.ip != ip {
                self.guest_coercions.pop();
            } else {
                break;
            }
        }
        match self.guest_coercions.last_mut() {
            Some(top) if top.depth == depth && top.ip == ip => {
                self.heap
                    .guest_coercion
                    .load(std::mem::take(&mut top.answers));
                Some(depth)
            }
            _ => None,
        }
    }

    /// The instruction completed or failed: its log is spent.
    pub(super) fn end_instruction_coercions(&mut self, ip: usize, depth: usize) {
        let _ = self.heap.guest_coercion.unload();
        if self
            .guest_coercions
            .last()
            .is_some_and(|top| top.ip == ip && top.depth == depth)
        {
            self.guest_coercions.pop();
        }
    }

    /// Drops the logs of instructions a throw abandoned: every one at frame
    /// depth `depth` or deeper.
    pub(super) fn abandon_guest_coercions(&mut self, depth: usize) {
        self.guest_coercions.retain(|log| log.depth < depth);
    }

    /// Copies the operands of an instruction that may reach a guest hook.
    pub(super) fn coercion_operands(&self, instruction: Instruction) -> Option<CoercionOperands> {
        let count = coercing_operand_count(self.chunk, instruction)?;
        let base = self.stack.len().checked_sub(count)?;
        let window = &self.stack[base..];
        // A call reaches a hook only when its callee is a built-in function
        // running detached; snapshotting every call's operands would tax the
        // common case for a suspension only a built-in can request.
        if let Some(callee) = call_coercion_callee(instruction, window) {
            return self
                .builtin_callee(callee)
                .ok()
                .flatten()
                .is_some()
                .then(|| CoercionOperands {
                    base,
                    values: window.to_vec(),
                });
        }
        window
            .iter()
            .any(may_hold_object)
            .then(|| CoercionOperands {
                base,
                values: window.to_vec(),
            })
    }

    /// The instruction at `ip` asked for a hook: restore its operands, keep
    /// its answers, and run the hook.
    pub(super) fn suspend_for_guest_coercion(
        &mut self,
        ip: usize,
        operands: Option<CoercionOperands>,
    ) -> Result<(), RuntimeError> {
        let (answers, request) = self.heap.guest_coercion.unload();
        let request = request.ok_or_else(|| RuntimeError::InvalidExceptionState {
            reason: "a guest coercion was requested without an object".into(),
        })?;
        // Every instruction that converts an object operand declares its
        // operands above; reaching a hook from any other is a VM defect.
        let Some(operands) = operands else {
            return Err(RuntimeError::InvalidExceptionState {
                reason: "an instruction without declared operands reached a guest ToPrimitive hook"
                    .into(),
            });
        };
        self.stack.truncate(operands.base);
        self.stack.extend(operands.values);
        self.ip = ip;
        let depth = self.frames.len();
        match self.guest_coercions.last_mut() {
            Some(top) if top.ip == ip && top.depth == depth => top.answers = answers,
            _ => self
                .guest_coercions
                .push(GuestCoercionLog { ip, depth, answers }),
        }
        self.run_guest_coercion(request.object, request.hint, 0)
    }

    /// OrdinaryToPrimitive from the method at `from`: calls the next own
    /// callable method, or records the built-in `toString`'s answer, or throws
    /// the `TypeError` no method answered with.
    pub(super) fn run_guest_coercion(
        &mut self,
        object: Value,
        hint: PrimitiveHint,
        from: usize,
    ) -> Result<(), RuntimeError> {
        let order = hint.method_order();
        for (index, name) in order.iter().enumerate().skip(from) {
            match self.own_member(&object, name)? {
                OwnMember::Callable(function) => {
                    // A hook's frame costs one instruction, as a builtin
                    // callback's does.
                    self.instructions_executed = self.instructions_executed.saturating_add(1);
                    let receiver = object.clone();
                    return self.begin_function_call(
                        function,
                        receiver,
                        CallArguments::Owned(Vec::new()),
                        ReturnTarget::Coercion(CoercionDriver {
                            object,
                            hint,
                            next: index + 1,
                        }),
                    );
                }
                // `Object.prototype.toString` answers the type tag.
                OwnMember::Absent if *name == "toString" => {
                    self.record_guest_answer(GuestPrimitive::Tag);
                    return Ok(());
                }
                // `Object.prototype.valueOf` answers the object itself, and a
                // non-callable own property is skipped.
                OwnMember::Absent | OwnMember::Present => {}
            }
        }
        Err(
            match self.heap.allocate_error(
                ErrorKind::TypeError,
                Some("Cannot convert object to primitive value".to_string()),
                None,
                None,
            ) {
                Ok(value) => RuntimeError::UncaughtException { value },
                Err(error) => error,
            },
        )
    }

    /// A hook returned: a primitive is the answer, an object sends the
    /// conversion on to the next method.
    pub(super) fn finish_guest_hook(
        &mut self,
        driver: CoercionDriver,
        result: Value,
    ) -> Result<(), RuntimeError> {
        if is_primitive(&result) {
            self.record_guest_answer(GuestPrimitive::Value(result));
            Ok(())
        } else {
            self.run_guest_coercion(driver.object, driver.hint, driver.next)
        }
    }

    fn record_guest_answer(&mut self, answer: GuestPrimitive) {
        if let Some(log) = self.guest_coercions.last_mut() {
            debug_assert_eq!(log.depth, self.frames.len());
            debug_assert_eq!(log.ip, self.ip);
            log.answers.push(answer);
        }
    }

    fn own_member(&self, object: &Value, name: &str) -> Result<OwnMember, RuntimeError> {
        let record = match object {
            Value::Record(record) => record.as_ref(),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => record.as_ref(),
                _ => return Ok(OwnMember::Absent),
            },
            _ => return Ok(OwnMember::Absent),
        };
        Ok(match record.get(name) {
            None => OwnMember::Absent,
            Some(member @ Value::Ref(id)) if self.heap.get(*id)?.is_function() => {
                OwnMember::Callable(member.clone())
            }
            Some(_) => OwnMember::Present,
        })
    }
}

/// What a built-in converts one argument with: `Number` for ToNumber and its
/// integer forms, `String` for ToString. `None` leaves the argument alone.
fn parameter_hint(parameters: &[Option<PrimitiveHint>], index: usize) -> Option<PrimitiveHint> {
    parameters.get(index).copied().flatten()
}

const N: Option<PrimitiveHint> = Some(PrimitiveHint::Number);
const S: Option<PrimitiveHint> = Some(PrimitiveHint::String);
const V: Option<PrimitiveHint> = None;

/// The conversions a string method applies to its arguments, in order.
fn string_method_parameters(method: &str) -> Option<&'static [Option<PrimitiveHint>]> {
    Some(match method {
        "at" | "charAt" | "charCodeAt" | "codePointAt" | "repeat" => &[N],
        "slice" | "substring" => &[N, N],
        "endsWith" | "startsWith" | "includes" | "indexOf" | "lastIndexOf" => &[S, N],
        "padStart" | "padEnd" => &[N, S],
        _ => return None,
    })
}

/// The conversions an array method applies to its arguments, in order.
fn array_method_parameters(method: &str) -> Option<&'static [Option<PrimitiveHint>]> {
    Some(match method {
        "at" | "flat" => &[N],
        "copyWithin" => &[N, N, N],
        "indexOf" | "lastIndexOf" | "includes" => &[V, N],
        "fill" => &[V, N, N],
        "slice" | "splice" | "toSpliced" => &[N, N],
        "with" => &[N, V],
        "join" => &[S],
        _ => return None,
    })
}

/// The conversions a number method applies to its argument.
fn number_method_parameters(method: &str) -> Option<&'static [Option<PrimitiveHint>]> {
    Some(match method {
        "toFixed" | "toPrecision" | "toExponential" | "toString" => &[N],
        _ => return None,
    })
}

/// The conversion a static built-in applies to argument `index`.
fn static_parameter_hint(method: &str, index: usize) -> Option<PrimitiveHint> {
    match method {
        "Number.parseInt" => parameter_hint(&[S, N], index),
        "Number.parseFloat" | "JSON.parse" => parameter_hint(&[S], index),
        "String.fromCharCode" | "String.fromCodePoint" => N,
        _ if method.starts_with("Math.") => N,
        _ => None,
    }
}

impl<H: ExecutionHost> Vm<'_, H> {
    /// Converts, in argument order, each argument a built-in converts that
    /// is an object with its own `valueOf`/`toString` (FIG-3652): the
    /// built-in's own code then sees the primitive the hooks answered, which
    /// it converts exactly as it would have converted the object. An
    /// argument the built-in does not convert is left alone.
    pub(super) fn convert_guest_arguments(
        &mut self,
        values: &mut [Value],
    ) -> Result<(), RuntimeError> {
        if !values
            .iter()
            .skip(1)
            .any(|value| matches!(value, Value::Ref(_) | Value::Record(_)))
        {
            return Ok(());
        }
        let Some(Value::String(method)) = values.first() else {
            return Ok(());
        };
        let method = method.to_string();
        if method.contains('.') {
            for (index, value) in values.iter_mut().skip(1).enumerate() {
                if let Some(hint) = static_parameter_hint(&method, index)
                    && self.heap.has_guest_primitive_hooks(value)?
                {
                    *value = self.heap.javascript_to_primitive_with_hint(value, hint)?;
                }
            }
            return Ok(());
        }
        let Some((receiver, arguments)) = values[1..].split_first_mut() else {
            return Ok(());
        };
        // `String.prototype.concat` converts every argument.
        if matches!(receiver, Value::String(_)) && method == "concat" {
            for value in arguments.iter_mut() {
                if self.heap.has_guest_primitive_hooks(value)? {
                    *value = self
                        .heap
                        .javascript_to_primitive_with_hint(value, PrimitiveHint::String)?;
                }
            }
            return Ok(());
        }
        let parameters = match receiver {
            Value::String(_) => string_method_parameters(&method),
            Value::Number(_) => number_method_parameters(&method),
            Value::List(items) | Value::Tuple(items) => {
                // An empty array answers `indexOf`, `lastIndexOf` and
                // `includes` before converting `fromIndex`.
                if items.is_empty()
                    && matches!(method.as_str(), "indexOf" | "lastIndexOf" | "includes")
                {
                    return Ok(());
                }
                array_method_parameters(&method)
            }
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(items) | HeapObject::Tuple(items) => {
                    if items.is_empty()
                        && matches!(method.as_str(), "indexOf" | "lastIndexOf" | "includes")
                    {
                        return Ok(());
                    }
                    array_method_parameters(&method)
                }
                _ => None,
            },
            _ => None,
        };
        let Some(parameters) = parameters else {
            return Ok(());
        };
        if method == "join" {
            return self.convert_joined_elements(values);
        }
        for (index, value) in arguments.iter_mut().enumerate() {
            if let Some(hint) = parameter_hint(parameters, index)
                && self.heap.has_guest_primitive_hooks(value)?
            {
                *value = self.heap.javascript_to_primitive_with_hint(value, hint)?;
            }
        }
        Ok(())
    }

    /// `Array.prototype.join`: ToString(separator), then ToString of each
    /// element in order. An array holding an object with hooks is replaced,
    /// for this call, by one holding the strings the hooks answered.
    fn convert_joined_elements(&mut self, values: &mut [Value]) -> Result<(), RuntimeError> {
        if let Some(separator) = values.get_mut(2)
            && self.heap.has_guest_primitive_hooks(separator)?
        {
            *separator = self
                .heap
                .javascript_to_primitive_with_hint(separator, PrimitiveHint::String)?;
        }
        let items = match &values[1] {
            Value::List(items) | Value::Tuple(items) => items.to_vec(),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(items) | HeapObject::Tuple(items) => items.clone(),
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };
        let mut hooked = false;
        for item in &items {
            hooked |= self.heap.has_guest_primitive_hooks(item)?;
        }
        if !hooked {
            return Ok(());
        }
        let mut converted = Vec::with_capacity(items.len());
        for item in items {
            converted.push(if self.heap.has_guest_primitive_hooks(&item)? {
                self.heap
                    .javascript_to_primitive_with_hint(&item, PrimitiveHint::String)?
            } else {
                item
            });
        }
        values[1] = Value::List(converted.into());
        Ok(())
    }
}
