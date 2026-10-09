//! TypeScript's spelling of the contract layer's schema shapes and of the
//! names a prompt shows: what the TypeScript prompt adapter writes, and
//! nothing a cell's lowering reads.

use lash_sansio::{ExtraKeys, ObjectShape, SchemaShape, ShapeKind};

///
/// The shape is the contract layer's one reading of a JSON Schema; this is
/// TypeScript's spelling of it and reads no schema itself. An object keeps
/// its fields whether or not it is closed, and gains an index signature only
/// when the schema says extra keys are allowed.
pub(crate) fn render_schema_shape(shape: &SchemaShape) -> String {
    match &shape.kind {
        ShapeKind::Unknown => "unknown".to_string(),
        ShapeKind::Null => "null".to_string(),
        ShapeKind::Bool => "boolean".to_string(),
        ShapeKind::Int | ShapeKind::Float => "number".to_string(),
        ShapeKind::Str => "string".to_string(),
        ShapeKind::Literals(values) => {
            render_union(values.iter().map(serde_json::Value::to_string))
        }
        ShapeKind::List(item) => format!("Array<{}>", render_schema_shape(item)),
        ShapeKind::Tuple(items) => format!(
            "[{}]",
            items
                .iter()
                .map(render_schema_shape)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::Object(object) => render_object(object),
        ShapeKind::Union(members) => render_union(members.iter().map(render_schema_shape)),
        ShapeKind::Named(name) => render_type_name(name),
        ShapeKind::Process(None) => "Process".to_string(),
        ShapeKind::Process(Some(signature)) => format!(
            "Process<[{}], {}>",
            signature
                .params
                .iter()
                .map(|param| format!("{}: {}", param.name, render_schema_shape(&param.shape)))
                .collect::<Vec<_>>()
                .join(", "),
            render_schema_shape(&signature.output)
        ),
        // `ShapeKind` is non-exhaustive: a kind this dialect has no spelling
        // for yet is shown as the widest type rather than guessed at.
        _ => "unknown".to_string(),
    }
}

/// Alternatives joined with `|`. `int` and `float` are both `number`, so
/// members that spell the same are shown once.
fn render_union(members: impl Iterator<Item = String>) -> String {
    let mut spelled = Vec::<String>::new();
    for member in members {
        if !spelled.contains(&member) {
            spelled.push(member);
        }
    }
    spelled.join(" | ")
}

/// The names a TypeScript cell resolves itself: a tool under one of them
/// could never be called.
const GLOBAL_NAMESPACES: &[&str] = &[
    "Array",
    "Boolean",
    "Date",
    "Error",
    "JSON",
    "Map",
    "Math",
    "Number",
    "Object",
    "Promise",
    "RegExp",
    "Set",
    "String",
    "URL",
    "URLSearchParams",
    "console",
    "globalThis",
];

/// Confirms a TypeScript cell can write `call_path` verbatim as a tool call:
/// it has a module to call the operation on, its receiver is a name a cell
/// can write in expression position, every member is an identifier, and its
/// module is not a namespace the language resolves itself. A path that fails
/// is refused at registration rather than advertised as a callable nothing
/// (FIG-1444).
pub(crate) fn ensure_tool_call_path_addressable(call_path: &str) -> Result<(), String> {
    let segments = call_path.split('.').collect::<Vec<_>>();
    if segments.len() < 2 {
        return Err(format!(
            "tool call path `{call_path}` has no module path, so a TypeScript cell has no receiver to call it on"
        ));
    }
    // A member name may be a reserved word (`processes.await(…)`); the
    // receiver, which a cell writes as an identifier, may not.
    if let Some(segment) = segments.iter().enumerate().find_map(|(index, segment)| {
        (!is_identifier(segment) || (index == 0 && is_reserved_word(segment))).then_some(segment)
    }) {
        return Err(format!(
            "`{segment}` is a word no cell can write in expression position"
        ));
    }
    if GLOBAL_NAMESPACES.contains(&segments[0]) {
        return Err(format!(
            "`{}` is an ECMAScript global namespace, which the dialect resolves itself",
            segments[0]
        ));
    }
    Ok(())
}

fn render_object(object: &ObjectShape) -> String {
    let extra = match &object.extra_keys {
        ExtraKeys::Closed => None,
        // A schema that names its fields and says nothing about other keys
        // is shown as those fields; one that names none accepts anything.
        ExtraKeys::Unstated if object.fields.is_empty() => Some("unknown".to_string()),
        ExtraKeys::Unstated => None,
        ExtraKeys::Open(extra) => Some(render_schema_shape(extra)),
        _ => Some("unknown".to_string()),
    };
    if object.fields.is_empty() {
        return match extra {
            Some(extra) => format!("Record<string, {extra}>"),
            None => "Record<string, never>".to_string(),
        };
    }
    let mut members = object
        .fields
        .iter()
        .map(|field| {
            format!(
                "{}{}: {}",
                render_property_name(&field.name),
                if field.required { "" } else { "?" },
                render_schema_shape(&field.shape)
            )
        })
        .collect::<Vec<_>>();
    if let Some(extra) = extra {
        members.push(format!("[key: string]: {extra}"));
    }
    format!("{{ {} }}", members.join("; "))
}

/// A named host type as an identifier this dialect can write.
///
/// Host data types are dotted names (`cron.Tick`): a valid schema `$ref`,
/// not a valid TypeScript name. The one spelling — dots to underscores —
/// is used in the `type … =` declaration and in every `ShapeKind::Named`
/// reference alike, so the model is never shown a name it cannot resolve
/// against the declaration above it. A name still not writable — a reserved
/// word, or one colliding with the generated-binding namespace — takes the
/// generated encoding instead.
pub(crate) fn render_type_name(name: &str) -> String {
    render_identifier(&name.replace('.', "_"))
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

#[expect(
    clippy::expect_used,
    reason = "the value is a `str`, and `serde_json` never fails to encode one"
)]
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
#[cfg(test)]
pub(crate) fn reserved_words() -> &'static [&'static str] {
    RESERVED_WORDS
}
