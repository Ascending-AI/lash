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

/// One built-in function object an advertised prototype carries.
///
/// ECMA gives each one identity: `'a'.includes === 'b'.includes`, while
/// `'a'.includes !== [].includes`, since those are two functions on two
/// prototypes. A function is therefore named by its prototype and its own
/// `name`, never by the property key it was read through: `Set.prototype.keys`
/// *is* `Set.prototype.values`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct BuiltinFunction(u16);

struct BuiltinFunctionRow {
    prototype: BuiltinPrototype,
    name: &'static str,
    /// ECMA's `length`: the count of required parameters the specification
    /// states, which is not the arity the signature prose advertises.
    length: u8,
}

macro_rules! builtin_functions {
    ($(($prototype:expr, $name:literal, $length:literal)),+ $(,)?) => {
        &[$(BuiltinFunctionRow {
            prototype: $prototype,
            name: $name,
            length: $length,
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
        (P::Date, "getTime", 0),
        (P::Date, "getUTCDate", 0),
        (P::Date, "getUTCDay", 0),
        (P::Date, "getUTCFullYear", 0),
        (P::Date, "getUTCHours", 0),
        (P::Date, "getUTCMilliseconds", 0),
        (P::Date, "getUTCMinutes", 0),
        (P::Date, "getUTCMonth", 0),
        (P::Date, "getUTCSeconds", 0),
        (P::Date, "toISOString", 0),
        (P::Date, "toJSON", 1),
        (P::Date, "toString", 0),
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
    ]
};

/// Property keys whose value is a function registered under another name.
const BUILTIN_FUNCTION_ALIASES: &[(BuiltinPrototype, &str, &str)] =
    &[(BuiltinPrototype::Set, "keys", "values")];

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

// A readable built-in the call surface does not advertise would be a method
// value no member call can reach, so the table is held to the signatures at
// compile time, alias keys included.
const _: () = {
    assert!(BUILTIN_FUNCTIONS.len() <= u16::MAX as usize);
    let mut i = 0;
    while i < BUILTIN_FUNCTIONS.len() {
        assert!(
            is_advertised_instance_method(BUILTIN_FUNCTIONS[i].name),
            "a readable built-in function is not an advertised instance method"
        );
        i += 1;
    }
    let mut i = 0;
    while i < BUILTIN_FUNCTION_ALIASES.len() {
        assert!(
            is_advertised_instance_method(BUILTIN_FUNCTION_ALIASES[i].1),
            "a built-in alias key is not an advertised instance method"
        );
        i += 1;
    }
};

impl BuiltinFunction {
    fn row(self) -> &'static BuiltinFunctionRow {
        &BUILTIN_FUNCTIONS[usize::from(self.0)]
    }

    pub(crate) fn prototype(self) -> BuiltinPrototype {
        self.row().prototype
    }

    /// ECMA's `name` of the function object.
    pub(crate) fn name(self) -> &'static str {
        self.row().name
    }

    /// ECMA's `length` of the function object.
    pub(crate) fn length(self) -> u8 {
        self.row().length
    }

    /// The function `prototype` carries under its own `name`, which is how a
    /// wire names one.
    pub(crate) fn named(prototype: BuiltinPrototype, name: &str) -> Option<Self> {
        BUILTIN_FUNCTIONS
            .iter()
            .position(|row| row.prototype == prototype && row.name == name)
            .map(|index| Self(index as u16))
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

/// Whether `method` is an instance method of the advertised surface.
pub(crate) fn is_instance_method(method: &str) -> bool {
    INSTANCE_STDLIB_SIGNATURES
        .iter()
        .any(|signature| signature.method == method)
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
            assert_eq!(
                BuiltinFunction::named(row.prototype, row.name),
                Some(BuiltinFunction(index as u16)),
                "{}.prototype.{} is listed twice",
                row.prototype.name(),
                row.name
            );
            assert_eq!(
                BuiltinPrototype::from_name(row.prototype.name()),
                Some(row.prototype)
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
