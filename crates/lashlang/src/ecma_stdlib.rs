//! The ECMA standard-library surface the VM implements, stated once as
//! prose signatures.
//!
//! This table is the dialect contract: `lash-typescript` renders it into
//! the model prompt and lowers from it, and the VM derives its
//! fixed-length normalization arities by parsing `arguments`
//! ([`signature_arity`]). Arity therefore has exactly one owner — the
//! prose — and the runtime's numeric view cannot drift from the
//! advertised contract.

/// Shorthand for the receiver-kind column of [`INSTANCE_STDLIB_SIGNATURES`],
/// so a signature row stays readable at one line.
use self::LiteralReceivers as On;

/// The literal receiver shapes an instance method may be called on.
///
/// A receiver written as a literal is the one case where the lowerer knows the
/// receiver's type outright, so it can refuse `[1].toUpperCase()` at compile
/// time instead of shaping-failing at run time. Which methods each shape
/// carries used to be a second hand-maintained table beside
/// [`INSTANCE_STDLIB_SIGNATURES`], and the two drifted: `valueOf` was listed
/// for every literal shape except arrays, so `[1].valueOf()` was refused while
/// the same call on a bound array was accepted (FIG-1718). Storing the answer
/// as a column on the signature row leaves one home for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiteralReceivers(u8);

impl LiteralReceivers {
    /// No literal receiver carries this method — it needs a value only a
    /// binding or constructor can produce (a `Map`, a `Set`, a `Date`, a
    /// `RegExp`, a `URLSearchParams`).
    pub const NONE: Self = Self(0);
    pub const STRING: Self = Self(1 << 0);
    pub const ARRAY: Self = Self(1 << 1);
    pub const NUMBER: Self = Self(1 << 2);
    /// Boolean, `null`, `undefined` and object literals. They share one row of
    /// the matrix because the dialect accepts the same three methods on all of
    /// them.
    pub const OTHER: Self = Self(1 << 3);
    pub const ALL: Self = Self(0b1111);

    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, kind: Self) -> bool {
        self.0 & kind.0 == kind.0
    }
}

/// One accepted standard-library call shape. `arguments` is intentionally
/// human-readable: this table is the dialect contract as well as the lowerer's
/// name inventory, so optional forms cannot hide behind a comment saying
/// "ECMA optional arguments".
///
/// `receivers` answers which literal receivers carry the method. It is
/// meaningful only for the instance table; a static call has an owner
/// namespace rather than a receiver, so those rows carry
/// [`LiteralReceivers::NONE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StdlibSignature {
    pub owner: &'static str,
    pub method: &'static str,
    pub arguments: &'static str,
    pub receivers: LiteralReceivers,
}

macro_rules! signatures {
    ($(($owner:literal, $method:literal, $arguments:literal)),+ $(,)?) => {
        &[$(StdlibSignature {
            owner: $owner,
            method: $method,
            arguments: $arguments,
            receivers: LiteralReceivers::NONE,
        }),+]
    };
}

macro_rules! instance_signatures {
    ($(($method:literal, $arguments:literal, $receivers:expr)),+ $(,)?) => {
        &[$(StdlibSignature {
            owner: "instance",
            method: $method,
            arguments: $arguments,
            receivers: $receivers,
        }),+]
    };
}

pub const STATIC_STDLIB_SIGNATURES: &[StdlibSignature] = signatures![
    ("Object", "keys", "value"),
    ("Object", "values", "value"),
    ("Object", "entries", "value"),
    ("Object", "fromEntries", "iterable"),
    ("Object", "assign", "target, ...sources"),
    ("Object", "groupBy", "iterable, callback"),
    ("Object", "hasOwn", "value, key"),
    ("Object", "is", "left, right"),
    ("Array", "from", "source[, mapFn[, thisArg]]"),
    ("Array", "isArray", "value"),
    ("Array", "of", "...values"),
    ("String", "fromCharCode", "...codeUnits"),
    ("String", "fromCodePoint", "...codePoints"),
    ("String", "raw", "template, ...substitutions"),
    ("Map", "groupBy", "iterable, callback"),
    ("Date", "parse", "value"),
    (
        "Date",
        "UTC",
        "year[, month[, date[, hours[, minutes[, seconds[, milliseconds]]]]]]"
    ),
    ("Number", "isFinite", "value"),
    ("Number", "isInteger", "value"),
    ("Number", "isNaN", "value"),
    ("Number", "isSafeInteger", "value"),
    ("Number", "parseFloat", "value"),
    ("Number", "parseInt", "value[, radix]"),
    ("JSON", "parse", "text"),
    ("JSON", "stringify", "[value[, replacer[, space]]]"),
    ("Math", "abs", "value"),
    ("Math", "acos", "value"),
    ("Math", "asin", "value"),
    ("Math", "acosh", "value"),
    ("Math", "asinh", "value"),
    ("Math", "atan", "value"),
    ("Math", "atan2", "y, x"),
    ("Math", "atanh", "value"),
    ("Math", "cbrt", "value"),
    ("Math", "ceil", "value"),
    ("Math", "clz32", "value"),
    ("Math", "cos", "value"),
    ("Math", "cosh", "value"),
    ("Math", "exp", "value"),
    ("Math", "expm1", "value"),
    ("Math", "floor", "value"),
    ("Math", "fround", "value"),
    ("Math", "hypot", "...values"),
    ("Math", "imul", "left, right"),
    ("Math", "log", "value"),
    ("Math", "log1p", "value"),
    ("Math", "log10", "value"),
    ("Math", "log2", "value"),
    ("Math", "round", "value"),
    ("Math", "sin", "value"),
    ("Math", "sinh", "value"),
    ("Math", "tan", "value"),
    ("Math", "tanh", "value"),
    ("Math", "trunc", "value"),
    ("Math", "max", "...values"),
    ("Math", "min", "...values"),
    ("Math", "pow", "base, exponent"),
    // A journaled host effect, not a computation: the draw is recorded on the
    // first execution and replayed exactly on every later one, which is the
    // same seam `Date.now()` uses. It is in the inventory because the call is
    // accepted; the note on it is what keeps the surface honest.
    ("Math", "random", ""),
    ("Math", "sqrt", "value"),
    ("Math", "sign", "value"),
    ("URL", "canParse", "input[, base]"),
];

pub const INSTANCE_STDLIB_SIGNATURES: &[StdlibSignature] = instance_signatures![
    ("at", "[index]", On::STRING.or(On::ARRAY)),
    ("concat", "...values", On::STRING.or(On::ARRAY)),
    ("charAt", "[index]", On::STRING),
    ("charCodeAt", "[index]", On::STRING),
    ("codePointAt", "[index]", On::STRING),
    ("append", "name, value", On::NONE),
    ("add", "value", On::NONE),
    ("clear", "", On::NONE),
    ("delete", "key[, value]", On::NONE),
    ("entries", "", On::NONE),
    ("exec", "input", On::NONE),
    ("endsWith", "search[, endPosition]", On::STRING),
    ("filter", "callback[, thisArg]", On::ARRAY),
    ("fill", "[value[, start[, end]]]", On::ARRAY),
    ("copyWithin", "[target[, start[, end]]]", On::ARRAY),
    ("find", "callback[, thisArg]", On::ARRAY),
    ("findIndex", "callback[, thisArg]", On::ARRAY),
    ("findLast", "callback[, thisArg]", On::ARRAY),
    ("findLastIndex", "callback[, thisArg]", On::ARRAY),
    ("flat", "[depth]", On::ARRAY),
    ("flatMap", "callback[, thisArg]", On::ARRAY),
    ("forEach", "callback[, thisArg]", On::ARRAY),
    ("get", "key", On::NONE),
    ("getAll", "name", On::NONE),
    ("has", "key[, value]", On::NONE),
    ("includes", "value[, fromIndex]", On::STRING.or(On::ARRAY)),
    ("indexOf", "value[, fromIndex]", On::STRING.or(On::ARRAY)),
    ("join", "[separator]", On::ARRAY),
    (
        "lastIndexOf",
        "value[, fromIndex]",
        On::STRING.or(On::ARRAY)
    ),
    ("map", "callback[, thisArg]", On::ARRAY),
    ("match", "regexp", On::STRING),
    ("matchAll", "globalRegExp", On::STRING),
    ("every", "callback[, thisArg]", On::ARRAY),
    ("padEnd", "length[, padString]", On::STRING),
    ("padStart", "length[, padString]", On::STRING),
    ("repeat", "[count]", On::STRING),
    (
        "replace",
        "[search[, replacement|callback(match, offset, string)]]",
        On::STRING
    ),
    (
        "replaceAll",
        "[searchString[, replacementString]]",
        On::STRING
    ),
    ("reduce", "callback[, initialValue]", On::ARRAY),
    ("reduceRight", "callback[, initialValue]", On::ARRAY),
    ("reverse", "", On::ARRAY),
    ("slice", "[start[, end]]", On::STRING.or(On::ARRAY)),
    ("sort", "[compareFn]", On::ARRAY),
    ("some", "callback[, thisArg]", On::ARRAY),
    ("splice", "[start[, deleteCount[, ...items]]]", On::ARRAY),
    ("push", "...values", On::ARRAY),
    ("pop", "", On::ARRAY),
    ("shift", "", On::ARRAY),
    ("unshift", "...values", On::ARRAY),
    ("split", "[separator[, limit]]", On::STRING),
    ("search", "regexp", On::STRING),
    ("startsWith", "search[, position]", On::STRING),
    ("substring", "[start[, end]]", On::STRING),
    ("toExponential", "[fractionDigits]", On::NUMBER),
    ("toFixed", "[digits]", On::NUMBER),
    ("toPrecision", "[precision]", On::NUMBER),
    ("toReversed", "", On::ARRAY),
    ("toSorted", "[compareFn]", On::ARRAY),
    ("toSpliced", "[start[, deleteCount[, ...items]]]", On::ARRAY),
    ("set", "key, value", On::NONE),
    ("keys", "", On::NONE),
    ("toLowerCase", "", On::STRING),
    ("toUpperCase", "", On::STRING),
    ("toString", "", On::ALL),
    ("trim", "", On::STRING),
    ("trimEnd", "", On::STRING),
    ("trimStart", "", On::STRING),
    ("test", "input", On::NONE),
    ("valueOf", "", On::ALL),
    ("values", "", On::NONE),
    ("with", "index, value", On::ARRAY),
    ("hasOwnProperty", "key", On::OTHER),
    ("union", "set", On::NONE),
    ("intersection", "set", On::NONE),
    ("difference", "set", On::NONE),
    ("symmetricDifference", "set", On::NONE),
    ("isSubsetOf", "set", On::NONE),
    ("isSupersetOf", "set", On::NONE),
    ("isDisjointFrom", "set", On::NONE),
    ("toJSON", "[key]", On::NONE),
    ("getTime", "", On::NONE),
    ("getUTCFullYear", "", On::NONE),
    ("getUTCMonth", "", On::NONE),
    ("getUTCDate", "", On::NONE),
    ("getUTCDay", "", On::NONE),
    ("getUTCHours", "", On::NONE),
    ("getUTCMinutes", "", On::NONE),
    ("getUTCSeconds", "", On::NONE),
    ("getUTCMilliseconds", "", On::NONE),
    ("toISOString", "", On::NONE),
    ("toUTCString", "", On::NONE),
];

/// The ECMA prototype objects whose advertised methods a value inherits.
///
/// The value model has no prototype objects (ADR 0062), but ECMA answers a
/// property read that misses a value's own properties from its prototype
/// chain, and that is where the advertised instance methods live. Each runtime
/// value kind names its prototype here, so a read of `'x'.includes` answers
/// `String.prototype.includes`: one function, whichever string it was read
/// from (FIG-3701).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BuiltinPrototype {
    Object,
    Function,
    Array,
    String,
    Number,
    Boolean,
    Map,
    Set,
    Date,
    RegExp,
    Error,
    Url,
    UrlSearchParams,
}

impl BuiltinPrototype {
    const ALL: [Self; 13] = [
        Self::Object,
        Self::Function,
        Self::Array,
        Self::String,
        Self::Number,
        Self::Boolean,
        Self::Map,
        Self::Set,
        Self::Date,
        Self::RegExp,
        Self::Error,
        Self::Url,
        Self::UrlSearchParams,
    ];

    /// The constructor whose `prototype` this is, as ECMA and the wire spell it.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Object => "Object",
            Self::Function => "Function",
            Self::Array => "Array",
            Self::String => "String",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::Map => "Map",
            Self::Set => "Set",
            Self::Date => "Date",
            Self::RegExp => "RegExp",
            Self::Error => "Error",
            Self::Url => "URL",
            Self::UrlSearchParams => "URLSearchParams",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|prototype| prototype.name() == name)
    }

    /// The next object on the chain: every one of these prototypes inherits
    /// from `Object.prototype`, which inherits from nothing.
    const fn parent(self) -> Option<Self> {
        match self {
            Self::Object => None,
            _ => Some(Self::Object),
        }
    }
}

/// What owns a built-in value.
///
/// A prototype for an instance method, or a named object for the rest of the
/// first-class surface (FIG-3656): `"globalThis"` names the global scope's
/// values — the constructors, namespaces and function-valued globals — while
/// `Object("Math")` owns the `Math.max` statics and `Object("Number")` owns
/// the `Number.prototype` object under the name `"prototype"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum BuiltinOwner {
    Prototype(BuiltinPrototype),
    Object(&'static str),
}

impl BuiltinOwner {
    /// The name a wire carries for the owner, which is also how a qualified
    /// built-in name is spelled: `"Array"` owns both `Array.prototype`'s
    /// methods and `Array`'s statics; `"globalThis"` owns the bare globals.
    pub(crate) const GLOBAL: &'static str = "globalThis";

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Prototype(prototype) => prototype.name(),
            Self::Object(name) => name,
        }
    }
}

/// One built-in object an advertised surface carries.
///
/// ECMA gives each one identity: `'a'.includes === 'b'.includes`, while
/// `'a'.includes !== [].includes`, since those are two functions on two
/// prototypes, and `Math === Math` holds across reads because `Math` is one
/// object. A built-in is therefore named by its owner and its own `name`,
/// never by the property key it was read through: `Set.prototype.keys`
/// *is* `Set.prototype.values`.
///
/// Not every row is callable: the namespaces and `Owner.prototype` objects
/// are built-ins too, so `typeof` consults `callable` — `Math` answers
/// `"object"` where `Math.max` answers `"function"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct BuiltinFunction(u16);

struct BuiltinFunctionRow {
    owner: BuiltinOwner,
    name: &'static str,
    /// ECMA's `length`: the count of required parameters the specification
    /// states, which is not the arity the signature prose advertises.
    /// Meaningful only on a callable row.
    length: u8,
    /// Whether ECMA gives the object a `[[Call]]` — what `typeof` and the
    /// call dispatch consult.
    callable: bool,
}

macro_rules! builtin_functions {
    ($(($prototype:expr, $name:literal, $length:literal)),+ $(,)?) => {
        &[$(BuiltinFunctionRow {
            owner: BuiltinOwner::Prototype($prototype),
            name: $name,
            length: $length,
            callable: true,
        }),+]
    };
}

macro_rules! builtin_objects {
    ($(($owner:expr, $name:literal, $length:literal, $callable:literal)),+ $(,)?) => {
        &[$(BuiltinFunctionRow {
            owner: BuiltinOwner::Object($owner),
            name: $name,
            length: $length,
            callable: $callable,
        }),+]
    };
}

/// Every built-in function a value can read, restricted to the advertised
/// instance methods: `name` and `length` are node v25's, per prototype.
/// A method outside [`INSTANCE_STDLIB_SIGNATURES`] is not readable here, so
/// the read surface cannot grow past the call surface.
const BUILTIN_FUNCTIONS: &[BuiltinFunctionRow] = {
    use BuiltinPrototype as P;
    builtin_functions![
        (P::Object, "hasOwnProperty", 1),
        (P::Object, "toString", 0),
        (P::Object, "valueOf", 0),
        (P::Function, "toString", 0),
        (P::Array, "at", 1),
        (P::Array, "concat", 1),
        (P::Array, "copyWithin", 2),
        (P::Array, "entries", 0),
        (P::Array, "every", 1),
        (P::Array, "fill", 1),
        (P::Array, "filter", 1),
        (P::Array, "find", 1),
        (P::Array, "findIndex", 1),
        (P::Array, "findLast", 1),
        (P::Array, "findLastIndex", 1),
        (P::Array, "flat", 0),
        (P::Array, "flatMap", 1),
        (P::Array, "forEach", 1),
        (P::Array, "includes", 1),
        (P::Array, "indexOf", 1),
        (P::Array, "join", 1),
        (P::Array, "keys", 0),
        (P::Array, "lastIndexOf", 1),
        (P::Array, "map", 1),
        (P::Array, "pop", 0),
        (P::Array, "push", 1),
        (P::Array, "reduce", 1),
        (P::Array, "reduceRight", 1),
        (P::Array, "reverse", 0),
        (P::Array, "shift", 0),
        (P::Array, "slice", 2),
        (P::Array, "some", 1),
        (P::Array, "sort", 1),
        (P::Array, "splice", 2),
        (P::Array, "toLocaleString", 0),
        (P::Array, "toReversed", 0),
        (P::Array, "toSorted", 1),
        (P::Array, "toSpliced", 2),
        (P::Array, "toString", 0),
        (P::Array, "unshift", 1),
        (P::Array, "values", 0),
        (P::Array, "with", 2),
        (P::String, "at", 1),
        (P::String, "charAt", 1),
        (P::String, "charCodeAt", 1),
        (P::String, "codePointAt", 1),
        (P::String, "concat", 1),
        (P::String, "endsWith", 1),
        (P::String, "includes", 1),
        (P::String, "indexOf", 1),
        (P::String, "lastIndexOf", 1),
        (P::String, "match", 1),
        (P::String, "matchAll", 1),
        (P::String, "padEnd", 1),
        (P::String, "padStart", 1),
        (P::String, "repeat", 1),
        (P::String, "replace", 2),
        (P::String, "replaceAll", 2),
        (P::String, "search", 1),
        (P::String, "slice", 2),
        (P::String, "split", 2),
        (P::String, "startsWith", 1),
        (P::String, "substring", 2),
        (P::String, "toLowerCase", 0),
        (P::String, "toString", 0),
        (P::String, "toUpperCase", 0),
        (P::String, "trim", 0),
        (P::String, "trimEnd", 0),
        (P::String, "trimStart", 0),
        (P::String, "valueOf", 0),
        (P::Number, "toExponential", 1),
        (P::Number, "toFixed", 1),
        (P::Number, "toPrecision", 1),
        (P::Number, "toString", 1),
        (P::Number, "valueOf", 0),
        (P::Boolean, "toString", 0),
        (P::Boolean, "valueOf", 0),
        (P::Map, "clear", 0),
        (P::Map, "delete", 1),
        (P::Map, "entries", 0),
        (P::Map, "forEach", 1),
        (P::Map, "get", 1),
        (P::Map, "has", 1),
        (P::Map, "keys", 0),
        (P::Map, "set", 2),
        (P::Map, "values", 0),
        (P::Set, "add", 1),
        (P::Set, "clear", 0),
        (P::Set, "delete", 1),
        (P::Set, "difference", 1),
        (P::Set, "entries", 0),
        (P::Set, "forEach", 1),
        (P::Set, "has", 1),
        (P::Set, "intersection", 1),
        (P::Set, "isDisjointFrom", 1),
        (P::Set, "isSubsetOf", 1),
        (P::Set, "isSupersetOf", 1),
        (P::Set, "symmetricDifference", 1),
        (P::Set, "union", 1),
        (P::Set, "values", 0),
        (P::Date, "getDate", 0),
        (P::Date, "getDay", 0),
        (P::Date, "getFullYear", 0),
        (P::Date, "getHours", 0),
        (P::Date, "getMilliseconds", 0),
        (P::Date, "getMinutes", 0),
        (P::Date, "getMonth", 0),
        (P::Date, "getSeconds", 0),
        (P::Date, "getTime", 0),
        (P::Date, "getTimezoneOffset", 0),
        (P::Date, "getUTCDate", 0),
        (P::Date, "getUTCDay", 0),
        (P::Date, "getUTCFullYear", 0),
        (P::Date, "getUTCHours", 0),
        (P::Date, "getUTCMilliseconds", 0),
        (P::Date, "getUTCMinutes", 0),
        (P::Date, "getUTCMonth", 0),
        (P::Date, "getUTCSeconds", 0),
        (P::Date, "getYear", 0),
        (P::Date, "setDate", 1),
        (P::Date, "setFullYear", 3),
        (P::Date, "setHours", 4),
        (P::Date, "setMilliseconds", 1),
        (P::Date, "setMinutes", 3),
        (P::Date, "setMonth", 2),
        (P::Date, "setSeconds", 2),
        (P::Date, "setTime", 1),
        (P::Date, "setUTCDate", 1),
        (P::Date, "setUTCFullYear", 3),
        (P::Date, "setUTCHours", 4),
        (P::Date, "setUTCMilliseconds", 1),
        (P::Date, "setUTCMinutes", 3),
        (P::Date, "setUTCMonth", 2),
        (P::Date, "setUTCSeconds", 2),
        (P::Date, "setYear", 1),
        (P::Date, "toDateString", 0),
        (P::Date, "toISOString", 0),
        (P::Date, "toJSON", 1),
        (P::Date, "toLocaleDateString", 0),
        (P::Date, "toLocaleString", 0),
        (P::Date, "toLocaleTimeString", 0),
        (P::Date, "toString", 0),
        (P::Date, "toTimeString", 0),
        (P::Date, "toUTCString", 0),
        (P::Date, "valueOf", 0),
        (P::RegExp, "exec", 1),
        (P::RegExp, "test", 1),
        (P::RegExp, "toString", 0),
        (P::Error, "toString", 0),
        (P::Url, "toJSON", 0),
        (P::Url, "toString", 0),
        (P::UrlSearchParams, "append", 2),
        (P::UrlSearchParams, "delete", 1),
        (P::UrlSearchParams, "entries", 0),
        (P::UrlSearchParams, "forEach", 1),
        (P::UrlSearchParams, "get", 1),
        (P::UrlSearchParams, "getAll", 1),
        (P::UrlSearchParams, "has", 1),
        (P::UrlSearchParams, "keys", 0),
        (P::UrlSearchParams, "set", 2),
        (P::UrlSearchParams, "sort", 0),
        (P::UrlSearchParams, "toString", 0),
        (P::UrlSearchParams, "values", 0),
        // Readable method values the call surface does not advertise: the
        // `Function.prototype` trio and `Object.prototype`'s legacy members.
        // Reading `f.bind` answers the one function object; calling it is a
        // `this`-binding question FIG-3700 owns, so the signatures stay shut.
        (P::Function, "apply", 2),
        (P::Function, "bind", 1),
        (P::Function, "call", 1),
        (P::Object, "isPrototypeOf", 1),
        (P::Object, "propertyIsEnumerable", 1),
        (P::Object, "toLocaleString", 0),
        (P::String, "localeCompare", 1),
        (P::String, "normalize", 0),
        (P::String, "toLocaleLowerCase", 0),
        (P::String, "toLocaleUpperCase", 0),
        (P::Number, "toLocaleString", 0),
        (P::RegExp, "compile", 1),
        (P::Object, "__defineGetter__", 2),
        (P::Object, "__defineSetter__", 2),
        (P::Object, "__lookupGetter__", 1),
        (P::Object, "__lookupSetter__", 1),
    ]
};

/// Instance-method names that are readable built-ins without being advertised
/// calls — the one sanctioned gap in the read/call surface invariant.
const READABLE_INSTANCE_EXTRAS: &[&str] = &[
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
    "apply",
    "bind",
    "call",
    "compile",
    // The `Date.prototype` methods a call still refuses — local time and
    // mutation are named refusals — but whose function values ECMA carries.
    "getDate",
    "getDay",
    "getFullYear",
    "getHours",
    "getMilliseconds",
    "getMinutes",
    "getMonth",
    "getSeconds",
    "getTimezoneOffset",
    "getYear",
    "isPrototypeOf",
    "localeCompare",
    "normalize",
    "propertyIsEnumerable",
    "setDate",
    "setFullYear",
    "setHours",
    "setMilliseconds",
    "setMinutes",
    "setMonth",
    "setSeconds",
    "setTime",
    "setUTCDate",
    "setUTCFullYear",
    "setUTCHours",
    "setUTCMilliseconds",
    "setUTCMinutes",
    "setUTCMonth",
    "setUTCSeconds",
    "setYear",
    "toDateString",
    "toGMTString",
    "toLocaleDateString",
    "toLocaleLowerCase",
    "toLocaleString",
    "toLocaleTimeString",
    "toLocaleUpperCase",
    "toTimeString",
];

/// The object-scope built-ins: the global constructors, namespaces and
/// function-valued globals under `"globalThis"`, every `Owner.prototype`
/// object, and the static methods each owner carries. `name`/`length` are
/// node v25's; `Number.parseInt` and `Number.parseFloat` are absent because
/// they are the global `parseInt`/`parseFloat` objects — the read aliases
/// them.
const BUILTIN_OBJECTS: &[BuiltinFunctionRow] = {
    const G: &str = BuiltinOwner::GLOBAL;
    builtin_objects![
        // Constructors and function-valued globals — `typeof` `"function"`.
        (G, "AggregateError", 2, true),
        (G, "Array", 1, true),
        (G, "ArrayBuffer", 1, true),
        (G, "AsyncFunction", 1, true),
        (G, "BigInt", 1, true),
        (G, "BigInt64Array", 3, true),
        (G, "BigUint64Array", 3, true),
        (G, "Boolean", 1, true),
        (G, "DataView", 1, true),
        (G, "Date", 7, true),
        (G, "Error", 1, true),
        (G, "EvalError", 1, true),
        (G, "FinalizationRegistry", 1, true),
        (G, "Float16Array", 3, true),
        (G, "Float32Array", 3, true),
        (G, "Float64Array", 3, true),
        (G, "Function", 1, true),
        (G, "GeneratorFunction", 1, true),
        (G, "Int16Array", 3, true),
        (G, "Int32Array", 3, true),
        (G, "Int8Array", 3, true),
        (G, "Iterator", 1, true),
        (G, "Map", 0, true),
        (G, "Number", 1, true),
        (G, "Object", 1, true),
        (G, "Promise", 1, true),
        (G, "Proxy", 2, true),
        (G, "RangeError", 1, true),
        (G, "ReferenceError", 1, true),
        (G, "RegExp", 2, true),
        (G, "Set", 0, true),
        (G, "SharedArrayBuffer", 1, true),
        (G, "String", 1, true),
        (G, "Symbol", 0, true),
        (G, "SyntaxError", 1, true),
        (G, "TypeError", 1, true),
        (G, "Uint16Array", 3, true),
        (G, "Uint32Array", 3, true),
        (G, "Uint8Array", 3, true),
        (G, "Uint8ClampedArray", 3, true),
        (G, "URIError", 1, true),
        (G, "URL", 1, true),
        (G, "URLSearchParams", 0, true),
        (G, "WeakMap", 0, true),
        (G, "WeakRef", 1, true),
        (G, "WeakSet", 0, true),
        (G, "atob", 1, true),
        (G, "btoa", 1, true),
        (G, "decodeURI", 1, true),
        (G, "decodeURIComponent", 1, true),
        (G, "encodeURI", 1, true),
        (G, "encodeURIComponent", 1, true),
        (G, "escape", 1, true),
        (G, "eval", 1, true),
        (G, "isFinite", 1, true),
        (G, "isNaN", 1, true),
        (G, "parseFloat", 1, true),
        (G, "parseInt", 2, true),
        (G, "structuredClone", 1, true),
        (G, "unescape", 1, true),
        // The namespaces — `typeof` `"object"`.
        (G, "Atomics", 0, false),
        (G, "Intl", 0, false),
        (G, "JSON", 0, false),
        (G, "Math", 0, false),
        (G, "Reflect", 0, false),
        // The `Owner.prototype` objects — `typeof` `"object"` except
        // `Function.prototype`, which is itself callable.
        ("AggregateError", "prototype", 0, false),
        ("Array", "prototype", 0, false),
        ("ArrayBuffer", "prototype", 0, false),
        ("AsyncFunction", "prototype", 0, false),
        ("BigInt", "prototype", 0, false),
        ("BigInt64Array", "prototype", 0, false),
        ("BigUint64Array", "prototype", 0, false),
        ("Boolean", "prototype", 0, false),
        ("DataView", "prototype", 0, false),
        ("Date", "prototype", 0, false),
        ("Error", "prototype", 0, false),
        ("EvalError", "prototype", 0, false),
        ("FinalizationRegistry", "prototype", 0, false),
        ("Float16Array", "prototype", 0, false),
        ("Float32Array", "prototype", 0, false),
        ("Float64Array", "prototype", 0, false),
        ("Function", "prototype", 0, true),
        ("GeneratorFunction", "prototype", 0, false),
        ("Int16Array", "prototype", 0, false),
        ("Int32Array", "prototype", 0, false),
        ("Int8Array", "prototype", 0, false),
        ("Iterator", "prototype", 0, false),
        ("Map", "prototype", 0, false),
        ("Number", "prototype", 0, false),
        ("Object", "prototype", 0, false),
        ("Promise", "prototype", 0, false),
        ("RangeError", "prototype", 0, false),
        ("ReferenceError", "prototype", 0, false),
        ("RegExp", "prototype", 0, false),
        ("Set", "prototype", 0, false),
        ("SharedArrayBuffer", "prototype", 0, false),
        ("String", "prototype", 0, false),
        ("Symbol", "prototype", 0, false),
        ("SyntaxError", "prototype", 0, false),
        ("TypeError", "prototype", 0, false),
        ("Uint16Array", "prototype", 0, false),
        ("Uint32Array", "prototype", 0, false),
        ("Uint8Array", "prototype", 0, false),
        ("Uint8ClampedArray", "prototype", 0, false),
        ("URIError", "prototype", 0, false),
        ("URL", "prototype", 0, false),
        ("URLSearchParams", "prototype", 0, false),
        ("WeakMap", "prototype", 0, false),
        ("WeakRef", "prototype", 0, false),
        ("WeakSet", "prototype", 0, false),
        // Constructor statics.
        ("Array", "from", 1, true),
        ("Array", "fromAsync", 1, true),
        ("Array", "isArray", 1, true),
        ("Array", "of", 0, true),
        ("ArrayBuffer", "isView", 1, true),
        ("BigInt", "asIntN", 2, true),
        ("BigInt", "asUintN", 2, true),
        ("Date", "UTC", 7, true),
        ("Date", "now", 0, true),
        ("Date", "parse", 1, true),
        ("Error", "captureStackTrace", 2, true),
        ("Error", "isError", 1, true),
        ("Iterator", "from", 1, true),
        ("Map", "groupBy", 2, true),
        ("Number", "isFinite", 1, true),
        ("Number", "isInteger", 1, true),
        ("Number", "isNaN", 1, true),
        ("Number", "isSafeInteger", 1, true),
        ("Object", "assign", 2, true),
        ("Object", "create", 2, true),
        ("Object", "defineProperties", 2, true),
        ("Object", "defineProperty", 3, true),
        ("Object", "entries", 1, true),
        ("Object", "freeze", 1, true),
        ("Object", "fromEntries", 1, true),
        ("Object", "getOwnPropertyDescriptor", 2, true),
        ("Object", "getOwnPropertyDescriptors", 1, true),
        ("Object", "getOwnPropertyNames", 1, true),
        ("Object", "getOwnPropertySymbols", 1, true),
        ("Object", "getPrototypeOf", 1, true),
        ("Object", "groupBy", 2, true),
        ("Object", "hasOwn", 2, true),
        ("Object", "is", 2, true),
        ("Object", "isExtensible", 1, true),
        ("Object", "isFrozen", 1, true),
        ("Object", "isSealed", 1, true),
        ("Object", "keys", 1, true),
        ("Object", "preventExtensions", 1, true),
        ("Object", "seal", 1, true),
        ("Object", "setPrototypeOf", 2, true),
        ("Object", "values", 1, true),
        ("Promise", "all", 1, true),
        ("Promise", "allSettled", 1, true),
        ("Promise", "any", 1, true),
        ("Promise", "race", 1, true),
        ("Promise", "reject", 1, true),
        ("Promise", "resolve", 1, true),
        ("Promise", "try", 1, true),
        ("Promise", "withResolvers", 0, true),
        ("Proxy", "revocable", 2, true),
        ("RegExp", "escape", 1, true),
        ("String", "fromCharCode", 1, true),
        ("String", "fromCodePoint", 1, true),
        ("String", "raw", 1, true),
        ("Symbol", "for", 1, true),
        ("Symbol", "keyFor", 1, true),
        ("URL", "canParse", 2, true),
        ("URL", "createObjectURL", 1, true),
        ("URL", "parse", 2, true),
        ("URL", "revokeObjectURL", 1, true),
        // Namespace members.
        ("Atomics", "add", 3, true),
        ("Atomics", "and", 3, true),
        ("Atomics", "compareExchange", 4, true),
        ("Atomics", "exchange", 3, true),
        ("Atomics", "isLockFree", 1, true),
        ("Atomics", "load", 2, true),
        ("Atomics", "notify", 2, true),
        ("Atomics", "or", 3, true),
        ("Atomics", "pause", 0, true),
        ("Atomics", "store", 3, true),
        ("Atomics", "sub", 3, true),
        ("Atomics", "wait", 4, true),
        ("Atomics", "waitAsync", 4, true),
        ("Atomics", "xor", 3, true),
        ("Intl", "Collator", 0, true),
        ("Intl", "DateTimeFormat", 0, true),
        ("Intl", "DisplayNames", 0, true),
        ("Intl", "DurationFormat", 0, true),
        ("Intl", "ListFormat", 0, true),
        ("Intl", "Locale", 0, true),
        ("Intl", "NumberFormat", 0, true),
        ("Intl", "PluralRules", 0, true),
        ("Intl", "RelativeTimeFormat", 0, true),
        ("Intl", "Segmenter", 0, true),
        ("Intl", "getCanonicalLocales", 1, true),
        ("Intl", "supportedValuesOf", 1, true),
        ("JSON", "isRawJSON", 1, true),
        ("JSON", "parse", 2, true),
        ("JSON", "rawJSON", 1, true),
        ("JSON", "stringify", 3, true),
        ("Math", "abs", 1, true),
        ("Math", "acos", 1, true),
        ("Math", "acosh", 1, true),
        ("Math", "asin", 1, true),
        ("Math", "asinh", 1, true),
        ("Math", "atan", 1, true),
        ("Math", "atan2", 2, true),
        ("Math", "atanh", 1, true),
        ("Math", "cbrt", 1, true),
        ("Math", "ceil", 1, true),
        ("Math", "clz32", 1, true),
        ("Math", "cos", 1, true),
        ("Math", "cosh", 1, true),
        ("Math", "exp", 1, true),
        ("Math", "expm1", 1, true),
        ("Math", "f16round", 1, true),
        ("Math", "floor", 1, true),
        ("Math", "fround", 1, true),
        ("Math", "hypot", 2, true),
        ("Math", "imul", 2, true),
        ("Math", "log", 1, true),
        ("Math", "log1p", 1, true),
        ("Math", "log2", 1, true),
        ("Math", "log10", 1, true),
        ("Math", "max", 2, true),
        ("Math", "min", 2, true),
        ("Math", "pow", 2, true),
        ("Math", "random", 0, true),
        ("Math", "round", 1, true),
        ("Math", "sign", 1, true),
        ("Math", "sin", 1, true),
        ("Math", "sinh", 1, true),
        ("Math", "sqrt", 1, true),
        ("Math", "tan", 1, true),
        ("Math", "tanh", 1, true),
        ("Math", "trunc", 1, true),
        ("Reflect", "apply", 3, true),
        ("Reflect", "construct", 2, true),
        ("Reflect", "defineProperty", 3, true),
        ("Reflect", "deleteProperty", 2, true),
        ("Reflect", "get", 2, true),
        ("Reflect", "getOwnPropertyDescriptor", 2, true),
        ("Reflect", "getPrototypeOf", 1, true),
        ("Reflect", "has", 2, true),
        ("Reflect", "isExtensible", 1, true),
        ("Reflect", "ownKeys", 1, true),
        ("Reflect", "preventExtensions", 1, true),
        ("Reflect", "set", 3, true),
        ("Reflect", "setPrototypeOf", 2, true),
        // Prototype methods whose owners have no `BuiltinPrototype` —
        // `Promise.prototype.then` reads as a function value even though no
        // heap object kind inherits it.
        ("Promise.prototype", "then", 2, true),
        ("Promise.prototype", "catch", 1, true),
        ("Promise.prototype", "finally", 1, true),
        ("Symbol.prototype", "toString", 0, true),
        ("Symbol.prototype", "valueOf", 0, true),
        ("BigInt.prototype", "toLocaleString", 0, true),
        ("BigInt.prototype", "toString", 1, true),
        ("BigInt.prototype", "valueOf", 0, true),
        ("ArrayBuffer.prototype", "resize", 1, true),
        ("ArrayBuffer.prototype", "slice", 2, true),
        ("ArrayBuffer.prototype", "transfer", 0, true),
        ("SharedArrayBuffer.prototype", "grow", 1, true),
        ("SharedArrayBuffer.prototype", "slice", 2, true),
        ("DataView.prototype", "getBigInt64", 1, true),
        ("DataView.prototype", "getBigUint64", 1, true),
        ("DataView.prototype", "getFloat16", 1, true),
        ("DataView.prototype", "getFloat32", 1, true),
        ("DataView.prototype", "getFloat64", 1, true),
        ("DataView.prototype", "getInt8", 1, true),
        ("DataView.prototype", "getInt16", 1, true),
        ("DataView.prototype", "getInt32", 1, true),
        ("DataView.prototype", "getUint8", 1, true),
        ("DataView.prototype", "getUint16", 1, true),
        ("DataView.prototype", "getUint32", 1, true),
        ("DataView.prototype", "setBigInt64", 2, true),
        ("DataView.prototype", "setBigUint64", 2, true),
        ("DataView.prototype", "setFloat16", 2, true),
        ("DataView.prototype", "setFloat32", 2, true),
        ("DataView.prototype", "setFloat64", 2, true),
        ("DataView.prototype", "setInt8", 2, true),
        ("DataView.prototype", "setInt16", 2, true),
        ("DataView.prototype", "setInt32", 2, true),
        ("DataView.prototype", "setUint8", 2, true),
        ("DataView.prototype", "setUint16", 2, true),
        ("DataView.prototype", "setUint32", 2, true),
        ("WeakMap.prototype", "delete", 1, true),
        ("WeakMap.prototype", "get", 1, true),
        ("WeakMap.prototype", "has", 1, true),
        ("WeakMap.prototype", "set", 2, true),
        ("WeakSet.prototype", "add", 1, true),
        ("WeakSet.prototype", "delete", 1, true),
        ("WeakSet.prototype", "has", 1, true),
        ("WeakRef.prototype", "deref", 0, true),
        ("FinalizationRegistry.prototype", "register", 2, true),
        ("FinalizationRegistry.prototype", "unregister", 1, true),
        ("AsyncFunction.prototype", "apply", 2, true),
        ("AsyncFunction.prototype", "bind", 1, true),
        ("AsyncFunction.prototype", "call", 1, true),
        ("AsyncFunction.prototype", "toString", 0, true),
        ("GeneratorFunction.prototype", "apply", 2, true),
        ("GeneratorFunction.prototype", "bind", 1, true),
        ("GeneratorFunction.prototype", "call", 1, true),
        ("GeneratorFunction.prototype", "toString", 0, true),
    ]
};

/// Prototype members that are the same function object under two names —
/// `(owner, alias, real)` — `Set.prototype.keys` is `Set.prototype.values`,
/// and `Date.prototype.toGMTString` is `Date.prototype.toUTCString`.
const BUILTIN_FUNCTION_ALIASES: &[(BuiltinPrototype, &str, &str)] = &[
    (BuiltinPrototype::Date, "toGMTString", "toUTCString"),
    (BuiltinPrototype::Set, "keys", "values"),
];

const fn is_advertised_instance_method(method: &str) -> bool {
    let mut i = 0;
    while i < INSTANCE_STDLIB_SIGNATURES.len() {
        if str_eq(INSTANCE_STDLIB_SIGNATURES[i].method, method) {
            return true;
        }
        i += 1;
    }
    false
}

const fn is_readable_instance_extra(method: &str) -> bool {
    let mut i = 0;
    while i < READABLE_INSTANCE_EXTRAS.len() {
        if str_eq(READABLE_INSTANCE_EXTRAS[i], method) {
            return true;
        }
        i += 1;
    }
    false
}

// A readable built-in the call surface does not advertise would be a method
// value no member call can reach, so the table is held to the signatures at
// compile time, alias keys and the readable extras included.
const _: () = {
    assert!(BUILTIN_FUNCTIONS.len() + BUILTIN_OBJECTS.len() <= u16::MAX as usize);
    let mut i = 0;
    while i < BUILTIN_FUNCTIONS.len() {
        let name = BUILTIN_FUNCTIONS[i].name;
        assert!(
            is_advertised_instance_method(name) || is_readable_instance_extra(name),
            "a readable built-in function is not an advertised instance method"
        );
        i += 1;
    }
    let mut i = 0;
    while i < BUILTIN_FUNCTION_ALIASES.len() {
        assert!(
            is_advertised_instance_method(BUILTIN_FUNCTION_ALIASES[i].1)
                || is_readable_instance_extra(BUILTIN_FUNCTION_ALIASES[i].1),
            "a built-in alias key is not an advertised instance method"
        );
        i += 1;
    }
};

impl BuiltinFunction {
    fn row(self) -> &'static BuiltinFunctionRow {
        let index = usize::from(self.0);
        if index < BUILTIN_FUNCTIONS.len() {
            &BUILTIN_FUNCTIONS[index]
        } else {
            &BUILTIN_OBJECTS[index - BUILTIN_FUNCTIONS.len()]
        }
    }

    /// What owns the object: a prototype for an instance method, a named
    /// object for a static or a `Owner.prototype`, or the global scope.
    pub(crate) fn owner(self) -> BuiltinOwner {
        self.row().owner
    }

    /// The owning prototype, when the built-in is an instance method.
    pub(crate) fn prototype(self) -> Option<BuiltinPrototype> {
        match self.owner() {
            BuiltinOwner::Prototype(prototype) => Some(prototype),
            BuiltinOwner::Object(_) => None,
        }
    }

    /// Whether ECMA gives the object a `[[Call]]`: the constructors, methods
    /// and function-valued globals answer `true`; the namespaces and
    /// `Owner.prototype` objects answer `false` — what `typeof` reports.
    pub(crate) fn callable(self) -> bool {
        self.row().callable
    }

    /// ECMA's `name` of the function object.
    pub(crate) fn name(self) -> &'static str {
        self.row().name
    }

    /// ECMA's `length` of the function object.
    pub(crate) fn length(self) -> u8 {
        self.row().length
    }

    /// The name the surface metadata and diagnostics spell: `"Math"` for a
    /// namespace, `"Math.max"` for a static, `"Number.prototype"` for a
    /// prototype object, `"Number.prototype.toString"` for a method.
    pub(crate) fn qualified_name(self) -> String {
        match self.owner() {
            BuiltinOwner::Prototype(prototype) => {
                format!("{}.prototype.{}", prototype.name(), self.name())
            }
            BuiltinOwner::Object(owner) if owner == BuiltinOwner::GLOBAL => self.name().to_string(),
            BuiltinOwner::Object(owner) => format!("{owner}.{}", self.name()),
        }
    }

    /// The function `prototype` carries under its own `name`.
    pub(crate) fn named(prototype: BuiltinPrototype, name: &str) -> Option<Self> {
        BUILTIN_FUNCTIONS
            .iter()
            .position(|row| row.owner == BuiltinOwner::Prototype(prototype) && row.name == name)
            .map(|index| Self(index as u16))
    }

    /// The built-in an owner scope carries under `name` — how a wire names
    /// one. `scope` is a prototype name for an instance method, an owner name
    /// for a static or a `Owner.prototype` object, or `"globalThis"` for a
    /// global. A prototype scope wins the lookup: `("Number", "toString")` is
    /// `Number.prototype.toString`, and no static shadows an instance method
    /// of the same owner.
    pub(crate) fn named_scoped(scope: &str, name: &str) -> Option<Self> {
        if let Some(prototype) = BuiltinPrototype::from_name(scope)
            && let Some(function) = Self::named(prototype, name)
        {
            return Some(function);
        }
        Self::named_static(scope, name)
    }

    /// The built-in an owner scope carries under `name`, looking only at the
    /// object-scope rows — no prototype preference. A surface asks this when
    /// it needs a *static* specifically: `Number.toString` is inherited from
    /// `Function.prototype`, not an own `Number.prototype.toString` row.
    pub(crate) fn named_static(scope: &str, name: &str) -> Option<Self> {
        BUILTIN_OBJECTS
            .iter()
            .position(|row| {
                matches!(row.owner, BuiltinOwner::Object(owner) if owner == scope)
                    && row.name == name
            })
            .map(|index| Self((index + BUILTIN_FUNCTIONS.len()) as u16))
    }

    /// The built-in a qualified name spells: `"Math.max"` is
    /// `Object("Math")`'s `"max"`, `"Number.prototype"` is `Object("Number")`'s
    /// `"prototype"`, `"Number.prototype.toString"` is the `Number` prototype's
    /// `"toString"` method, and a bare `"Number"` is a global. A
    /// `"Owner.prototype.member"` whose owner is not a [`BuiltinPrototype`]
    /// lives under the `"Owner.prototype"` object scope.
    pub(crate) fn named_qualified(name: &str) -> Option<Self> {
        if let Some((owner, member)) = name.split_once(".prototype.") {
            if let Some(prototype) = BuiltinPrototype::from_name(owner) {
                // `Set.prototype.keys` and `Date.prototype.toGMTString` are
                // the aliased members: the one function object under two
                // property keys.
                let member = BUILTIN_FUNCTION_ALIASES
                    .iter()
                    .find(|(owner, alias, _)| *owner == prototype && *alias == member)
                    .map_or(member, |(_, _, real)| *real);
                if let Some(function) = Self::named(prototype, member) {
                    return Some(function);
                }
            }
            return Self::named_static(&format!("{owner}.prototype"), member);
        }
        match name.rsplit_once('.') {
            Some((scope, leaf)) => Self::named_scoped(scope, leaf),
            None => Self::named_scoped(BuiltinOwner::GLOBAL, name),
        }
    }

    /// Whether a bare identifier may materialize a global built-in value:
    /// every global-scope row except `AsyncFunction`/`GeneratorFunction`,
    /// which exist only as the `.constructor` of the functions they built —
    /// a bare identifier stays unbound, like Node.
    pub(crate) fn is_global_value(name: &str) -> bool {
        !matches!(name, "AsyncFunction" | "GeneratorFunction")
            && Self::named_scoped(BuiltinOwner::GLOBAL, name).is_some()
    }

    /// Whether any prototype carries a built-in under property key `key`: the
    /// cheap question a read asks before it looks for the receiver's prototype.
    pub(crate) fn is_method_key(key: &str) -> bool {
        BUILTIN_FUNCTIONS.iter().any(|row| row.name == key)
            || BUILTIN_FUNCTION_ALIASES
                .iter()
                .any(|(_, alias, _)| *alias == key)
    }

    /// The function a property read of `key` finds on the chain that starts
    /// at `prototype`, once the value's own properties have missed.
    pub(crate) fn inherited(prototype: BuiltinPrototype, key: &str) -> Option<Self> {
        let mut prototype = Some(prototype);
        while let Some(current) = prototype {
            let name = BUILTIN_FUNCTION_ALIASES
                .iter()
                .find(|(owner, alias, _)| *owner == current && *alias == key)
                .map_or(key, |(_, _, name)| *name);
            if let Some(function) = Self::named(current, name) {
                return Some(function);
            }
            prototype = current.parent();
        }
        None
    }
}

/// The fixed positional arity a signature row declares, or `None` when the
/// row is variadic (`...rest`).
///
/// Derived by counting the parameters the prose names: commas inside
/// parentheses do not separate parameters — `replace`'s
/// `replacement|callback(match, offset, string)` alternative is one
/// parameter — while every comma at parenthesis depth zero does, however
/// deeply `[optional]` brackets nest it.
pub(crate) const fn signature_arity(arguments: &str) -> Option<usize> {
    let bytes = arguments.as_bytes();
    if bytes.is_empty() {
        return Some(0);
    }
    let mut parens = 0usize;
    let mut parameters = 1usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'.' if i + 2 < bytes.len() && bytes[i + 1] == b'.' && bytes[i + 2] == b'.' => {
                return None;
            }
            b'(' => parens += 1,
            b')' => parens = parens.saturating_sub(1),
            b',' if parens == 0 => parameters += 1,
            _ => {}
        }
        i += 1;
    }
    Some(parameters)
}

/// `str` equality usable in `const` contexts, where `==` is not yet
/// available.
const fn str_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut i = 0;
    while i < left.len() {
        if left[i] != right[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `method` is an instance method of the advertised surface. The
/// runtime asks this on every standard-library call, so the names are sorted
/// once and searched rather than scanned.
pub(crate) fn is_instance_method(method: &str) -> bool {
    static METHODS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    METHODS
        .get_or_init(|| {
            let mut methods = INSTANCE_STDLIB_SIGNATURES
                .iter()
                .map(|signature| signature.method)
                .collect::<Vec<_>>();
            methods.sort_unstable();
            methods.dedup();
            methods
        })
        .binary_search(&method)
        .is_ok()
}

/// The declared arity of an instance method that takes a fixed number of
/// arguments — `None` for variadic rows and for names outside the surface.
pub(crate) const fn instance_method_arity(method: &str) -> Option<usize> {
    let mut i = 0;
    while i < INSTANCE_STDLIB_SIGNATURES.len() {
        let signature = &INSTANCE_STDLIB_SIGNATURES[i];
        if str_eq(signature.method, method) {
            return signature_arity(signature.arguments);
        }
        i += 1;
    }
    None
}

/// The declared arity of a static call spelled `"Owner.method"`, under the
/// same rule as [`instance_method_arity`].
pub(crate) const fn static_method_arity(name: &str) -> Option<usize> {
    let bytes = name.as_bytes();
    let mut dot = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            dot = i;
            break;
        }
        i += 1;
    }
    if i == bytes.len() {
        return None;
    }
    let mut j = 0;
    while j < STATIC_STDLIB_SIGNATURES.len() {
        let signature = &STATIC_STDLIB_SIGNATURES[j];
        let owner = signature.owner.as_bytes();
        let method = signature.method.as_bytes();
        if owner.len() == dot
            && method.len() == bytes.len() - dot - 1
            && {
                let mut k = 0;
                while k < owner.len() {
                    if owner[k] != bytes[k] {
                        break;
                    }
                    k += 1;
                }
                k == owner.len()
            }
            && {
                let mut k = 0;
                while k < method.len() {
                    if method[k] != bytes[dot + 1 + k] {
                        break;
                    }
                    k += 1;
                }
                k == method.len()
            }
        {
            return signature_arity(signature.arguments);
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prose grammar the arity parse reads: nested `[optional]` groups
    /// still separate parameters, commas inside a `|` alternative's parens do
    /// not, and `...` marks the row variadic.
    #[test]
    fn signature_arity_counts_positional_parameters() {
        let cases: &[(&str, Option<usize>)] = &[
            ("", Some(0)),
            ("value", Some(1)),
            ("[index]", Some(1)),
            ("y, x", Some(2)),
            ("value[, radix]", Some(2)),
            ("search[, endPosition]", Some(2)),
            ("[separator[, limit]]", Some(2)),
            ("key, value", Some(2)),
            ("source[, mapFn[, thisArg]]", Some(3)),
            ("[value[, start[, end]]]", Some(3)),
            ("[value[, replacer[, space]]]", Some(3)),
            ("callback[, thisArg]", Some(2)),
            (
                "[search[, replacement|callback(match, offset, string)]]",
                Some(2),
            ),
            (
                "year[, month[, date[, hours[, minutes[, seconds[, milliseconds]]]]]]",
                Some(7),
            ),
            ("...values", None),
            ("target, ...sources", None),
            ("[start[, deleteCount[, ...items]]]", None),
        ];
        for (arguments, arity) in cases {
            assert_eq!(signature_arity(arguments), *arity, "{arguments}");
        }
    }

    /// Every advertised instance method is some prototype's function, so no
    /// method the call surface names reads as `undefined`; and no two rows
    /// name one function, since a function is its prototype and name.
    #[test]
    fn every_advertised_instance_method_is_a_readable_builtin() {
        for signature in INSTANCE_STDLIB_SIGNATURES {
            assert!(
                BuiltinPrototype::ALL
                    .into_iter()
                    .any(
                        |prototype| BuiltinFunction::inherited(prototype, signature.method)
                            .is_some()
                    ),
                "{} is advertised but no prototype carries it",
                signature.method
            );
        }
        for (index, row) in BUILTIN_FUNCTIONS.iter().enumerate() {
            let BuiltinOwner::Prototype(prototype) = row.owner else {
                panic!("an instance-method row must name a prototype");
            };
            assert_eq!(
                BuiltinFunction::named(prototype, row.name),
                Some(BuiltinFunction(index as u16)),
                "{}.prototype.{} is listed twice",
                prototype.name(),
                row.name
            );
            assert_eq!(
                BuiltinPrototype::from_name(prototype.name()),
                Some(prototype)
            );
        }
        // The object-scope rows round-trip through their qualified names,
        // and no two of them spell the same one.
        for (index, row) in BUILTIN_OBJECTS.iter().enumerate() {
            let function = BuiltinFunction(BUILTIN_FUNCTIONS.len() as u16 + index as u16);
            assert_eq!(
                BuiltinFunction::named_qualified(&function.qualified_name()),
                Some(function),
                "{} is listed twice",
                function.qualified_name()
            );
            let BuiltinOwner::Object(scope) = row.owner else {
                panic!("an object row must name an owner scope");
            };
            assert_eq!(
                BuiltinFunction::named_scoped(scope, row.name),
                Some(function),
                "{}.{} is listed twice",
                scope,
                row.name
            );
        }
    }

    /// Every row in both tables parses — the derivation has no silent
    /// fallback, so a malformed `arguments` string would be caught here.
    #[test]
    fn every_signature_row_parses_an_arity() {
        for signature in STATIC_STDLIB_SIGNATURES
            .iter()
            .chain(INSTANCE_STDLIB_SIGNATURES)
        {
            let arity = signature_arity(signature.arguments);
            assert!(
                arity.is_some() || signature.arguments.contains("..."),
                "{}.{} ({})",
                signature.owner,
                signature.method,
                signature.arguments
            );
        }
    }
}
