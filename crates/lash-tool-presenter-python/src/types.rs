use std::collections::BTreeMap;

use lash_tool_presenter_api::PresentationRefusal;
use lash_tool_shape::{
    DefinitionId, ExtraKeys, ObjectShape, ReadableNameAllocator, SchemaShape, ShapeImportRefusal,
    ShapeKind,
};
use serde_json::Value;

pub(crate) const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

pub(crate) const RESERVED_NAMES: &[&str] = &[
    "Any",
    "Never",
    "Literal",
    "Union",
    "TypedDict",
    "NotRequired",
    "Callable",
    "list",
    "dict",
    "tuple",
    "str",
    "int",
    "float",
    "bool",
    "Record",
    "HistoryItem",
];

fn unsupported(keyword: &str) -> PresentationRefusal {
    ShapeImportRefusal::UnsupportedConstruct {
        keyword: keyword.to_owned(),
    }
    .into()
}

fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&name)
}

fn host_name(name: &str) -> String {
    let name = name.replace('.', "_");
    if identifier(&name) {
        name
    } else {
        format!("{name}_")
    }
}

/// Reserve the Python spelling of every host type in an anonymous shape.
/// Reserve all documents before assigning any shared definition names.
///
/// ```rust,ignore
/// let mut allocator = ReadableNameAllocator::default();
/// reserve_host_names(&ShapeKind::Named("host.Record".into()).into(), &mut allocator);
/// assert!(!allocator.reserve("host_Record"));
/// ```
pub fn reserve_host_names(shape: &SchemaShape, allocator: &mut ReadableNameAllocator) {
    match &shape.kind {
        ShapeKind::Named(name) => {
            allocator.reserve(&host_name(name));
        }
        ShapeKind::List(item) => reserve_host_names(item, allocator),
        ShapeKind::Tuple(items) | ShapeKind::Union(items) => {
            for item in items {
                reserve_host_names(item, allocator);
            }
        }
        ShapeKind::Object(object) => {
            for field in &object.fields {
                reserve_host_names(&field.shape, allocator);
            }
            if let ExtraKeys::Open(extra) = &object.extra_keys {
                reserve_host_names(extra, allocator);
            }
        }
        ShapeKind::Process(Some(process)) => {
            for param in &process.params {
                reserve_host_names(&param.shape, allocator);
            }
            reserve_host_names(&process.output, allocator);
        }
        _ => {}
    }
}

/// Spell an expression using quoted local references, including within lists
/// and mutually recursive `Union` aliases. Names come from [`crate::definition_names`].
///
/// ```rust,ignore
/// let expression = render_type(&document.roots[0], &names)?;
/// // list["Node"]
/// ```
///
/// # Errors
/// Unbound references and unsupported constructs refuse rather than become `Any`.
pub fn render_type(
    shape: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    let render = |shape| render_type(shape, names);
    Ok(match &shape.kind {
        ShapeKind::Unknown => "Any".to_owned(),
        ShapeKind::Never => "Never".to_owned(),
        ShapeKind::Ref(id) => {
            let name = names
                .get(id)
                .ok_or_else(|| ShapeImportRefusal::MissingReference {
                    reference: format!("definition {}", id.0),
                })?;
            if !identifier(name) {
                return Err(unsupported("Python definition name"));
            }
            quoted(name)
        }
        ShapeKind::Null => "None".to_owned(),
        ShapeKind::Bool => "bool".to_owned(),
        ShapeKind::Int => "int".to_owned(),
        ShapeKind::Float => "float".to_owned(),
        ShapeKind::Str => "str".to_owned(),
        ShapeKind::Literals(values) => {
            let values = values
                .iter()
                .map(|value| match value {
                    Value::String(value) => Ok(quoted(value)),
                    Value::Bool(true) => Ok("True".to_owned()),
                    Value::Bool(false) => Ok("False".to_owned()),
                    Value::Number(number) if number.is_i64() || number.is_u64() => {
                        Ok(number.to_string())
                    }
                    _ => Err(unsupported("Python Literal value")),
                })
                .collect::<Result<Vec<_>, _>>()?;
            format!("Literal[{}]", values.join(", "))
        }
        ShapeKind::List(item) => format!("list[{}]", render(item)?),
        ShapeKind::Tuple(items) => {
            if items.is_empty() {
                "tuple[()]".to_owned()
            } else {
                format!(
                    "tuple[{}]",
                    items
                        .iter()
                        .map(render)
                        .collect::<Result<Vec<_>, _>>()?
                        .join(", ")
                )
            }
        }
        ShapeKind::Object(object) => render_object("Record", object, names)?,
        ShapeKind::Union(members) => format!(
            "Union[{}]",
            members
                .iter()
                .map(render)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        ShapeKind::Named(name) => host_name(name),
        ShapeKind::Process(None) => "Callable[..., Any]".to_owned(),
        ShapeKind::Process(Some(process)) => format!(
            "Callable[[{}], {}]",
            process
                .params
                .iter()
                .map(|param| render(&param.shape))
                .collect::<Result<Vec<_>, _>>()?
                .join(", "),
            render(&process.output)?
        ),
        _ => return Err(unsupported("Python shape kind")),
    })
}

/// Emit one named definition. Functional `TypedDict` preserves keys such as
/// `and`, punctuation and empty strings; `NotRequired` marks key absence only.
///
/// ```rust,ignore
/// let text = emit_definition("Filter", &definition.shape, &names)?;
/// // Filter = TypedDict("Filter", {"and": NotRequired[list["Filter"]]})
/// ```
///
/// # Errors
/// Invalid names, unbound references and unsupported expressions refuse.
pub fn emit_definition(
    name: &str,
    shape: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    if !identifier(name) {
        return Err(unsupported("Python definition name"));
    }
    let expression = match &shape.kind {
        ShapeKind::Object(object) => render_object(name, object, names)?,
        _ => render_type(shape, names)?,
    };
    Ok(format!("{name} = {expression}"))
}

/// Emit a tool signature against explicit input/output definition contexts.
/// The caller emits those contexts' definitions alongside the signature.
///
/// ```rust,ignore
/// let signature = emit_tool_signature("lookup", &input, false, &input_names, &output, &output_names)?;
/// // async def lookup(input: "Query") -> str
/// ```
///
/// # Errors
/// Invalid callable names, unbound references and unsupported expressions refuse.
pub fn emit_tool_signature(
    call_path: &str,
    input: &SchemaShape,
    input_omittable: bool,
    input_names: &BTreeMap<DefinitionId, String>,
    output: &SchemaShape,
    output_names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    if !identifier(call_path) {
        return Err(unsupported("Python callable name"));
    }
    Ok(format!(
        "async def {call_path}(input: {}{}) -> {}",
        render_type(input, input_names)?,
        if input_omittable { " = {}" } else { "" },
        render_type(output, output_names)?
    ))
}

fn render_object(
    name: &str,
    object: &ObjectShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    if object.fields.is_empty() {
        let extra = match &object.extra_keys {
            ExtraKeys::Closed => "Never".to_owned(),
            ExtraKeys::Unstated => "Any".to_owned(),
            ExtraKeys::Open(extra) => render_type(extra, names)?,
            _ => return Err(unsupported("Python extra keys")),
        };
        return Ok(format!("dict[str, {extra}]"));
    }
    let fields = object
        .fields
        .iter()
        .map(|field| {
            let mut expression = render_type(&field.shape, names)?;
            if !field.required {
                expression = format!("NotRequired[{expression}]");
            }
            Ok(format!("{}: {expression}", quoted(&field.name)))
        })
        .collect::<Result<Vec<_>, PresentationRefusal>>()?;
    let extra = match &object.extra_keys {
        ExtraKeys::Closed => ", closed=True".to_owned(),
        ExtraKeys::Unstated => String::new(),
        ExtraKeys::Open(extra) => format!(", extra_items={}", render_type(extra, names)?),
        _ => return Err(unsupported("Python extra keys")),
    };
    Ok(format!(
        "TypedDict({}, {{{}}}{extra})",
        quoted(name),
        fields.join(", ")
    ))
}

fn quoted(value: &str) -> String {
    Value::String(value.to_owned()).to_string()
}

pub(crate) fn note_text(value: &str) -> String {
    value.replace('`', "\\`").replace(['\n', '\r'], " ")
}
