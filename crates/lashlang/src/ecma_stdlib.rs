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
];

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
