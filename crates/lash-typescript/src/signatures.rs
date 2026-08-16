use lashlang::{TypeExpr, TypeField, json_schema_to_type_expr};
use serde_json::Value;

/// One accepted standard-library call shape. `arguments` is intentionally
/// human-readable: this table is the dialect contract as well as the lowerer's
/// name inventory, so optional forms cannot hide behind a comment saying
/// "ECMA optional arguments".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StdlibSignature {
    pub(crate) owner: &'static str,
    pub(crate) method: &'static str,
    pub(crate) arguments: &'static str,
}

macro_rules! signatures {
    ($(($owner:literal, $method:literal, $arguments:literal)),+ $(,)?) => {
        &[$(StdlibSignature { owner: $owner, method: $method, arguments: $arguments }),+]
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

pub(crate) const INSTANCE_STDLIB_SIGNATURES: &[StdlibSignature] = signatures![
    ("instance", "at", "[index]"),
    ("instance", "concat", "...values"),
    ("instance", "charAt", "[index]"),
    ("instance", "charCodeAt", "[index]"),
    ("instance", "codePointAt", "[index]"),
    ("instance", "append", "name, value"),
    ("instance", "add", "value"),
    ("instance", "clear", ""),
    ("instance", "delete", "key[, value]"),
    ("instance", "entries", ""),
    ("instance", "exec", "input"),
    ("instance", "endsWith", "search[, endPosition]"),
    ("instance", "filter", "callback[, thisArg]"),
    ("instance", "fill", "[value[, start[, end]]]"),
    ("instance", "find", "callback[, thisArg]"),
    ("instance", "findIndex", "callback[, thisArg]"),
    ("instance", "findLast", "callback[, thisArg]"),
    ("instance", "findLastIndex", "callback[, thisArg]"),
    ("instance", "flat", "[depth]"),
    ("instance", "flatMap", "callback[, thisArg]"),
    ("instance", "forEach", "callback[, thisArg]"),
    ("instance", "get", "key"),
    ("instance", "getAll", "name"),
    ("instance", "has", "key[, value]"),
    ("instance", "includes", "value[, fromIndex]"),
    ("instance", "indexOf", "value[, fromIndex]"),
    ("instance", "join", "[separator]"),
    ("instance", "lastIndexOf", "value[, fromIndex]"),
    ("instance", "map", "callback[, thisArg]"),
    ("instance", "match", "regexp"),
    ("instance", "matchAll", "globalRegExp"),
    ("instance", "every", "callback[, thisArg]"),
    ("instance", "padEnd", "length[, padString]"),
    ("instance", "padStart", "length[, padString]"),
    ("instance", "repeat", "[count]"),
    (
        "instance",
        "replace",
        "[search[, replacement|callback(match, offset, string)]]"
    ),
    (
        "instance",
        "replaceAll",
        "[searchString[, replacementString]]"
    ),
    ("instance", "reduce", "callback[, initialValue]"),
    ("instance", "reduceRight", "callback[, initialValue]"),
    ("instance", "reverse", ""),
    ("instance", "slice", "[start[, end]]"),
    ("instance", "sort", "[compareFn]"),
    ("instance", "some", "callback[, thisArg]"),
    ("instance", "splice", "[start[, deleteCount[, ...items]]]"),
    ("instance", "push", "...values"),
    ("instance", "pop", ""),
    ("instance", "shift", ""),
    ("instance", "unshift", "...values"),
    ("instance", "split", "[separator[, limit]]"),
    ("instance", "search", "regexp"),
    ("instance", "startsWith", "search[, position]"),
    ("instance", "substring", "[start[, end]]"),
    ("instance", "toExponential", "[fractionDigits]"),
    ("instance", "toFixed", "[digits]"),
    ("instance", "toPrecision", "[precision]"),
    ("instance", "toReversed", ""),
    ("instance", "toSorted", "[compareFn]"),
    (
        "instance",
        "toSpliced",
        "[start[, deleteCount[, ...items]]]"
    ),
    ("instance", "set", "key, value"),
    ("instance", "keys", ""),
    ("instance", "toLowerCase", ""),
    ("instance", "toUpperCase", ""),
    ("instance", "toString", ""),
    ("instance", "trim", ""),
    ("instance", "trimEnd", ""),
    ("instance", "trimStart", ""),
    ("instance", "test", "input"),
    ("instance", "valueOf", ""),
    ("instance", "values", ""),
    ("instance", "with", "index, value"),
    ("instance", "hasOwnProperty", "key"),
    ("instance", "union", "set"),
    ("instance", "intersection", "set"),
    ("instance", "difference", "set"),
    ("instance", "symmetricDifference", "set"),
    ("instance", "isSubsetOf", "set"),
    ("instance", "isSupersetOf", "set"),
    ("instance", "isDisjointFrom", "set"),
    ("instance", "toJSON", "[key]"),
    ("instance", "getTime", ""),
    ("instance", "getUTCFullYear", ""),
    ("instance", "getUTCMonth", ""),
    ("instance", "getUTCDate", ""),
    ("instance", "getUTCDay", ""),
    ("instance", "getUTCHours", ""),
    ("instance", "getUTCMinutes", ""),
    ("instance", "getUTCSeconds", ""),
    ("instance", "getUTCMilliseconds", ""),
    ("instance", "toISOString", ""),
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
    if segments.len() > 1
        && segments
            .iter()
            .all(|segment| is_identifier(segment) && !is_reserved_word(segment))
    {
        let (operation, modules) = segments.split_last().expect("non-empty call path");
        let mut declaration = format!(
            "function {operation}(input: {}): Promise<{}>;",
            render_type(&input),
            render_type(&output)
        );
        // Only the outermost wrapper may carry `declare`: a nested one is
        // already inside an ambient context, and `declare namespace a { declare
        // namespace b { … } }` is not valid TypeScript. This is prompt text the
        // model reads as ground truth.
        for (depth, module) in modules.iter().rev().enumerate() {
            let ambient = if depth + 1 == modules.len() {
                "declare "
            } else {
                ""
            };
            declaration = format!("{ambient}namespace {module} {{ {declaration} }}");
        }
        declaration
    } else {
        format!(
            "declare function {}(input: {}): Promise<{}>;",
            render_identifier(name),
            render_type(&input),
            render_type(&output)
        )
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
    matches!(
        name,
        "abstract"
            | "accessor"
            | "any"
            | "as"
            | "asserts"
            | "async"
            | "await"
            | "bigint"
            | "boolean"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "constructor"
            | "continue"
            | "debugger"
            | "declare"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "from"
            | "function"
            | "get"
            | "global"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "infer"
            | "instanceof"
            | "interface"
            | "is"
            | "keyof"
            | "let"
            | "module"
            | "namespace"
            | "never"
            | "new"
            | "null"
            | "number"
            | "object"
            | "of"
            | "override"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "readonly"
            | "require"
            | "return"
            | "satisfies"
            | "set"
            | "static"
            | "string"
            | "super"
            | "switch"
            | "symbol"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "undefined"
            | "unique"
            | "unknown"
            | "using"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
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
}
