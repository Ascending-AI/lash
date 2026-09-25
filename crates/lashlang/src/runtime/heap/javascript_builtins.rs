//! ECMA built-in constructors and namespaces as first-class values.
//!
//! Every built-in the dialect materializes — a constructor (`Number`), a
//! namespace (`Math`), a prototype object (`Number.prototype`), a static or
//! prototype method (`Math.max`, `Array.prototype.push`) — is a
//! `HeapObject::BuiltinFunction` interned by its `ecma_stdlib` row, so
//! identity, `typeof`, equality, GC and the durable wires all behave like
//! any other heap reference. The row carries the name, the owner scope, the
//! ECMA `length`, and whether the value is callable. The heap keeps the
//! side-tables the interned identity has no room for:
//!
//! ```text
//! builtin_expandos    HeapId → HeapId   guest-written own record
//! builtin_deleted     HeapId → names    statics `delete`d by the guest
//! builtin_enumerable  HeapId → names    expando keys `Object.keys` visits
//! ```
//!
//! The tables below state the *static* own surface beyond the row metadata:
//! the data constants, and the presence-only names no `Value` can carry.
//! Reading a method name answers the qualified built-in function value;
//! writing and deleting follow each property's ECMA attributes so
//! `verifyProperty`-style probes observe writable methods and frozen
//! constants.

use super::*;
use crate::runtime::access::{FUNCTION_PROTOTYPE_KEYS, is_object_prototype_key};

/// Whether `name` is an ECMA global a bare identifier may materialize —
/// exported for the lowerer, which cannot name the `ecma_stdlib` tables
/// itself.
pub fn is_javascript_builtin_global(name: &str) -> bool {
    BuiltinFunction::is_global_value(name)
}

/// `Number`'s and `Math`'s data constants: own, non-writable, non-enumerable,
/// non-configurable.
fn builtin_constant(name: &str, key: &str) -> Option<f64> {
    Some(match (name, key) {
        ("Number", "EPSILON") => f64::EPSILON,
        ("Number", "MAX_VALUE") => f64::MAX,
        ("Number", "MIN_VALUE") => 5e-324,
        ("Number", "NaN") => f64::NAN,
        ("Number", "NEGATIVE_INFINITY") => f64::NEG_INFINITY,
        ("Number", "POSITIVE_INFINITY") => f64::INFINITY,
        ("Number", "MAX_SAFE_INTEGER") => 9_007_199_254_740_991.0,
        ("Number", "MIN_SAFE_INTEGER") => -9_007_199_254_740_991.0,
        ("Math", "E") => std::f64::consts::E,
        ("Math", "PI") => std::f64::consts::PI,
        ("Math", "LN2") => std::f64::consts::LN_2,
        ("Math", "LN10") => std::f64::consts::LN_10,
        ("Math", "LOG2E") => std::f64::consts::LOG2_E,
        ("Math", "LOG10E") => std::f64::consts::LOG10_E,
        ("Math", "SQRT1_2") => std::f64::consts::FRAC_1_SQRT_2,
        ("Math", "SQRT2") => std::f64::consts::SQRT_2,
        _ => return None,
    })
}

/// One own property of a built-in object.
#[derive(Clone, Copy)]
enum BuiltinOwn {
    /// A data constant — `{writable: false, enumerable: false, configurable:
    /// false}`.
    Constant(f64),
    /// `length` — `{writable: false, enumerable: false, configurable: true}`.
    Length(f64),
    /// `name` — `{writable: false, enumerable: false, configurable: true}`.
    Name,
    /// `prototype` — `{writable: false, enumerable: false, configurable:
    /// false}`.
    Prototype,
    /// A method — `{writable: true, enumerable: false, configurable: true}`.
    /// Reads materialize a `Owner.method` built-in function value.
    Method,
    /// `Owner.prototype.constructor` — `{writable: true, enumerable: false,
    /// configurable: true}`, reading the owning constructor.
    PrototypeConstructor,
    /// A presence-only non-writable data name (`Symbol.iterator` and friends,
    /// which no `Value` can hold) — `{writable: false, enumerable: false,
    /// configurable: false}`.
    PresenceOnly,
}

impl BuiltinOwn {
    fn writable(self) -> bool {
        matches!(self, BuiltinOwn::Method | BuiltinOwn::PrototypeConstructor)
    }

    fn configurable(self) -> bool {
        matches!(
            self,
            BuiltinOwn::Length(_)
                | BuiltinOwn::Name
                | BuiltinOwn::Method
                | BuiltinOwn::PrototypeConstructor
        )
    }
}

/// The prototype-method surface a `Owner.prototype` built-in answers as own
/// properties, mirroring `access.rs`'s `in`-presence tables plus the
/// primitive-wrapper prototypes' methods.
fn builtin_prototype_surface(owner: &str) -> &'static [&'static str] {
    match owner {
        "Object" => crate::runtime::access::OBJECT_PROTOTYPE_KEYS,
        "Array" | "Array.prototype" => crate::runtime::access::ARRAY_PROTOTYPE_KEYS,
        "Number" => &[
            "constructor",
            "toExponential",
            "toFixed",
            "toLocaleString",
            "toPrecision",
            "toString",
            "valueOf",
        ],
        "String" => &[
            "at",
            "charAt",
            "charCodeAt",
            "codePointAt",
            "concat",
            "endsWith",
            "includes",
            "indexOf",
            "lastIndexOf",
            "localeCompare",
            "match",
            "matchAll",
            "normalize",
            "padEnd",
            "padStart",
            "repeat",
            "replace",
            "replaceAll",
            "search",
            "slice",
            "split",
            "startsWith",
            "substring",
            "toLocaleLowerCase",
            "toLocaleUpperCase",
            "toLowerCase",
            "toString",
            "toUpperCase",
            "trim",
            "trimEnd",
            "trimStart",
            "valueOf",
        ],
        "Boolean" => &["constructor", "toString", "valueOf"],
        "Error" | "AggregateError" | "EvalError" | "RangeError" | "ReferenceError"
        | "SyntaxError" | "TypeError" | "URIError" => {
            &["constructor", "message", "name", "toString"]
        }
        "Function" | "AsyncFunction" | "GeneratorFunction" => FUNCTION_PROTOTYPE_KEYS,
        "RegExp" => &[
            "compile",
            "dotAll",
            "exec",
            "flags",
            "global",
            "ignoreCase",
            "multiline",
            "source",
            "sticky",
            "test",
            "toString",
            "unicode",
            "unicodeSets",
        ],
        "Map" => &[
            "clear", "delete", "entries", "forEach", "get", "has", "keys", "set", "size", "values",
        ],
        "Set" => &[
            "add",
            "clear",
            "delete",
            "difference",
            "entries",
            "forEach",
            "has",
            "intersection",
            "isDisjointFrom",
            "isSubsetOf",
            "isSupersetOf",
            "keys",
            "size",
            "symmetricDifference",
            "union",
            "values",
        ],
        "URL" => &[
            "hash",
            "host",
            "hostname",
            "href",
            "origin",
            "password",
            "pathname",
            "port",
            "protocol",
            "search",
            "searchParams",
            "toJSON",
            "toString",
            "username",
        ],
        "URLSearchParams" => &[
            "append", "delete", "entries", "forEach", "get", "getAll", "has", "keys", "set",
            "size", "sort", "toString", "values",
        ],
        "Promise" => &["catch", "finally", "then"],
        "Symbol" => &["description", "toString", "valueOf"],
        "BigInt" => &["toLocaleString", "toString", "valueOf"],
        "ArrayBuffer" | "SharedArrayBuffer" => &[
            "byteLength",
            "maxByteLength",
            "resizable",
            "resize",
            "slice",
            "transfer",
        ],
        "DataView" => &[
            "buffer",
            "byteLength",
            "byteOffset",
            "getBigInt64",
            "getBigUint64",
            "getFloat16",
            "getFloat32",
            "getFloat64",
            "getInt8",
            "getInt16",
            "getInt32",
            "getUint8",
            "getUint16",
            "getUint32",
            "setBigInt64",
            "setBigUint64",
            "setFloat16",
            "setFloat32",
            "setFloat64",
            "setInt8",
            "setInt16",
            "setInt32",
            "setUint8",
            "setUint16",
            "setUint32",
        ],
        "WeakMap" => &["delete", "get", "has", "set"],
        "WeakSet" => &["add", "delete", "has"],
        "WeakRef" => &["deref"],
        "FinalizationRegistry" => &["register", "unregister"],
        _ => &[],
    }
}

/// The named own surface of `name` beyond `length`/`name`/`prototype`.
fn builtin_extra_own(name: &str, key: &str) -> Option<BuiltinOwn> {
    if let Some(constant) = builtin_constant(name, key) {
        return Some(BuiltinOwn::Constant(constant));
    }
    if let Some(owner) = name.strip_suffix(".prototype") {
        // `Owner.prototype`'s own surface: `constructor` plus the kind's
        // prototype methods.
        if key == "constructor" {
            return Some(BuiltinOwn::PrototypeConstructor);
        }
        if BuiltinFunction::named_qualified(&format!("{name}.{key}"))
            .is_some_and(|function| function.callable())
        {
            return Some(BuiltinOwn::Method);
        }
        if builtin_prototype_surface(owner).contains(&key) {
            return Some(BuiltinOwn::PresenceOnly);
        }
        return None;
    }
    if name.contains('.') {
        // A `Owner.method` function value: `length` and `name` only.
        return None;
    }
    // `Number.parseInt` IS the global `parseInt` in ECMA — one object, two
    // spellings — so the aliases read as methods without rows of their own.
    if name == "Number" && matches!(key, "parseInt" | "parseFloat") {
        return Some(BuiltinOwn::Method);
    }
    // A global's own statics are the object-scope rows it owns. Lookup is
    // deliberately *not* `named_scoped`: `Number.toString` is
    // `Function.prototype.toString` inherited, not `Number.prototype`'s —
    // the prototype preference would claim it an own property.
    if BuiltinFunction::named_static(name, key).is_some_and(|f| f.callable()) {
        return Some(BuiltinOwn::Method);
    }
    // `Error.stackTraceLimit` is an own data constant, not a method.
    if name == "Error" && key == "stackTraceLimit" {
        return Some(BuiltinOwn::Constant(10.0));
    }
    // The legacy `RegExp` statics are own data properties — the match
    // snapshots — not functions, and no `Value` carries their live text:
    // present, not readable.
    if name == "RegExp"
        && matches!(
            key,
            "$1" | "$2"
                | "$3"
                | "$4"
                | "$5"
                | "$6"
                | "$7"
                | "$8"
                | "$9"
                | "$_"
                | "$`"
                | "$&"
                | "$+"
                | "'"
                | "input"
                | "lastMatch"
                | "lastParen"
                | "leftContext"
                | "rightContext"
        )
    {
        return Some(BuiltinOwn::PresenceOnly);
    }
    // The well-known `Symbol` keys are own data constants no `Value` can
    // carry: present, not readable.
    if name == "Symbol"
        && matches!(
            key,
            "asyncDispose"
                | "asyncIterator"
                | "dispose"
                | "hasInstance"
                | "isConcatSpreadable"
                | "iterator"
                | "match"
                | "matchAll"
                | "replace"
                | "search"
                | "species"
                | "split"
                | "toPrimitive"
                | "toStringTag"
                | "unscopables"
        )
    {
        return Some(BuiltinOwn::PresenceOnly);
    }
    None
}

/// The constructors: the globals whose `prototype` is an own property.
/// `Math.max` has no `prototype`; `Number` does — and `Proxy` is the one
/// constructor without one.
const BUILTIN_CONSTRUCTOR_NAMES: &[&str] = &[
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "AsyncFunction",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "Error",
    "EvalError",
    "FinalizationRegistry",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Function",
    "GeneratorFunction",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "Iterator",
    "Map",
    "Number",
    "Object",
    "Promise",
    "RangeError",
    "ReferenceError",
    "RegExp",
    "Set",
    "SharedArrayBuffer",
    "String",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "URIError",
    "URL",
    "URLSearchParams",
    "WeakMap",
    "WeakRef",
    "WeakSet",
];

/// The static own surface: `length`, `name` and `prototype` where ECMA puts
/// them, then the per-built-in data and method names. `length`/`name` ride
/// on every callable — including `Function.prototype`, the one prototype
/// object that is itself a function.
fn builtin_own_property(name: &str, key: &str) -> Option<BuiltinOwn> {
    let function = BuiltinFunction::named_qualified(name)?;
    if name.ends_with(".prototype") {
        // `Owner.prototype` objects carry `constructor` and the kind's
        // prototype methods; `Function.prototype` adds the function slots.
        return match key {
            "length" if function.callable() => {
                Some(BuiltinOwn::Length(f64::from(function.length())))
            }
            "name" if function.callable() => Some(BuiltinOwn::Name),
            _ => builtin_extra_own(name, key),
        };
    }
    if name.contains('.') {
        // `Owner.method` function values: `length` and `name` only.
        return match key {
            "length" => Some(BuiltinOwn::Length(f64::from(function.length()))),
            "name" => Some(BuiltinOwn::Name),
            _ => None,
        };
    }
    match key {
        "prototype" if BUILTIN_CONSTRUCTOR_NAMES.contains(&name) => Some(BuiltinOwn::Prototype),
        "length" if function.callable() => Some(BuiltinOwn::Length(f64::from(function.length()))),
        "name" if function.callable() => Some(BuiltinOwn::Name),
        _ => builtin_extra_own(name, key),
    }
}

impl Heap {
    /// The qualified name behind `id` — `"Math"`, `"Math.max"`,
    /// `"Number.prototype"`, `"Array.prototype.push"` — when the object is a
    /// built-in.
    pub(crate) fn builtin_name(&self, id: HeapId) -> Option<String> {
        match self.get(id).ok()? {
            HeapObject::BuiltinFunction(function) => Some(function.qualified_name()),
            _ => None,
        }
    }

    pub(crate) fn is_builtin_object(&self, id: HeapId) -> bool {
        matches!(self.get(id), Ok(HeapObject::BuiltinFunction(_)))
    }

    /// `typeof` on a built-in: the constructors, methods and function-valued
    /// globals answer `"function"`; the namespaces and `Owner.prototype`
    /// objects answer `"object"`.
    pub(crate) fn builtin_is_callable(&self, id: HeapId) -> bool {
        matches!(self.get(id), Ok(HeapObject::BuiltinFunction(function)) if function.callable())
    }

    /// The canonical reference for a qualified built-in name, interned by its
    /// `ecma_stdlib` row. Built-ins are pinned at collection so the id — and
    /// therefore `===` identity — is stable for the life of the heap, and a
    /// wire restores the index from the objects it carries.
    pub(crate) fn builtin_value(&mut self, name: &str) -> Result<Value, RuntimeError> {
        // `Number.parseInt` IS the global `parseInt` in ECMA, and parseFloat
        // likewise: the two spellings materialize one object so `===` holds.
        let name = match name {
            "Number.parseInt" => "parseInt",
            "Number.parseFloat" => "parseFloat",
            name => name,
        };
        let Some(function) = BuiltinFunction::named_qualified(name) else {
            return Err(RuntimeError::ValidationFailed {
                reason: format!("`{name}` is not a materializable ECMA built-in"),
            });
        };
        self.builtin_function(function)
    }

    /// `Object.hasOwn(builtin, key)`: the guest's expandos plus every static
    /// name that has not been deleted.
    pub(crate) fn builtin_has_own(&self, id: HeapId, key: &str) -> Result<bool, RuntimeError> {
        let Some(name) = self.builtin_name(id) else {
            return Ok(false);
        };
        if let Some(own) = self.builtin_expandos.get(&id)
            && let HeapObject::Record(own) = self.get(*own)?
            && own.get(key).is_some()
        {
            return Ok(true);
        }
        if builtin_own_property(&name, key).is_none() {
            return Ok(false);
        }
        Ok(!self
            .builtin_deleted
            .get(&id)
            .is_some_and(|deleted| deleted.contains(key)))
    }

    /// `key in builtin`: own surface plus `Object.prototype`'s keys and, for
    /// callable built-ins, `Function.prototype`'s.
    pub(crate) fn builtin_has_property(&self, id: HeapId, key: &str) -> Result<bool, RuntimeError> {
        if self.builtin_has_own(id, key)? {
            return Ok(true);
        }
        let Some(name) = self.builtin_name(id) else {
            return Ok(false);
        };
        // `prototype` is an own slot on constructors only — `Function.prototype`
        // does not carry it, so `'prototype' in eval` answers false.
        let function_chain = self.builtin_is_callable(id)
            && FUNCTION_PROTOTYPE_KEYS.contains(&key)
            && !(key == "prototype"
                && !matches!(
                    builtin_own_property(&name, "prototype"),
                    Some(BuiltinOwn::Prototype)
                ));
        Ok(is_object_prototype_key(key) || function_chain)
    }

    /// The keys `for...in`/`Object.keys` visit: only guest-written enumerable
    /// expandos; the static surface is never enumerable.
    pub(crate) fn builtin_enumerable_keys(&self, id: HeapId) -> Result<Vec<String>, RuntimeError> {
        Ok(self
            .builtin_enumerable
            .get(&id)
            .cloned()
            .unwrap_or_default())
    }

    /// Reading `builtin.key`: an expando first, then the statics — methods
    /// read as `Owner.method` built-in function values — then the
    /// `Object.prototype`/`Function.prototype` names a reader can answer,
    /// and finally `constructor` per the built-in's kind.
    pub(crate) fn builtin_read(&mut self, id: HeapId, key: &str) -> Result<Value, RuntimeError> {
        let Some(name) = self.builtin_name(id) else {
            return Ok(Value::Undefined);
        };
        if let Some(own) = self.builtin_expandos.get(&id).copied()
            && let HeapObject::Record(own) = self.get(own)?
            && let Some(value) = own.get(key)
        {
            return Ok(value.clone());
        }
        let deleted = self
            .builtin_deleted
            .get(&id)
            .is_some_and(|deleted| deleted.contains(key));
        if !deleted {
            match builtin_own_property(&name, key) {
                Some(BuiltinOwn::Constant(value)) => return Ok(Value::Number(value)),
                Some(BuiltinOwn::Length(length)) => return Ok(Value::Number(length)),
                Some(BuiltinOwn::Name) => {
                    // `Function.prototype.name` is the empty string; every
                    // other callable answers its own `name`.
                    let own_name = if name.ends_with(".prototype") {
                        ""
                    } else {
                        name.rsplit('.').next().unwrap_or(&name)
                    };
                    return Ok(Value::String(own_name.into()));
                }
                Some(BuiltinOwn::Prototype) => {
                    return self.builtin_value(&format!("{}.prototype", name));
                }
                Some(BuiltinOwn::PrototypeConstructor) => {
                    let owner = name.strip_suffix(".prototype").unwrap_or(&name);
                    return self.builtin_value(owner);
                }
                Some(BuiltinOwn::Method) => {
                    return self.builtin_value(&format!("{}.{}", name, key));
                }
                Some(BuiltinOwn::PresenceOnly) => return Ok(Value::Undefined),
                None => {}
            }
        }
        self.builtin_inherited_read(id, key)
    }

    /// What the prototype chain answers for `key` when the built-in does not
    /// own it — an absent name, or one `delete` removed: a deleted `length`
    /// exposes `Function.prototype.length`, which is `0`. A callable built-in
    /// reads `Function.prototype` first (`length`/`name`/`constructor` and the
    /// method values), then `Object.prototype`; every other built-in reads
    /// `Object.prototype` only. `arguments`/`caller` on a callable are
    /// `Function.prototype`'s poisoned accessors: reading throws TypeError.
    fn builtin_inherited_read(&mut self, id: HeapId, key: &str) -> Result<Value, RuntimeError> {
        if self.builtin_is_callable(id) {
            match key {
                "length" => return Ok(Value::Number(0.0)),
                "name" => return Ok(Value::String("".into())),
                "constructor" => return self.builtin_value("Function"),
                "arguments" | "caller" => return Err(restricted_function_property()),
                _ => {
                    if let Some(function) = BuiltinFunction::named(BuiltinPrototype::Function, key)
                    {
                        return self.builtin_function(function);
                    }
                }
            }
        }
        match key {
            "constructor" => self.builtin_value("Object"),
            _ if is_object_prototype_key(key) => {
                if let Some(function) = BuiltinFunction::named(BuiltinPrototype::Object, key) {
                    return self.builtin_function(function);
                }
                Ok(Value::Undefined)
            }
            _ => Ok(Value::Undefined),
        }
    }

    /// `builtin.key = value`: a non-writable static throws the strict-mode
    /// `TypeError`; a writable static is shadowed by an own expando that
    /// keeps its non-enumerable attribute; a new name — or the re-creation
    /// of a deleted static — is an enumerable own property.
    pub(crate) fn builtin_assign(
        &mut self,
        id: HeapId,
        key: &str,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let Some(name) = self.builtin_name(id) else {
            return Err(RuntimeError::CannotAssignField {
                field: key.to_string(),
                actual: "function".to_string(),
            });
        };
        // `caller`/`arguments` on a callable are `Function.prototype`'s
        // poisoned accessors: writing throws the same TypeError reading does.
        if self.builtin_is_callable(id) && matches!(key, "caller" | "arguments") {
            return Err(restricted_function_property());
        }
        let is_own = match self.builtin_expandos.get(&id) {
            Some(own) => {
                matches!(self.get(*own)?, HeapObject::Record(own) if own.get(key).is_some())
            }
            None => false,
        };
        let descriptor = builtin_own_property(&name, key);
        let was_deleted = self
            .builtin_deleted
            .get(&id)
            .is_some_and(|deleted| deleted.contains(key));
        if !is_own && !was_deleted && matches!(descriptor, Some(property) if !property.writable()) {
            // The dialect's uniform strict mode: writing a non-writable
            // static throws the `TypeError` ECMA raises under `"use strict"`.
            return Err(RuntimeError::type_error(format!(
                "Cannot assign to read only property '{key}' of {name}"
            )));
        }
        if was_deleted {
            self.builtin_deleted
                .get_mut(&id)
                .map(|deleted| deleted.remove(key));
        }
        // A first-time store creates an enumerable own property; a shadowed
        // or restored static keeps the static's non-enumerable attribute —
        // except re-creating a deleted name, which ECMA treats as new.
        let enumerable = !is_own && (was_deleted || descriptor.is_none());
        if enumerable {
            self.builtin_enumerable
                .entry(id)
                .or_default()
                .push(key.to_string());
        }
        let own = self.builtin_expando_record(id)?;
        self.insert_record_key(own, key, value)
    }

    /// `delete builtin.key`: a configurable static is marked deleted (it is
    /// also dropped from the expando record if a write shadowed it); a
    /// non-configurable static throws the strict-mode `TypeError`; an
    /// expando removes like any record key.
    pub(crate) fn builtin_delete(&mut self, id: HeapId, key: &str) -> Result<bool, RuntimeError> {
        let Some(name) = self.builtin_name(id) else {
            return Ok(true);
        };
        if let Some(property) = builtin_own_property(&name, key) {
            let is_own = match self.builtin_expandos.get(&id) {
                Some(own) => {
                    matches!(self.get(*own)?, HeapObject::Record(own) if own.get(key).is_some())
                }
                None => false,
            };
            if !property.configurable() && !is_own {
                return Err(RuntimeError::type_error(format!(
                    "Cannot delete property '{key}' of {name}"
                )));
            }
            if is_own && let Some(&own) = self.builtin_expandos.get(&id) {
                self.remove_record_key(own, key)?;
            }
            self.builtin_deleted
                .entry(id)
                .or_default()
                .insert(key.to_string());
            if let Some(enumerable) = self.builtin_enumerable.get_mut(&id) {
                enumerable.retain(|enumerable| enumerable != key);
            }
            return Ok(true);
        }
        if let Some(own) = self.builtin_expandos.get(&id).copied()
            && matches!(self.get(own)?, HeapObject::Record(own) if own.get(key).is_some())
        {
            self.remove_record_key(own, key)?;
            if let Some(enumerable) = self.builtin_enumerable.get_mut(&id) {
                enumerable.retain(|enumerable| enumerable != key);
            }
        }
        Ok(true)
    }

    /// The `Constructor` name an object's `.constructor` answers, per ECMA:
    /// the wrapper kinds for primitives, `Object`/`Array`/`Function` for the
    /// structural kinds, the class for each exotic, and the error kind for an
    /// error. `None` leaves the ordinary field read to answer (`undefined`).
    pub(crate) fn javascript_constructor_of(
        &self,
        value: &Value,
    ) -> Result<Option<String>, RuntimeError> {
        Ok(match value {
            Value::String(_) => Some("String".to_string()),
            Value::Number(_) => Some("Number".to_string()),
            Value::Bool(_) => Some("Boolean".to_string()),
            Value::List(_) | Value::Tuple(_) => Some("Array".to_string()),
            Value::Record(_) => Some("Object".to_string()),
            Value::Ref(id) => match self.get(*id)? {
                HeapObject::Record(_) => Some("Object".to_string()),
                HeapObject::List(_) | HeapObject::Tuple(_) | HeapObject::RegExpMatch(_) => {
                    Some("Array".to_string())
                }
                HeapObject::Closure { .. } => Some("Function".to_string()),
                HeapObject::BuiltinFunction(function) => Some(
                    if function.callable() {
                        "Function"
                    } else {
                        "Object"
                    }
                    .to_string(),
                ),
                HeapObject::RegExp(_) => Some("RegExp".to_string()),
                HeapObject::Map(_) => Some("Map".to_string()),
                HeapObject::Set(_) => Some("Set".to_string()),
                HeapObject::Date(_) => Some("Date".to_string()),
                HeapObject::Error(error) => Some(error.kind.name().to_string()),
                HeapObject::Url(_) => Some("URL".to_string()),
                HeapObject::UrlSearchParams(_) => Some("URLSearchParams".to_string()),
                // A binding cell is never a guest value, so nothing reads a
                // member of one.
                HeapObject::Cell(_) => None,
            },
            _ => None,
        })
    }

    /// A record `Lash.Arguments` minted for a call frame. The mark lets the
    /// member paths answer the strict-mode `callee`/`caller` poison and keep
    /// `length`/`callee` out of enumeration.
    pub(crate) fn mark_arguments_record(&mut self, id: HeapId) {
        self.arguments_records.insert(id);
    }

    pub(crate) fn is_arguments_record(&self, id: HeapId) -> bool {
        self.arguments_records.contains(&id)
    }

    /// Closures must stand behind a compiled function.
    pub(crate) fn validate_closures(
        &self,
        functions: &[CompiledFunction],
    ) -> Result<(), RuntimeError> {
        for (_, object) in self.objects_in_id_order() {
            let HeapObject::Closure {
                function, captures, ..
            } = object
            else {
                continue;
            };
            let compiled = functions
                .get(*function as usize)
                .ok_or(RuntimeError::UnknownFunction { index: *function })?;
            if captures.len() != compiled.capture_count {
                return Err(RuntimeError::ClosureCaptureCountMismatch {
                    index: *function,
                    expected: compiled.capture_count,
                    actual: captures.len(),
                });
            }
        }
        Ok(())
    }

    /// The side-table rows keyed by `id` carry references `child_refs` cannot
    /// see: a built-in's expando record keeps what it stores, and a RegExp's
    /// raw `lastIndex` override roots the value it holds. They mark only when
    /// their owner does — an unreachable built-in is swept wholesale, map
    /// entry included, so `===` still cannot flap across a collection.
    pub(crate) fn collect_side_state_refs(&self, id: HeapId, pending: &mut Vec<HeapId>) {
        if let Some(own) = self.builtin_expandos.get(&id) {
            pending.push(*own);
        }
        if let Some(value) = self.regexp_last_index_overrides.get(&id) {
            collect_value_refs(value, pending);
        }
    }

    /// Drop the side-table rows whose objects were swept.
    pub(crate) fn sweep_builtin_side_state(&mut self, marked: &BTreeSet<HeapId>) {
        self.list_holes.retain(|id, _| marked.contains(id));
        self.regexp_last_index_overrides
            .retain(|id, _| marked.contains(id));
        self.builtin_expandos.retain(|id, _| marked.contains(id));
        self.builtin_deleted.retain(|id, _| marked.contains(id));
        self.builtin_enumerable.retain(|id, _| marked.contains(id));
        self.arguments_records.retain(|id| marked.contains(id));
    }

    /// The expando record for `id`, allocating it on the first write.
    fn builtin_expando_record(&mut self, id: HeapId) -> Result<HeapId, RuntimeError> {
        if let Some(own) = self.builtin_expandos.get(&id) {
            return Ok(*own);
        }
        let value = self.allocate_record(Record::new())?;
        let Value::Ref(own) = value else {
            return Err(RuntimeError::ValidationFailed {
                reason: "built-in surface records must allocate as heap refs".to_string(),
            });
        };
        self.builtin_expandos.insert(id, own);
        Ok(own)
    }

    fn insert_record_key(
        &mut self,
        id: HeapId,
        key: &str,
        value: Value,
    ) -> Result<(), RuntimeError> {
        let mut object = self.get(id)?.clone();
        let HeapObject::Record(record) = &mut object else {
            return Err(RuntimeError::ValidationFailed {
                reason: "built-in surface records must stay records".to_string(),
            });
        };
        record.insert_str(key, value);
        self.commit_object_update(id, object)
    }

    fn remove_record_key(&mut self, id: HeapId, key: &str) -> Result<(), RuntimeError> {
        let mut object = self.get(id)?.clone();
        let HeapObject::Record(record) = &mut object else {
            return Err(RuntimeError::ValidationFailed {
                reason: "built-in surface records must stay records".to_string(),
            });
        };
        record.remove(key);
        self.commit_object_update(id, object)
    }
}
