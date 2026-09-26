//! Built-in methods as values (FIG-3701): the reads that find one on a
//! value's prototype, and the calls whose callee is one.
//!
//! `const f = 'x'.includes` is `String.prototype.includes`, an ECMA function
//! object with no frame of its own. What `f` does with `this` is spelled by
//! the call that reached it (FIG-3787):
//!
//! - A member call — `o.m(...)` where `o.m` resolved to a built-in — passes
//!   `o` as the receiver, so a re-attached `o.push = Array.prototype.push`
//!   runs `push`'s generic ECMA steps on `o`.
//! - `f.call(receiver, ...)`, `f.apply(receiver, args)` and
//!   `f.bind(receiver)(...)` pass their receiver through the same channel;
//!   `Function.prototype.call`/`apply` unwrap the receiver chain iteratively
//!   so `a.call.call(b)` never stacks a frame per hop, and `bind` answers a
//!   closure on the [`BOUND_FUNCTION_INDEX`] sentinel that
//!   [`Vm::begin_function_call`] unwraps.
//! - A detached plain call `f(...)` passes `undefined`, and every advertised
//!   built-in rejects that receiver with the TypeError ECMA orders — the
//!   steps ECMA runs ahead of the receiver check (argument conversion,
//!   comparator validation) run first so the error a call throws is the one
//!   node throws.

use super::javascript_array_like::is_driven_array_method;
use super::*;
use crate::runtime::heap::guest_coercion::PrimitiveHint;
use crate::runtime::{BuiltinPrototype, javascript_to_string};

impl<H: ExecutionHost> Vm<'_, H> {
    /// A read that found no own property answers from the prototype chain:
    /// an advertised method is the one built-in function object for it, so
    /// `'a'.includes === 'b'.includes`. A value that has the property, even
    /// as `undefined`, keeps it; the inherited lookup already refused those.
    pub(super) fn or_inherited_builtin(
        &mut self,
        value: Value,
        target: &Value,
        key: &str,
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = target else {
            return Ok(value);
        };
        if !matches!(value, Value::Undefined) {
            return Ok(value);
        }
        let inherited = heap_inherited_builtin(self.heap.get(*id)?, key);
        self.or_builtin(value, inherited)
    }

    pub(super) fn or_builtin(
        &mut self,
        value: Value,
        inherited: Option<BuiltinFunction>,
    ) -> Result<Value, RuntimeError> {
        match inherited {
            Some(function) if matches!(value, Value::Undefined) => {
                self.heap.builtin_function(function)
            }
            _ => Ok(value),
        }
    }

    /// Answers `Object.is` with a heap operand by reference, and says whether
    /// it did. SameValue on an object is identity, so nothing is exported: a
    /// function has no exported form at all, which made `Object.is(f, f)` a
    /// boundary fault for every function value.
    pub(super) fn object_is_by_reference(&mut self, values: &[Value]) -> bool {
        let [Value::String(method), left, right] = values else {
            return false;
        };
        if method.as_str() != "Object.is" {
            return false;
        };
        let is_primitive = |value: &Value| {
            matches!(
                value,
                Value::Null
                    | Value::Undefined
                    | Value::Bool(_)
                    | Value::Number(_)
                    | Value::String(_)
            )
        };
        let same = match (left, right) {
            (Value::Ref(left), Value::Ref(right)) => left == right,
            (Value::Ref(_), other) | (other, Value::Ref(_)) if is_primitive(other) => false,
            _ => return false,
        };
        self.stack.push(Value::Bool(same));
        true
    }

    /// The built-in function `callee` is, when it is one.
    pub(super) fn builtin_callee(
        &self,
        callee: &Value,
    ) -> Result<Option<BuiltinFunction>, RuntimeError> {
        let Value::Ref(id) = callee else {
            return Ok(None);
        };
        Ok(match self.heap.get(*id)? {
            HeapObject::BuiltinFunction(function) => Some(*function),
            _ => None,
        })
    }

    /// Calls built-in `function` with `receiver` as its `this` and hands the
    /// answer to `return_target` as a returning frame would.
    ///
    /// This is the one place a call can still begin something a value alone
    /// cannot answer: `call`/`apply`/`bind` re-call or wrap their own
    /// receiver, and the callback array methods drive guest frames.
    pub(super) fn call_detached_builtin(
        &mut self,
        mut function: BuiltinFunction,
        mut receiver: Value,
        mut args: Vec<Value>,
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        // `Function.prototype.call` and `.apply` re-call their receiver —
        // itself a function value — under a fresh `this`. Unwrapping builtin
        // targets iteratively keeps `f.call.call(g, r, ...)` chains and
        // bound-function chains inside this one step.
        loop {
            let Some(prototype) = function.prototype() else {
                let qualified = function.qualified_name();
                // `Array.of`/`Array.from` run `Construct(C, «len»)` on their
                // `this`. A guest or bound-function receiver is a
                // constructor this dialect cannot invoke — authored `new` is
                // the same refusal — while a non-constructor `this` falls to
                // the ordinary ArrayCreate path.
                if matches!(qualified.as_str(), "Array.of" | "Array.from")
                    && self.is_callable(&receiver)?
                    && !matches!(
                        self.builtin_callee(&receiver)?,
                        Some(builtin) if builtin.qualified_name() == "Array"
                    )
                {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_NEW_UNSUPPORTED: `{qualified}` constructs its `this` receiver"
                        ),
                    });
                }
                if qualified == "Array.from" {
                    return self.detached_array_from(&args, return_target);
                }
                let result = self.global_builtin_result(&qualified, &args)?;
                return self.complete_call(result, return_target);
            };
            if prototype == BuiltinPrototype::Function {
                match function.name() {
                    "call" | "apply" => {
                        let name = function.name().to_string();
                        if !self.is_callable(&receiver)? {
                            return self.not_a_function_receiver(&name, &receiver);
                        }
                        let this_arg = args.first().cloned().unwrap_or(Value::Undefined);
                        let call_args = if name == "call" {
                            args.get(1..).unwrap_or(&[]).to_vec()
                        } else {
                            self.apply_arguments(args.get(1))?
                        };
                        if let Some(builtin) = self.builtin_callee(&receiver)? {
                            // The builtin's own dispatch runs again with the
                            // new receiver — `x.call` on a method re-enters
                            // here rather than hopping a frame.
                            function = builtin;
                            receiver = this_arg;
                            args = call_args;
                            continue;
                        }
                        return self.begin_function_call(
                            receiver,
                            this_arg,
                            CallArguments::Owned(call_args),
                            return_target,
                        );
                    }
                    "bind" => {
                        if !self.is_callable(&receiver)? {
                            return self.not_a_function_receiver("bind", &receiver);
                        }
                        let bound = self.bind_function(
                            &receiver,
                            args.first().cloned().unwrap_or(Value::Undefined),
                            args.get(1..).unwrap_or(&[]),
                        )?;
                        return self.complete_call(bound, return_target);
                    }
                    _ => {}
                }
            }
            if prototype == BuiltinPrototype::Array
                && is_driven_array_method(function.name(), &args)
            {
                return self.call_array_like_method(receiver, function.name(), args, return_target);
            }
            // `Array.prototype.toString` delegates through the receiver's own
            // `join`: a guest `join` runs as a frame on the same receiver.
            if prototype == BuiltinPrototype::Array
                && function.name() == "toString"
                && !matches!(receiver, Value::Undefined | Value::Null)
            {
                let join = self.array_like_get(&receiver, "join")?;
                if self.builtin_callee(&join)?.is_none() && self.is_callable(&join)? {
                    return self.begin_function_call(
                        join,
                        receiver,
                        CallArguments::Owned(Vec::new()),
                        return_target,
                    );
                }
            }
            // `Date.prototype.toJSON` is generic: `ToPrimitive(O, Number)`,
            // then `Invoke(O, "toISOString")` — a guest call the value path
            // cannot answer, so the call-level dispatch drives it.
            if (prototype, function.name()) == (BuiltinPrototype::Date, "toJSON") {
                if matches!(receiver, Value::Undefined | Value::Null) {
                    return Err(RuntimeError::type_error(
                        "Cannot convert undefined or null to object",
                    ));
                }
                let primitive = self
                    .heap
                    .javascript_to_primitive_with_hint(&receiver, PrimitiveHint::Number)?;
                if let Value::Number(number) = primitive
                    && !number.is_finite()
                {
                    return self.complete_call(Value::Null, return_target);
                }
                // `Invoke(O, "toISOString")`: an own member wins; otherwise
                // the prototype chain's built-in — which a real `Date`
                // resolves to `Date.prototype.toISOString`.
                let member = self.array_like_get(&receiver, "toISOString")?;
                let member = self.or_inherited_builtin(member, &receiver, "toISOString")?;
                if let Some(builtin) = self.builtin_callee(&member)? {
                    function = builtin;
                    continue;
                }
                if self.is_callable(&member)? {
                    return self.begin_function_call(
                        member,
                        receiver,
                        CallArguments::Owned(Vec::new()),
                        return_target,
                    );
                }
                return Err(RuntimeError::type_error("toISOString is not a function"));
            }
            // The collection `forEach` methods drive guest callbacks; on a
            // matching heap receiver the driver's pending-call queue is the
            // same live ordered entry list the member call uses.
            if matches!(
                (prototype, function.name()),
                (BuiltinPrototype::Map, "forEach") | (BuiltinPrototype::Set, "forEach")
            ) && let Value::Ref(id) = &receiver
                && matches!(
                    (prototype, self.heap.get(*id)?),
                    (BuiltinPrototype::Map, HeapObject::Map(_))
                        | (BuiltinPrototype::Set, HeapObject::Set(_))
                )
            {
                if !matches!(return_target, ReturnTarget::Direct) {
                    return Err(RuntimeError::ValidationFailed {
                        reason: "TS_NESTED_DRIVER_UNSUPPORTED: a driving method reached as a callback cannot suspend".to_string(),
                    });
                }
                return self.execute_javascript_heap_method(function.name(), *id, &args);
            }
            let result = self.detached_builtin_result(function, &receiver, &args)?;
            return self.complete_call(result, return_target);
        }
    }

    /// The arguments `f.apply(receiver, list)` passes on: ECMA's
    /// `CreateListFromArrayLike` — `undefined`/`null` mean none, anything
    /// else must be an object whose `length` and indices read like an
    /// array's.
    fn apply_arguments(&mut self, list: Option<&Value>) -> Result<Vec<Value>, RuntimeError> {
        let Some(list) = list else {
            return Ok(Vec::new());
        };
        match list {
            Value::Undefined | Value::Null => Ok(Vec::new()),
            Value::List(items) | Value::Tuple(items) => Ok(items.to_vec()),
            Value::Ref(_) | Value::Record(_) => {
                let length = self.array_like_length(list)?;
                self.array_like_dense(list, 0, length)
            }
            _ => Err(RuntimeError::type_error(
                "CreateListFromArrayLike called on non-object",
            )),
        }
    }

    /// `Function.prototype.bind`: the product is a closure on the
    /// [`BOUND_FUNCTION_INDEX`] sentinel whose captures are `[target,
    /// receiver, boundArgs]` — `begin_function_call` unwraps it into the
    /// target's call, so bound functions snapshot, restore and collect
    /// exactly as closures do.
    fn bind_function(
        &mut self,
        target: &Value,
        receiver: Value,
        bound_args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let name = self.bound_function_name(target)?;
        let length = self.bound_function_length(target, bound_args.len() as u64)?;
        let bound_args = self.heap.allocate_list(bound_args.to_vec())?;
        self.heap.allocate_object(HeapObject::Closure {
            function: BOUND_FUNCTION_INDEX,
            captures: vec![target.clone(), receiver, bound_args],
            name: Some(Value::String(name.into())),
            length: Some(Value::Number(length)),
        })
    }

    /// The bound function's `name`: `"bound "` plus the target's own name, or
    /// the empty string ECMA answers for a nameless target.
    fn bound_function_name(&mut self, target: &Value) -> Result<String, RuntimeError> {
        let name = match target {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Closure {
                    name: Some(Value::String(name)),
                    ..
                } => name.to_string(),
                HeapObject::BuiltinFunction(function) => function.name().to_string(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        Ok(format!("bound {name}"))
    }

    /// The bound function's `length`: the target's own, minus the bound
    /// arguments, clamped to zero.
    fn bound_function_length(
        &mut self,
        target: &Value,
        bound_args: u64,
    ) -> Result<f64, RuntimeError> {
        let length = match target {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Closure {
                    length: Some(Value::Number(length)),
                    ..
                } => *length,
                HeapObject::BuiltinFunction(function) => function.length() as f64,
                _ => 0.0,
            },
            _ => 0.0,
        };
        Ok((length - bound_args as f64).max(0.0))
    }

    /// V8's text for `Function.prototype.<call|apply|bind>` on a receiver
    /// that is not a function.
    fn not_a_function_receiver(
        &mut self,
        name: &str,
        receiver: &Value,
    ) -> Result<(), RuntimeError> {
        let text = self.v8_value_text(receiver)?;
        let kind = crate::runtime::value_type_name(receiver);
        Err(RuntimeError::type_error(format!(
            "Function.prototype.{name} was called on {text}, which is a {kind} and not a function"
        )))
    }

    /// What built-in `function` answers with `receiver` as its `this`.
    ///
    /// Everything here answers a value — the calls that cannot (the callback
    /// array methods, `call`/`apply`/`bind`, a receiver's own `join` from
    /// `toString`) are spelled in [`Self::call_detached_builtin`].
    pub(super) fn detached_builtin_result(
        &mut self,
        function: BuiltinFunction,
        receiver: &Value,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let Some(prototype) = function.prototype() else {
            // A global or a static answers by its qualified name — the
            // constructors, the URI codecs and their friends have no
            // receiver to reject.
            return self.global_builtin_result(&function.qualified_name(), args);
        };
        let name = function.name();
        match (prototype, name) {
            // ToPropertyKey(V) precedes ToObject(this): the key converts,
            // with whatever that conversion does, before the receiver throws.
            (BuiltinPrototype::Object, "hasOwnProperty") => {
                self.heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?;
            }
            // Both sorts validate the comparator before they touch `this`.
            (BuiltinPrototype::Array, "sort" | "toSorted") => {
                if let Some(comparator) = args.first()
                    && !matches!(comparator, Value::Undefined)
                    && !self.is_callable(comparator)?
                {
                    return Err(RuntimeError::IncompatibleReceiver {
                        message: format!(
                            "The comparison function must be either a function or undefined: {}",
                            self.v8_value_text(comparator)?
                        ),
                    });
                }
            }
            _ => {}
        }
        if matches!(receiver, Value::Undefined | Value::Null) {
            // ECMA's Object.prototype.toString answers a tag for every
            // receiver, `undefined` included.
            if prototype == BuiltinPrototype::Object && name == "toString" {
                return Ok(Value::String(
                    match receiver {
                        Value::Null => "[object Null]",
                        _ => "[object Undefined]",
                    }
                    .into(),
                ));
            }
            return Err(RuntimeError::IncompatibleReceiver {
                message: undefined_receiver_message(prototype, name, receiver),
            });
        }
        match prototype {
            BuiltinPrototype::Object => self.object_prototype_method(name, receiver, args),
            BuiltinPrototype::Function => match name {
                // `call`/`apply`/`bind` reached the value path only as a
                // callback driver's builtin; they cannot answer a value.
                "call" | "apply" | "bind" => Err(RuntimeError::ValidationFailed {
                    reason: "TS_FUNCTION_METHOD_UNSUPPORTED: a callback-bound function method needs the call path".to_string(),
                }),
                // Function.prototype.toString requires a callable receiver —
                // the check ECMA runs before it would answer source text —
                // then answers source text the value model does not retain.
                "toString" => {
                    if !self.is_callable(receiver)? {
                        return Err(RuntimeError::type_error(
                            "Function.prototype.toString requires that 'this' be a Function",
                        ));
                    }
                    Err(RuntimeError::ValidationFailed {
                        reason: "TS_FUNCTION_TOSTRING_UNSUPPORTED: this value model does not retain function source".to_string(),
                    })
                }
                _ => Err(RuntimeError::ValidationFailed {
                    reason: format!("TS_METHOD_UNSUPPORTED: Function.prototype.{name}"),
                }),
            },
            BuiltinPrototype::Array => self.array_like_method(receiver, name, args),
            BuiltinPrototype::String => match name {
                "toString" | "valueOf" => match self.string_primitive_of(receiver)? {
                    Some(text) => Ok(Value::String(text.into())),
                    None => Err(RuntimeError::IncompatibleReceiver {
                        message: incompatible_receiver_message(prototype, name, receiver, self)?,
                    }),
                },
                _ => {
                    // Every other String.prototype method coerces its
                    // receiver by ToString — guest `toString`/`valueOf`
                    // hooks suspend the owning instruction and replay here.
                    // The prototype's own `[[StringData]]` is the empty
                    // string, so `String.prototype.toLowerCase()` answers
                    // `""` rather than trying to convert a function object.
                    let text = match self.string_primitive_of(receiver)? {
                        Some(text) => text,
                        None => self.heap.javascript_to_string(receiver)?,
                    };
                    // The pure method reads primitives; each argument ECMA
                    // converts — `padStart`'s `maxLength`, then `fillString`
                    // — is converted on the method's own parameter order.
                    let mut args = args.to_vec();
                    self.convert_detached_guest_arguments(
                        BuiltinPrototype::String,
                        name,
                        &mut args,
                    )?;
                    super::javascript::javascript_string_method(
                        name,
                        &text,
                        &args,
                        &mut self.instructions_executed,
                    )
                }
            },
            BuiltinPrototype::Number => match self.number_primitive_of(receiver)? {
                Some(number) => {
                    let mut args = args.to_vec();
                    self.convert_detached_guest_arguments(
                        BuiltinPrototype::Number,
                        name,
                        &mut args,
                    )?;
                    super::javascript_number::javascript_number_method(name, number, &args)
                }
                None => Err(RuntimeError::IncompatibleReceiver {
                    message: incompatible_receiver_message(prototype, name, receiver, self)?,
                }),
            },
            BuiltinPrototype::Boolean => match self.bool_primitive_of(receiver)? {
                Some(value) => match name {
                    "toString" => Ok(Value::String(
                        if value { "true" } else { "false" }.into(),
                    )),
                    "valueOf" => Ok(Value::Bool(value)),
                    _ => Err(RuntimeError::ValidationFailed {
                        reason: format!("TS_METHOD_UNSUPPORTED: Boolean.prototype.{name}"),
                    }),
                },
                None => Err(RuntimeError::IncompatibleReceiver {
                    message: incompatible_receiver_message(prototype, name, receiver, self)?,
                }),
            },
            BuiltinPrototype::RegExp => match receiver {
                Value::Ref(id) if matches!(self.heap.get(*id)?, HeapObject::RegExp(_)) => {
                    self.regexp_prototype_method(name, *id, args)
                }
                _ => Err(RuntimeError::IncompatibleReceiver {
                    message: incompatible_receiver_message(prototype, name, receiver, self)?,
                }),
            },
            BuiltinPrototype::Date => match receiver {
                Value::Ref(id) if matches!(self.heap.get(*id)?, HeapObject::Date(_)) => self
                    .execute_javascript_date_method(name, *id)?
                    .ok_or_else(|| RuntimeError::ValidationFailed {
                        reason: format!("TS_METHOD_UNSUPPORTED: Date.prototype.{name}"),
                    }),
                _ => Err(RuntimeError::IncompatibleReceiver {
                    message: incompatible_receiver_message(prototype, name, receiver, self)?,
                }),
            },
            BuiltinPrototype::Error => self.error_prototype_method(name, receiver, args),
            BuiltinPrototype::Map | BuiltinPrototype::Set => {
                self.map_set_prototype_method(prototype, name, receiver, args)
            }
            BuiltinPrototype::Url => match receiver {
                Value::Ref(id) if matches!(self.heap.get(*id)?, HeapObject::Url(_)) => self
                    .execute_url_heap_method("URL", name, *id, args)?
                    .ok_or_else(|| RuntimeError::ValidationFailed {
                        reason: format!("TS_METHOD_UNSUPPORTED: URL.prototype.{name}"),
                    }),
                _ => Err(RuntimeError::IncompatibleReceiver {
                    message: incompatible_receiver_message(prototype, name, receiver, self)?,
                }),
            },
            BuiltinPrototype::UrlSearchParams => match receiver {
                Value::Ref(id)
                    if matches!(self.heap.get(*id)?, HeapObject::UrlSearchParams(_)) =>
                {
                    self.execute_url_heap_method("URLSearchParams", name, *id, args)?
                        .ok_or_else(|| RuntimeError::ValidationFailed {
                            reason: format!(
                                "TS_METHOD_UNSUPPORTED: URLSearchParams.prototype.{name}"
                            ),
                        })
                }
                _ => Err(RuntimeError::IncompatibleReceiver {
                    message: incompatible_receiver_message(prototype, name, receiver, self)?,
                }),
            },
        }
    }

    /// `Object.prototype.<method>` — every one of them is generic over
    /// `ToObject(this)`.
    fn object_prototype_method(
        &mut self,
        name: &str,
        receiver: &Value,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        match name {
            "toString" => Ok(Value::String(self.object_tag(receiver)?.into())),
            "toLocaleString" => Ok(Value::String(self.object_tag(receiver)?.into())),
            // `valueOf` answers `ToObject(this)`: the object itself, or the
            // ephemeral wrapper a primitive gets — a fresh object whose
            // `typeof` is `object`.
            "valueOf" => Ok(self.array_like_receiver_object(receiver)),
            "hasOwnProperty" => {
                let key = self
                    .heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?;
                Ok(Value::Bool(self.object_has_own(receiver, &key)?))
            }
            "propertyIsEnumerable" => {
                let key = self
                    .heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?;
                Ok(Value::Bool(self.object_has_own_enumerable(receiver, &key)?))
            }
            "isPrototypeOf" => Ok(Value::Bool(self.object_is_prototype_of(receiver, args)?)),
            _ => Err(RuntimeError::ValidationFailed {
                reason: format!("TS_METHOD_UNSUPPORTED: Object.prototype.{name}"),
            }),
        }
    }

    /// The `[[Class]]`-style tag `Object.prototype.toString` answers for a
    /// receiver.
    pub(super) fn object_tag(&self, receiver: &Value) -> Result<String, RuntimeError> {
        Ok(match receiver {
            Value::Null => "[object Null]".to_string(),
            Value::Undefined => "[object Undefined]".to_string(),
            Value::Bool(_) => "[object Boolean]".to_string(),
            Value::Number(_) => "[object Number]".to_string(),
            Value::String(_) => "[object String]".to_string(),
            Value::List(_) | Value::Tuple(_) | Value::Record(_) => {
                if matches!(receiver, Value::Record(_)) {
                    "[object Object]".to_string()
                } else {
                    "[object Array]".to_string()
                }
            }
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(_) | HeapObject::Tuple(_) | HeapObject::RegExpMatch(_) => {
                    "[object Array]".to_string()
                }
                // An `arguments` object is a record marked at construction;
                // its tag is `[object Arguments]`, not `[object Object]`.
                HeapObject::Record(_) if self.heap.is_arguments_record(*id) => {
                    "[object Arguments]".to_string()
                }
                HeapObject::Record(_) => "[object Object]".to_string(),
                HeapObject::Closure { .. } => "[object Function]".to_string(),
                HeapObject::BuiltinFunction(function) => {
                    if function.callable() {
                        "[object Function]".to_string()
                    } else {
                        builtin_object_tag(function)
                    }
                }
                HeapObject::RegExp(_) => "[object RegExp]".to_string(),
                HeapObject::Map(_) => "[object Map]".to_string(),
                HeapObject::Set(_) => "[object Set]".to_string(),
                HeapObject::Date(_) => "[object Date]".to_string(),
                HeapObject::Error(_) => "[object Error]".to_string(),
                HeapObject::Url(_) => "[object URL]".to_string(),
                HeapObject::UrlSearchParams(_) => "[object URLSearchParams]".to_string(),
                HeapObject::Cell(_) => {
                    return Err(RuntimeError::NotABindingCell {
                        actual: "Object.prototype.toString receiver".to_string(),
                    });
                }
            },
            _ => "[object Object]".to_string(),
        })
    }

    /// `hasOwnProperty`: an own member only, never the prototype's.
    fn object_has_own(&mut self, receiver: &Value, key: &str) -> Result<bool, RuntimeError> {
        match receiver {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::Record(record) => Ok(record.get(key).is_some()),
                HeapObject::List(items) | HeapObject::Tuple(items) => {
                    if key == "length" {
                        return Ok(true);
                    }
                    Ok(match super::javascript_stdlib::array_index_property(key) {
                        Some(index) => {
                            let index = index as usize;
                            index < items.len() && !self.heap.is_list_hole(*id, index)
                        }
                        None => false,
                    })
                }
                HeapObject::RegExpMatch(result) => Ok(match key {
                    "length" | "index" | "input" | "groups" => true,
                    _ => super::javascript_stdlib::array_index_property(key)
                        .is_some_and(|index| (index as usize) < result.items.len()),
                }),
                HeapObject::BuiltinFunction(_) => self.heap.builtin_has_own(*id, key),
                HeapObject::Closure { .. } => Ok(matches!(key, "length" | "name")),
                _ => Ok(false),
            },
            Value::Record(record) => Ok(record.get(key).is_some()),
            Value::List(items) | Value::Tuple(items) => Ok(key == "length"
                || super::javascript_stdlib::array_index_property(key)
                    .is_some_and(|index| (index as usize) < items.len())),
            Value::String(text) => Ok(key == "length"
                || super::javascript_stdlib::array_index_property(key)
                    .is_some_and(|index| (index as usize) < text.encode_utf16().count())),
            _ => Ok(false),
        }
    }

    /// `propertyIsEnumerable`: own and enumerable — every record field and
    /// array element the model stores is enumerable.
    fn object_has_own_enumerable(
        &mut self,
        receiver: &Value,
        key: &str,
    ) -> Result<bool, RuntimeError> {
        // `length`, `name` and the prototype members are non-enumerable; the
        // elements and record fields a guest writes are not.
        if matches!(key, "length" | "name") {
            return match receiver {
                Value::Ref(id) if matches!(self.heap.get(*id)?, HeapObject::Record(r) if r.get(key).is_some()) => {
                    Ok(true)
                }
                Value::Record(record) => Ok(record.get(key).is_some()),
                _ => Ok(false),
            };
        }
        self.object_has_own(receiver, key)
    }

    /// `isPrototypeOf`: the one prototype link the model exposes is the
    /// builtin `X.prototype` object — `X.prototype.isPrototypeOf(instance)`.
    fn object_is_prototype_of(
        &mut self,
        receiver: &Value,
        args: &[Value],
    ) -> Result<bool, RuntimeError> {
        let Value::Ref(id) = receiver else {
            return Ok(false);
        };
        let HeapObject::BuiltinFunction(owner) = self.heap.get(*id)? else {
            return Ok(false);
        };
        let qualified = owner.qualified_name();
        let Some(prototype_name) = qualified.strip_suffix(".prototype") else {
            return Ok(false);
        };
        Ok(match args.first() {
            Some(Value::Ref(arg)) => match self.heap.get(*arg)? {
                HeapObject::Map(_) => prototype_name == "Map",
                HeapObject::Set(_) => prototype_name == "Set",
                HeapObject::Date(_) => prototype_name == "Date",
                HeapObject::RegExp(_) => prototype_name == "RegExp",
                HeapObject::Error(error) => {
                    prototype_name == error.kind.name() || prototype_name == "Error"
                }
                HeapObject::Url(_) => prototype_name == "URL",
                HeapObject::UrlSearchParams(_) => prototype_name == "URLSearchParams",
                HeapObject::List(_) | HeapObject::Tuple(_) | HeapObject::RegExpMatch(_) => {
                    matches!(prototype_name, "Array" | "Object")
                }
                HeapObject::Record(_) => prototype_name == "Object",
                _ => prototype_name == "Object",
            },
            Some(Value::Record(_)) => prototype_name == "Object",
            Some(Value::List(_)) | Some(Value::Tuple(_)) => {
                matches!(prototype_name, "Array" | "Object")
            }
            _ => false,
        })
    }

    /// `String.prototype`'s intrinsic receiver: a `String` primitive, or the
    /// prototype object itself, whose `[[StringData]]` is the empty string.
    fn string_primitive_of(&mut self, receiver: &Value) -> Result<Option<String>, RuntimeError> {
        Ok(match receiver {
            Value::String(text) => Some(text.to_string()),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::BuiltinFunction(function)
                    if function.qualified_name() == "String.prototype" =>
                {
                    Some(String::new())
                }
                _ => None,
            },
            _ => None,
        })
    }

    /// `Number.prototype`'s intrinsic receiver: a `Number` primitive, or the
    /// prototype object itself, whose `[[NumberData]]` is `0`.
    fn number_primitive_of(&mut self, receiver: &Value) -> Result<Option<f64>, RuntimeError> {
        Ok(match receiver {
            Value::Number(number) => Some(*number),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::BuiltinFunction(function)
                    if function.qualified_name() == "Number.prototype" =>
                {
                    Some(0.0)
                }
                _ => None,
            },
            _ => None,
        })
    }

    /// `Boolean.prototype`'s intrinsic receiver: a `Boolean` primitive, or
    /// the prototype object itself, whose `[[BooleanData]]` is `false`.
    fn bool_primitive_of(&mut self, receiver: &Value) -> Result<Option<bool>, RuntimeError> {
        Ok(match receiver {
            Value::Bool(value) => Some(*value),
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::BuiltinFunction(function)
                    if function.qualified_name() == "Boolean.prototype" =>
                {
                    Some(false)
                }
                _ => None,
            },
            _ => None,
        })
    }

    /// `RegExp.prototype.<method>` on a `RegExp` receiver.
    fn regexp_prototype_method(
        &mut self,
        name: &str,
        receiver: HeapId,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        match name {
            "exec" => {
                let input = self
                    .heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?;
                self.exec_regexp(receiver, &input)
            }
            "test" => {
                let input = self
                    .heap
                    .javascript_to_string(args.first().unwrap_or(&Value::Undefined))?;
                Ok(Value::Bool(!matches!(
                    self.exec_regexp(receiver, &input)?,
                    Value::Null
                )))
            }
            "toString" => {
                let HeapObject::RegExp(regexp) = self.heap.get(receiver)? else {
                    unreachable!("RegExp receiver kind was checked")
                };
                Ok(Value::String(regexp_string(regexp).into()))
            }
            "valueOf" => Ok(Value::Ref(receiver)),
            _ => Err(RuntimeError::ValidationFailed {
                reason: format!("TS_METHOD_UNSUPPORTED: RegExp.prototype.{name}"),
            }),
        }
    }

    /// `Error.prototype.toString` is fully generic: it reads `name` and
    /// `message` off any object receiver.
    fn error_prototype_method(
        &mut self,
        name: &str,
        receiver: &Value,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        match name {
            "toString" => {
                let name_value = self.array_like_get(receiver, "name")?;
                let message_value = self.array_like_get(receiver, "message")?;
                let name = match name_value {
                    Value::Undefined => "Error".to_string(),
                    value => self.heap.javascript_to_string(&value)?,
                };
                let message = match message_value {
                    Value::Undefined => String::new(),
                    value => self.heap.javascript_to_string(&value)?,
                };
                Ok(Value::String(
                    if name.is_empty() {
                        message
                    } else if message.is_empty() {
                        name
                    } else {
                        format!("{name}: {message}")
                    }
                    .into(),
                ))
            }
            "valueOf" => Ok(receiver.clone()),
            _ => Err(RuntimeError::ValidationFailed {
                reason: format!("TS_METHOD_UNSUPPORTED: Error.prototype.{name}"),
            }),
        }
    }

    /// `Map.prototype`/`Set.prototype.<method>` on a receiver of the matching
    /// kind — the heap-method dispatch's synchronous results; `forEach`
    /// reaches only the call-level driver path and refuses here.
    fn map_set_prototype_method(
        &mut self,
        prototype: BuiltinPrototype,
        name: &str,
        receiver: &Value,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let Value::Ref(id) = receiver else {
            return Err(RuntimeError::IncompatibleReceiver {
                message: incompatible_receiver_message(prototype, name, receiver, self)?,
            });
        };
        let matches_kind = matches!(
            (prototype, self.heap.get(*id)?),
            (BuiltinPrototype::Map, HeapObject::Map(_))
                | (BuiltinPrototype::Set, HeapObject::Set(_))
        );
        if !matches_kind {
            return Err(RuntimeError::IncompatibleReceiver {
                message: incompatible_receiver_message(prototype, name, receiver, self)?,
            });
        }
        if name == "forEach" {
            return Err(RuntimeError::ValidationFailed {
                reason: "TS_NESTED_DRIVER_UNSUPPORTED: a driving method reached as a callback cannot suspend".to_string(),
            });
        }
        let base = self.stack.len();
        self.execute_javascript_heap_method(name, *id, args)?;
        if self.stack.len() == base {
            return Ok(Value::Undefined);
        }
        Ok(self.stack.pop().unwrap_or(Value::Undefined))
    }

    /// How V8 names a value inside a TypeError message: a primitive by its
    /// string, an array or a RegExp by its builtin tag, any other object by
    /// its constructor.
    pub(super) fn v8_value_text(&self, value: &Value) -> Result<String, RuntimeError> {
        let Value::Ref(id) = value else {
            return match value {
                Value::List(_) | Value::Tuple(_) => Ok("[object Array]".to_string()),
                Value::Record(_) => Ok("#<Object>".to_string()),
                primitive => Ok(javascript_to_string(primitive)),
            };
        };
        Ok(match self.heap.get(*id)? {
            HeapObject::List(_) | HeapObject::Tuple(_) | HeapObject::RegExpMatch(_) => {
                "[object Array]".to_string()
            }
            HeapObject::RegExp(_) => "[object RegExp]".to_string(),
            HeapObject::Record(_) => "#<Object>".to_string(),
            object => format!("#<{}>", object.kind_name()),
        })
    }

    pub(super) fn is_callable(&self, value: &Value) -> Result<bool, RuntimeError> {
        Ok(match value {
            Value::Ref(id) => self.heap.get(*id)?.is_function(),
            _ => false,
        })
    }

    /// `Array.from(items[, mapfn[, thisArg]])` reached as a value — by
    /// `.call`, `.apply`, `bind`, or a bare `Array.from` once the callee is
    /// the built-in object. `IsConstructor(this)` was already vetted by the
    /// caller; what remains is `items`: a nullish source is the TypeError,
    /// the iterable shapes this dialect materializes copy their entries
    /// shallowly, and everything else is an array-like read by `Get` over
    /// `0..ToLength(length)`. A guest `mapfn` drives through the callback
    /// channel like the prototype iterators do.
    fn detached_array_from(
        &mut self,
        args: &[Value],
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        let items = args.first().cloned().unwrap_or(Value::Undefined);
        if matches!(items, Value::Null | Value::Undefined) {
            return Err(crate::runtime::not_iterable_error(&items));
        }
        let mapfn = args
            .get(1)
            .cloned()
            .filter(|value| !matches!(value, Value::Undefined));
        let this_arg = args.get(2).cloned().unwrap_or(Value::Undefined);
        if let Some(mapfn) = &mapfn
            && !self.is_callable(mapfn)?
        {
            return Err(RuntimeError::type_error(format!(
                "{} is not a function",
                self.v8_value_text(mapfn)?
            )));
        }
        // The iterable sources keep each element's identity — Map/Set/
        // URLSearchParams iterate their entry lists, and an array copies
        // shallowly as `Array.from` does.
        let iterable: Option<Vec<Value>> = match &items {
            Value::Ref(id) => match self.heap.get(*id)? {
                HeapObject::List(items) | HeapObject::Tuple(items) => Some(items.clone()),
                HeapObject::RegExpMatch(result) => Some(result.items.clone()),
                HeapObject::Map(map) => Some(
                    map.entries
                        .iter()
                        .map(|(key, value)| Value::List(vec![key.clone(), value.clone()].into()))
                        .collect(),
                ),
                HeapObject::Set(set) => Some(set.values.clone()),
                HeapObject::UrlSearchParams(params) => Some(
                    params
                        .entries
                        .iter()
                        .map(|(name, value)| {
                            Value::List(
                                vec![Value::String(name.into()), Value::String(value.into())]
                                    .into(),
                            )
                        })
                        .collect(),
                ),
                _ => None,
            },
            Value::List(items) | Value::Tuple(items) => Some(items.to_vec()),
            Value::String(text) => Some(
                text.chars()
                    .map(|character| Value::String(character.to_string().into()))
                    .collect(),
            ),
            _ => None,
        };
        let (source, length) = match iterable {
            Some(elements) => {
                let length = elements.len() as u64;
                (Value::List(elements.into()), length)
            }
            // `ArrayCreate(len)` is the first step for the array-like source:
            // a `length` past the ECMA array limit is the RangeError, and a
            // surviving `length` is still guest-chosen, so the allocation the
            // dense answer needs is charged before any element exists.
            None => {
                let length = self.array_like_length(&items)?;
                if length > u32::MAX as u64 {
                    return Err(RuntimeError::range_error("Invalid array length"));
                }
                self.heap.ensure_list_allocation_len(length as usize)?;
                (items, length)
            }
        };
        let Some(mapfn) = mapfn else {
            let mut elements = Vec::with_capacity(length.min(usize::MAX as u64) as usize);
            if let Value::List(items) = &source {
                elements.extend(items.iter().cloned());
            } else {
                for index in 0..length {
                    elements.push(self.array_like_get(&source, &index.to_string())?);
                }
            }
            // The copy reads one element per member.
            self.charge_intrinsic_work(length.min(usize::MAX as u64) as usize);
            return self.complete_call(Value::List(elements.into()), return_target);
        };
        // `Call(mapfn, thisArg, «element, index»)` for every index — dense,
        // no `Has` gate, no receiver argument — so the walk answers a dense
        // `Collect` in visit order.
        let walk = ArrayLikeWalk {
            receiver: source,
            next: 0,
            length,
            descending: false,
            gated: false,
            omit_receiver: true,
        };
        self.begin_array_like_driver(
            mapfn,
            walk,
            CallbackCompletion::Collect,
            this_arg,
            return_target,
        )
    }
}

/// The `Object.prototype.toString` tag a non-callable built-in object wears:
/// `Math` is `[object Math]`, `Array.prototype` is `[object Array]`, the
/// constructor namespaces are plain objects.
fn builtin_object_tag(function: &BuiltinFunction) -> String {
    let name = function.qualified_name();
    let tag = match name.as_str() {
        // `Array.prototype` is itself an array, and `String`/`Number`/
        // `Boolean.prototype` carry their intrinsic slots — so they answer
        // their own tags. `Date`/`RegExp`/`Error.prototype` are ordinary
        // objects in modern ECMA and answer `[object Object]`.
        "Array.prototype" => "Array",
        "String.prototype" => "String",
        "Number.prototype" => "Number",
        "Boolean.prototype" => "Boolean",
        "Map.prototype" => "Map",
        "Set.prototype" => "Set",
        "URL.prototype" => "URL",
        "URLSearchParams.prototype" => "URLSearchParams",
        "Math" | "MathJSON" => "Math",
        "JSON" => "JSON",
        _ => "Object",
    };
    format!("[object {tag}]")
}

/// Node's text for a built-in that rejects an `undefined`/`null` receiver.
/// The class, a TypeError, is ECMA's; the wording is V8's, which differs by
/// family and, within `Array.prototype` and `Date.prototype`, by method.
fn undefined_receiver_message(prototype: BuiltinPrototype, name: &str, receiver: &Value) -> String {
    const TO_OBJECT: &str = "Cannot convert undefined or null to object";
    let owner = prototype.name();
    match prototype {
        BuiltinPrototype::Object => TO_OBJECT.to_string(),
        BuiltinPrototype::Function | BuiltinPrototype::Number | BuiltinPrototype::Boolean => {
            format!("{owner}.prototype.{name} requires that 'this' be a {owner}")
        }
        BuiltinPrototype::String => match name {
            "toString" | "valueOf" => {
                format!("String.prototype.{name} requires that 'this' be a String")
            }
            // V8 reports these two under their Annex B aliases.
            "trimEnd" => "String.prototype.trimRight called on null or undefined".to_string(),
            "trimStart" => "String.prototype.trimLeft called on null or undefined".to_string(),
            _ => format!("String.prototype.{name} called on null or undefined"),
        },
        BuiltinPrototype::Array => match name {
            "concat" | "every" | "filter" | "find" | "findIndex" | "findLast" | "findLastIndex"
            | "forEach" | "indexOf" | "map" | "reduce" | "reduceRight" | "some" => {
                format!("Array.prototype.{name} called on null or undefined")
            }
            _ => TO_OBJECT.to_string(),
        },
        BuiltinPrototype::Date => match name {
            "toString" | "toISOString" => {
                format!(
                    "Method Date.prototype.{name} called on incompatible receiver {}",
                    crate::runtime::value_type_name(receiver)
                )
            }
            "toJSON" => TO_OBJECT.to_string(),
            _ => "this is not a Date object.".to_string(),
        },
        BuiltinPrototype::Map
        | BuiltinPrototype::Set
        | BuiltinPrototype::RegExp
        | BuiltinPrototype::Error => {
            format!(
                "Method {owner}.prototype.{name} called on incompatible receiver {}",
                crate::runtime::value_type_name(receiver)
            )
        }
        BuiltinPrototype::Url => "Cannot read properties of undefined (reading 'URL')".to_string(),
        BuiltinPrototype::UrlSearchParams => {
            "Value of \"this\" must be of type URLSearchParams".to_string()
        }
    }
}

/// V8's text for a prototype method on a receiver of the wrong kind —
/// `String.prototype.toString` on a record, `Date.prototype.getTime` on an
/// array.
fn incompatible_receiver_message(
    prototype: BuiltinPrototype,
    name: &str,
    receiver: &Value,
    vm: &Vm<'_, impl ExecutionHost>,
) -> Result<String, RuntimeError> {
    let text = vm.v8_value_text(receiver)?;
    Ok(match prototype {
        BuiltinPrototype::String => {
            format!("String.prototype.{name} requires that 'this' be a String")
        }
        BuiltinPrototype::Number => {
            format!("Number.prototype.{name} requires that 'this' be a Number")
        }
        BuiltinPrototype::Boolean => {
            format!("Boolean.prototype.{name} requires that 'this' be a Boolean")
        }
        BuiltinPrototype::Date => "this is not a Date object.".to_string(),
        BuiltinPrototype::Map | BuiltinPrototype::Set | BuiltinPrototype::RegExp => {
            format!(
                "Method {}.prototype.{name} called on incompatible receiver {text}",
                prototype.name()
            )
        }
        BuiltinPrototype::Url => {
            format!("Method 'URL.prototype.{name}' called on incompatible receiver {text}")
        }
        BuiltinPrototype::UrlSearchParams => {
            "Value of \"this\" must be of type URLSearchParams".to_string()
        }
        _ => format!(
            "Method {}.prototype.{name} called on incompatible receiver {text}",
            prototype.name()
        ),
    })
}
