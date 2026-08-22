use crate::{Diagnostic, DiagnosticCode};
use lashlang::{TypeExpr, TypeField, json_schema_to_type_expr};
use serde_json::Value;

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
pub(crate) struct LiteralReceivers(u8);

impl LiteralReceivers {
    /// No literal receiver carries this method — it needs a value only a
    /// binding or constructor can produce (a `Map`, a `Set`, a `Date`, a
    /// `RegExp`, a `URLSearchParams`).
    pub(crate) const NONE: Self = Self(0);
    pub(crate) const STRING: Self = Self(1 << 0);
    pub(crate) const ARRAY: Self = Self(1 << 1);
    pub(crate) const NUMBER: Self = Self(1 << 2);
    /// Boolean, `null`, `undefined` and object literals. They share one row of
    /// the matrix because the dialect accepts the same three methods on all of
    /// them.
    pub(crate) const OTHER: Self = Self(1 << 3);
    pub(crate) const ALL: Self = Self(0b1111);

    pub(crate) const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn contains(self, kind: Self) -> bool {
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
pub(crate) struct StdlibSignature {
    pub(crate) owner: &'static str,
    pub(crate) method: &'static str,
    pub(crate) arguments: &'static str,
    pub(crate) receivers: LiteralReceivers,
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

pub(crate) const STATIC_STDLIB_SIGNATURES: &[StdlibSignature] = signatures![
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

pub(crate) const INSTANCE_STDLIB_SIGNATURES: &[StdlibSignature] = instance_signatures![
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

/// Renders the standard-library inventory used by the model prompt.
///
/// Keeping this text derived from the same tables that drive lowering makes a
/// newly accepted method impossible to omit from the prompt accidentally.
pub fn render_stdlib_contract() -> String {
    fn render(signatures: &[StdlibSignature]) -> String {
        signatures
            .iter()
            .map(|signature| {
                let arguments = signature.arguments;
                format!("`{}.{}`({arguments})", signature.owner, signature.method)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    format!(
        "Static calls: {}.\n\nInstance calls: {}.",
        render(STATIC_STDLIB_SIGNATURES),
        render(INSTANCE_STDLIB_SIGNATURES)
    )
}

/// Number of owner-qualified calls in the v1 standard-library contract.
pub fn stdlib_name_count() -> usize {
    STATIC_STDLIB_SIGNATURES.len() + INSTANCE_STDLIB_SIGNATURES.len()
}

/// Renders one tool's JSON schemas as a TypeScript declaration using the
/// shared Lash type engine's schema importer.
pub fn render_tool_signature(
    name: &str,
    input_schema: &Value,
    output_schema: Option<&Value>,
) -> String {
    let input = json_schema_to_type_expr(input_schema);
    let output = output_schema.map_or(TypeExpr::Any, json_schema_to_type_expr);
    let segments = name.split('.').collect::<Vec<_>>();
    let (operation, modules) = segments.split_last().expect("split never yields nothing");
    // Only the root of a call path is written in expression position; a cell
    // spells every segment after it as a property name, where ECMAScript accepts
    // reserved words. A path whose root is a word no cell can write cannot be
    // advertised honestly at all — registration refuses those, see
    // [`ensure_tool_call_path_addressable`].
    if segments.len() > 1
        && segments.iter().all(|segment| is_identifier(segment))
        && !is_expression_reserved_word(modules[0])
    {
        let signature = format!(
            "(input: {}): Promise<{}>",
            render_type(&input),
            render_type(&output)
        );
        if segments.iter().all(|segment| !is_reserved_word(segment)) {
            let mut declaration = format!("function {operation}{signature};");
            // Only the outermost wrapper may carry `declare`: a nested one is
            // already inside an ambient context, and `declare namespace a {
            // declare namespace b { … } }` is not valid TypeScript. This is
            // prompt text the model reads as ground truth.
            for module in modules.iter().rev() {
                declaration = format!("namespace {module} {{ {declaration} }}");
            }
            format!("declare {declaration}")
        } else {
            // A namespace or function *name* is a declaration position, which
            // admits no reserved word, so `inbox.delete` had no namespace
            // spelling and was advertised as the mangled `__lash_tool_<hex>`
            // identifier — a callable no binding provides, which rejected with
            // `TS_UNKNOWN_BINDING` for itself while the dotted path the catalog
            // never mentioned worked (FIG-1444). Declaring the tail as nested
            // properties of the root moves every reserved name into the position
            // a cell actually writes it in, so the declaration spells the call.
            let (root, inner) = modules.split_first().expect("checked len above");
            let mut declaration = format!("{operation}{signature}");
            for module in inner.iter().rev() {
                declaration = format!("{module}: {{ {declaration} }}");
            }
            format!("declare const {root}: {{ {declaration} }};")
        }
    } else {
        format!(
            "declare function {}(input: {}): Promise<{}>;",
            render_identifier(name),
            render_type(&input),
            render_type(&output)
        )
    }
}

/// Confirms a TypeScript cell can address `call_path` verbatim as a tool call.
///
/// The check lowers the call the catalog advertises instead of re-deriving the
/// dialect's name rules, so every reason a path is unreachable — a reserved word
/// no cell can write in expression position, an ECMA global namespace the
/// lowerer resolves itself, a method name the dialect refuses outright — is
/// honored by construction and cannot drift from the lowerer. Registration calls
/// it so a tool whose path the dialect resolves to anything other than a tool
/// call is refused rather than advertised as a callable nothing (FIG-1444).
pub fn ensure_tool_call_path_addressable(call_path: &str) -> Result<(), Diagnostic> {
    let segments = call_path.split('.').collect::<Vec<_>>();
    let (operation, modules) = segments.split_last().expect("split never yields nothing");
    if modules.is_empty() {
        return Err(Diagnostic::new(
            DiagnosticCode::UnknownBinding,
            format!(
                "tool call path `{call_path}` has no module path, so a TypeScript cell has no receiver to call it on"
            ),
            None,
        ));
    }
    // The inner diagnostic answers a different question than the caller asked:
    // `Math.floor` fails the probe as `TS_AWAIT_UNSUPPORTED`, which reads as an
    // instruction to drop the `await` rather than as "this path names an
    // ECMAScript global, so no tool can live under it". Lead with the reason the
    // path is unadvertisable and carry the inner diagnostic as the detail.
    let detail = match crate::parse(&format!("finish(await {call_path}({{}}));")) {
        Ok(program) if addresses_tool(&program.main, modules, operation) => return Ok(()),
        Ok(_) => "the dialect resolves that call itself instead of dispatching a tool".to_string(),
        Err(inner) => format!(
            "that cell is refused as {}: {}",
            inner.code.as_str(),
            inner.message
        ),
    };
    Err(Diagnostic::refusal(
        DiagnosticCode::MethodUnsupported,
        format!(
            "tool call path `{call_path}` does not dispatch a tool in a TypeScript cell, so advertising it would promise a callable no binding provides: writing `await {call_path}(input)` names a word no cell can write in expression position, an ECMAScript global namespace, or a method the dialect claims for itself — {detail}"
        ),
        None,
    ))
}

fn addresses_tool(expr: &lashlang::Expr, modules: &[&str], operation: &str) -> bool {
    match expr {
        lashlang::Expr::ReceiverCall {
            receiver,
            operation: called,
            ..
        } if called.as_str() == operation => match receiver.as_ref() {
            lashlang::Expr::ResourceRef(resource) => resource
                .path
                .iter()
                .map(|segment| segment.as_str())
                .eq(modules.iter().copied()),
            _ => false,
        },
        _ => expr
            .children()
            .any(|child| addresses_tool(child, modules, operation)),
    }
}

fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Any => "unknown".to_string(),
        TypeExpr::Str => "string".to_string(),
        TypeExpr::Int | TypeExpr::Float => "number".to_string(),
        TypeExpr::Bool => "boolean".to_string(),
        TypeExpr::Dict => "Record<string, unknown>".to_string(),
        TypeExpr::Null => "null".to_string(),
        TypeExpr::Enum(values) => values
            .iter()
            .map(|value| serde_json::to_string(value.as_str()).expect("strings serialize"))
            .collect::<Vec<_>>()
            .join(" | "),
        TypeExpr::List(item) => format!("Array<{}>", render_type(item)),
        TypeExpr::Object(fields) => render_object(fields),
        TypeExpr::Ref(name) => render_identifier(name),
        TypeExpr::Process { input, output, .. } => {
            format!("Process<{}, {}>", render_type(input), render_type(output))
        }
        TypeExpr::TriggerHandle(event) => format!("TriggerHandle<{}>", render_type(event)),
        TypeExpr::Union(items) => items
            .iter()
            .map(render_type)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn render_object(fields: &[TypeField]) -> String {
    if fields.is_empty() {
        return "Record<string, never>".to_string();
    }
    format!(
        "{{ {} }}",
        fields
            .iter()
            .map(|field| format!(
                "{}{}: {}",
                render_property_name(field.name.as_str()),
                if field.optional { "?" } else { "" },
                render_type(&field.ty)
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn render_identifier(name: &str) -> String {
    const GENERATED_PREFIX: &str = "__lash_tool_";
    if is_identifier(name) && !is_reserved_word(name) && !name.starts_with(GENERATED_PREFIX) {
        name.to_string()
    } else {
        let encoded = name
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("{GENERATED_PREFIX}{encoded}")
    }
}

fn render_property_name(name: &str) -> String {
    if is_identifier(name) && !is_reserved_word(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).expect("strings serialize")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first == '$' || first.is_alphabetic())
        && chars
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

fn is_reserved_word(name: &str) -> bool {
    RESERVED_WORDS.contains(&name)
}

/// Every word the renderer declines to emit in identifier position.
///
/// Exposed through [`reserved_words`] so the advertisement sweep can be driven
/// by this table instead of a hand-copied one: a word added here is swept for
/// callability without anybody remembering to extend the test.
const RESERVED_WORDS: &[&str] = &[
    "abstract",
    "accessor",
    "any",
    "as",
    "asserts",
    "async",
    "await",
    "bigint",
    "boolean",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "constructor",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "global",
    "if",
    "implements",
    "import",
    "in",
    "infer",
    "instanceof",
    "interface",
    "is",
    "keyof",
    "let",
    "module",
    "namespace",
    "never",
    "new",
    "null",
    "number",
    "object",
    "of",
    "override",
    "package",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "return",
    "satisfies",
    "set",
    "static",
    "string",
    "super",
    "switch",
    "symbol",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "unique",
    "unknown",
    "using",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Every word this dialect declines to render in identifier position.
pub fn reserved_words() -> &'static [&'static str] {
    RESERVED_WORDS
}

/// The subset of [`RESERVED_WORDS`] ECMAScript also forbids as an *expression*
/// identifier, so a cell cannot write it as the root of a call path at all.
///
/// The rest of the table — `type`, `get`, `any`, `string`, … — is reserved only
/// where TypeScript expects a declaration name; a cell writes those as plain
/// identifiers and the parser accepts them. Keeping the two apart is what lets
/// a module path spelled with a contextual keyword be advertised the way it is
/// actually called.
const EXPRESSION_RESERVED_WORDS: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

fn is_expression_reserved_word(name: &str) -> bool {
    EXPRESSION_RESERVED_WORDS.contains(&name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_schema_through_shared_type_engine() {
        let signature = render_tool_signature(
            "search-docs",
            &json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } },
                "required": ["query"]
            }),
            Some(&json!({ "type": "array", "items": { "type": "string" } })),
        );
        assert_eq!(
            signature,
            "declare function __lash_tool_7365617263682d646f6373(input: { limit?: number; query: string }): Promise<Array<string>>;"
        );
    }

    /// The expression-position table is a *subset* of the declaration-position
    /// one. A word only ECMAScript forbids would otherwise be rendered as a
    /// namespace name TypeScript rejects, and a word missing from the wider
    /// table would be advertised as a `const` whose name no cell can write.
    #[test]
    fn expression_reserved_words_are_a_subset_of_reserved_words() {
        for word in EXPRESSION_RESERVED_WORDS {
            assert!(
                is_reserved_word(word),
                "`{word}` is reserved in expression position but not in declaration position"
            );
        }
        assert!(EXPRESSION_RESERVED_WORDS.len() < RESERVED_WORDS.len());
    }

    /// The literal-receiver matrix the hand-written arms in the lowerer used to
    /// carry, transcribed once. The size of each column is the whole claim: the
    /// arms held 28 string, 33 array, 5 number and 3 remaining-literal names,
    /// and this table restates them with array `valueOf` added, which is the
    /// one divergence FIG-1718 resolves. A method widened onto a shape it did
    /// not carry moves a count and fails here rather than quietly enlarging the
    /// accepted surface.
    #[test]
    fn literal_receiver_column_restates_the_matrix_it_replaced() {
        let carrying = |kind: LiteralReceivers| {
            INSTANCE_STDLIB_SIGNATURES
                .iter()
                .filter(|signature| signature.receivers.contains(kind))
                .count()
        };
        assert_eq!(carrying(On::STRING), 28, "string literal receivers");
        assert_eq!(carrying(On::ARRAY), 34, "array literal receivers");
        assert_eq!(carrying(On::NUMBER), 5, "number literal receivers");
        assert_eq!(carrying(On::OTHER), 3, "remaining literal receivers");

        // Cardinality alone lets a compensating swap through — one method moved
        // off arrays and another moved on keeps the count. The array column is
        // the one this ticket changed, so pin its membership outright; the other
        // three are small enough that a swap inside them is caught by the string
        // and number columns disagreeing about the same name.
        let carried_by = |kind: LiteralReceivers| {
            let mut names = INSTANCE_STDLIB_SIGNATURES
                .iter()
                .filter(|signature| signature.receivers.contains(kind))
                .map(|signature| signature.method)
                .collect::<Vec<_>>();
            names.sort_unstable();
            names
        };
        assert_eq!(
            carried_by(On::ARRAY),
            [
                "at",
                "concat",
                "every",
                "fill",
                "filter",
                "find",
                "findIndex",
                "findLast",
                "findLastIndex",
                "flat",
                "flatMap",
                "forEach",
                "includes",
                "indexOf",
                "join",
                "lastIndexOf",
                "map",
                "pop",
                "push",
                "reduce",
                "reduceRight",
                "reverse",
                "shift",
                "slice",
                "some",
                "sort",
                "splice",
                "toReversed",
                "toSorted",
                "toSpliced",
                "toString",
                "unshift",
                "valueOf",
                "with",
            ]
        );
        assert_eq!(
            carried_by(On::NUMBER),
            [
                "toExponential",
                "toFixed",
                "toPrecision",
                "toString",
                "valueOf"
            ]
        );
        assert_eq!(
            carried_by(On::OTHER),
            ["hasOwnProperty", "toString", "valueOf"]
        );
    }

    /// The receiver-kind column is keyed by method name, so a second row for a
    /// name already in the table would answer the lookup only by accident of
    /// ordering.
    #[test]
    fn instance_method_names_are_unique() {
        let mut names = INSTANCE_STDLIB_SIGNATURES
            .iter()
            .map(|signature| signature.method)
            .collect::<Vec<_>>();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate instance method name");
    }

    /// A static call names an owner namespace rather than a receiver, so its
    /// rows must not claim a literal receiver shape.
    #[test]
    fn static_signatures_carry_no_receiver_kind() {
        assert!(
            STATIC_STDLIB_SIGNATURES
                .iter()
                .all(|signature| signature.receivers == LiteralReceivers::NONE)
        );
    }
}
