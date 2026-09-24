//! Built-in methods as values (FIG-3701): the reads that find one on a
//! value's prototype, and the calls whose callee is one.
//!
//! `const f = 'x'.includes` is `String.prototype.includes`, an ECMA function
//! object with no frame of its own. A plain call `f(...)` passes `undefined`
//! as its receiver, and every advertised built-in but one rejects that
//! receiver with a TypeError before it reads an argument. The exceptions are
//! the steps ECMA orders ahead of the receiver check, which run first so the
//! error a call throws is the one node throws.

use super::*;
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
        }
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

    /// Calls built-in `function` with `undefined` as its receiver and hands
    /// the answer to `return_target` as a returning frame would.
    pub(super) fn call_detached_builtin(
        &mut self,
        function: BuiltinFunction,
        args: &[Value],
        return_target: ReturnTarget,
    ) -> Result<(), RuntimeError> {
        let result = self.detached_builtin_result(function, args)?;
        self.complete_call(result, return_target)
    }

    /// What built-in `function` answers when its receiver is `undefined`.
    pub(super) fn detached_builtin_result(
        &mut self,
        function: BuiltinFunction,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        let prototype = function.prototype();
        let name = function.name();
        match (prototype, name) {
            // ECMA's Object.prototype.toString answers a tag for every
            // receiver, `undefined` included.
            (BuiltinPrototype::Object, "toString") => {
                return Ok(Value::String("[object Undefined]".into()));
            }
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
        Err(RuntimeError::IncompatibleReceiver {
            message: undefined_receiver_message(prototype, name),
        })
    }

    /// How V8 names a value inside a TypeError message: a primitive by its
    /// string, an array or a RegExp by its builtin tag, any other object by
    /// its constructor.
    fn v8_value_text(&self, value: &Value) -> Result<String, RuntimeError> {
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

    fn is_callable(&self, value: &Value) -> Result<bool, RuntimeError> {
        Ok(match value {
            Value::Ref(id) => self.heap.get(*id)?.is_function(),
            _ => false,
        })
    }
}

/// Node's text for a built-in that rejects an `undefined` receiver. The class,
/// a TypeError, is ECMA's; the wording is V8's, which differs by family and,
/// within `Array.prototype` and `Date.prototype`, by method.
fn undefined_receiver_message(prototype: BuiltinPrototype, name: &str) -> String {
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
                format!("Method Date.prototype.{name} called on incompatible receiver undefined")
            }
            "toJSON" => TO_OBJECT.to_string(),
            _ => "this is not a Date object.".to_string(),
        },
        BuiltinPrototype::Map
        | BuiltinPrototype::Set
        | BuiltinPrototype::RegExp
        | BuiltinPrototype::Error => {
            format!("Method {owner}.prototype.{name} called on incompatible receiver undefined")
        }
        BuiltinPrototype::Url => "Cannot read properties of undefined (reading 'URL')".to_string(),
        BuiltinPrototype::UrlSearchParams => {
            "Value of \"this\" must be of type URLSearchParams".to_string()
        }
    }
}
