//! TypeScript's spelling of the shared schema shapes and of the names a
//! prompt shows: what the TypeScript presenter writes, and nothing a cell's
//! lowering reads.

use std::collections::BTreeMap;

use lash_tool_shape::{
    DefinitionId, ExtraKeys, ObjectShape, SchemaShape, ShapeImportRefusal, ShapeKind,
};

/// Document-local references mapped to their allocated TypeScript names.
///
/// ```rust,ignore
/// let names = pool.prepare(&document)?.names;
/// let text = render_type_expression(&document.roots[0], &names)?;
/// ```
pub type DefinitionNames = BTreeMap<DefinitionId, String>;

/// Spell a reference-free shape (for example an inferred runtime value).
///
/// ```rust,ignore
/// assert_eq!(render_schema_shape(&ShapeKind::Str.into())?, "string");
/// ```
///
/// # Errors
/// Refuses local references without a definitions context and unsupported kinds.
pub fn render_schema_shape(shape: &SchemaShape) -> Result<String, ShapeImportRefusal> {
    render_type_expression(shape, &DefinitionNames::new())
}

/// Spell a type expression, retaining references as names rather than expanding.
/// Names come from the document's prepared definition pool.
///
/// ```rust,ignore
/// let prepared = pool.prepare(&document)?;
/// let text = render_type_expression(&document.roots[0], &prepared.names)?;
/// ```
///
/// # Errors
/// Refuses missing reference names and unsupported shape kinds.
pub fn render_type_expression(
    shape: &SchemaShape,
    names: &DefinitionNames,
) -> Result<String, ShapeImportRefusal> {
    let render = |shape: &SchemaShape| render_type_expression(shape, names);
    Ok(match &shape.kind {
        ShapeKind::Unknown => "unknown".to_string(),
        ShapeKind::Never => "never".to_string(),
        ShapeKind::Ref(id) => names
            .get(id)
            .cloned()
            .unwrap_or_else(|| "unknown /* reference validated by the tool */".into()),
        ShapeKind::Null => "null".to_string(),
        ShapeKind::Bool => "boolean".to_string(),
        ShapeKind::Int | ShapeKind::Float => "number".to_string(),
        ShapeKind::Str => "string".to_string(),
        ShapeKind::Literals(values) => {
            render_union(values.iter().map(serde_json::Value::to_string))
        }
        ShapeKind::List(item) => format!("Array<{}>", render(item)?),
        ShapeKind::Tuple(items) => format!(
            "[{}]",
            items
                .iter()
                .map(render)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        ShapeKind::Object(object) => render_object(object, names)?,
        ShapeKind::Union(members) => render_union(
            members
                .iter()
                .map(render)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter(),
        ),
        ShapeKind::Intersection(members) => {
            let text = members
                .iter()
                .filter(|member| !matches!(member.kind, ShapeKind::Unknown))
                .map(|member| {
                    let text = render(member)?;
                    Ok(
                        if matches!(member.kind, ShapeKind::Union(_) | ShapeKind::Literals(_)) {
                            format!("({text})")
                        } else {
                            text
                        },
                    )
                })
                .collect::<Result<Vec<_>, ShapeImportRefusal>>()?
                .join(" & ");
            if text.is_empty() {
                "unknown".into()
            } else {
                text
            }
        }
        ShapeKind::Named(name) => render_type_name(name),
        ShapeKind::Process(None) => "Process".to_string(),
        ShapeKind::Process(Some(signature)) => format!(
            "Process<[{}], {}>",
            signature
                .params
                .iter()
                .map(|param| Ok(format!(
                    "{}: {}",
                    render_property_name(&param.name),
                    render(&param.shape)?
                )))
                .collect::<Result<Vec<_>, ShapeImportRefusal>>()?
                .join(", "),
            render(&signature.output)?
        ),
        _ => {
            return Err(ShapeImportRefusal::InvalidSchema {
                keyword: "TypeScript shape kind".into(),
            });
        }
    })
}

fn render_union(members: impl Iterator<Item = String>) -> String {
    let mut spelled = Vec::<String>::new();
    for member in members {
        if !spelled.contains(&member) {
            spelled.push(member);
        }
    }
    if spelled.is_empty() {
        "never".into()
    } else {
        spelled.join(" | ")
    }
}

fn render_object(
    object: &ObjectShape,
    names: &DefinitionNames,
) -> Result<String, ShapeImportRefusal> {
    let extra = match &object.extra_keys {
        ExtraKeys::Closed => None,
        ExtraKeys::Unstated if object.fields.is_empty() => Some("unknown".to_string()),
        ExtraKeys::Unstated => None,
        ExtraKeys::Open(extra) => Some(render_type_expression(extra, names)?),
        _ => {
            return Err(ShapeImportRefusal::InvalidSchema {
                keyword: "TypeScript extra keys".into(),
            });
        }
    };
    if object.fields.is_empty() {
        return Ok(match extra {
            Some(extra) => format!("Record<string, {extra}>"),
            None => "Record<string, never>".to_string(),
        });
    }
    let mut members = object
        .fields
        .iter()
        .map(|field| {
            Ok(format!(
                "{}{}: {}",
                render_property_name(&field.name),
                if field.required { "" } else { "?" },
                render_type_expression(&field.shape, names)?
            ))
        })
        .collect::<Result<Vec<_>, ShapeImportRefusal>>()?;
    if let Some(extra) = extra {
        members.push(format!("[key: string]: {extra}"));
    }
    Ok(format!("{{ {} }}", members.join("; ")))
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
///
/// ```rust,ignore
/// assert_eq!(render_type_name("cron.Tick"), "cron_Tick");
/// ```
pub fn render_type_name(name: &str) -> String {
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

/// Whether `name` is a TypeScript identifier.
///
/// ```rust,ignore
/// assert!(is_identifier("Filter"));
/// ```
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first == '$' || first.is_alphabetic())
        && chars
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

/// Whether `name` is a word this dialect declines in identifier position.
///
/// ```rust,ignore
/// assert!(is_reserved_word("class"));
/// ```
pub fn is_reserved_word(name: &str) -> bool {
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
///
/// ```rust,ignore
/// assert!(reserved_words().contains(&"class"));
/// ```
pub fn reserved_words() -> &'static [&'static str] {
    RESERVED_WORDS
}
