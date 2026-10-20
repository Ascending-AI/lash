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
    "Annotated",
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
    ShapeImportRefusal::InvalidSchema {
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
        ShapeKind::Tuple(items) | ShapeKind::Union(items) | ShapeKind::Intersection(items) => {
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
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
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
/// Unbound references retain a validation note on `Any`.
pub fn render_type(
    shape: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    let render = |shape| render_type(shape, names);
    Ok(match &shape.kind {
        ShapeKind::Unknown => "Any".to_owned(),
        ShapeKind::Never => "Never".to_owned(),
        ShapeKind::Ref(id) => match names.get(id) {
            Some(name) => quoted(name),
            None => "Annotated[Any, \"reference validated by the tool\"]".into(),
        },
        ShapeKind::Null => "None".to_owned(),
        ShapeKind::Bool => "bool".to_owned(),
        ShapeKind::Int => "int".to_owned(),
        ShapeKind::Float => "float".to_owned(),
        ShapeKind::Str => "str".to_owned(),
        ShapeKind::Literals(values) => {
            if values.iter().any(|value| {
                !(value.is_string()
                    || value.is_boolean()
                    || value.as_i64().is_some()
                    || value.as_u64().is_some())
            }) {
                format!(
                    "Annotated[Any, {}]",
                    quoted(&format!("one of {}", Value::Array(values.clone())))
                )
            } else {
                let values = values
                    .iter()
                    .map(|value| match value {
                        Value::String(value) => quoted(value),
                        Value::Bool(true) => "True".into(),
                        Value::Bool(false) => "False".into(),
                        _ => value.to_string(),
                    })
                    .collect::<Vec<_>>();
                format!("Literal[{}]", values.join(", "))
            }
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
        ShapeKind::Object(object) => render_object("Record", object, names, false)?,
        ShapeKind::Union(members) => format!(
            "Union[{}]",
            members
                .iter()
                .map(|member| match &member.kind {
                    ShapeKind::Object(object) => render_object("Record", object, names, true),
                    _ => render(member),
                })
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        ShapeKind::Intersection(members) => {
            let mut members = members
                .iter()
                .filter(|member| !matches!(member.kind, ShapeKind::Unknown));
            let base = match members.next() {
                Some(SchemaShape {
                    kind: ShapeKind::Object(object),
                    ..
                }) => render_object("Record", object, names, true)?,
                Some(member) => render(member)?,
                None => "Any".into(),
            };
            let rules = members
                .map(|member| intersection_rule(member, names))
                .collect::<Result<Vec<_>, _>>()?;
            if rules.is_empty() {
                base
            } else {
                format!("Annotated[{base}, {}]", quoted(&rules.join(" and ")))
            }
        }
        ShapeKind::Named(name) => host_name(name),
        ShapeKind::Process(None) | ShapeKind::Function(None) => "Callable[..., Any]".to_owned(),
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => format!(
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
/// Invalid generated names or malformed shape graphs refuse.
pub fn emit_definition(
    name: &str,
    shape: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    if !identifier(name) {
        return Err(unsupported("Python definition name"));
    }
    let expression = match &shape.kind {
        ShapeKind::Object(object) => render_object(name, object, names, false)?,
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
/// Invalid callable names or malformed shape graphs refuse.
pub fn emit_tool_signature(
    call_path: &str,
    input: &SchemaShape,
    input_omittable: bool,
    input_names: &BTreeMap<DefinitionId, String>,
    output: &SchemaShape,
    output_names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    // A cell calls an operation by its dotted path: each segment is a name,
    // a keyword one included, which a cell reaches through `getattr`.
    if !call_path
        .split('.')
        .all(|segment| identifier(segment) || KEYWORDS.contains(&segment))
    {
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
    compact_optional: bool,
) -> Result<String, PresentationRefusal> {
    let total_false = compact_optional && object.fields.iter().all(|field| !field.required);
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
            if !field.required && !total_false {
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
    let total = if total_false { ", total=False" } else { "" };
    Ok(format!(
        "TypedDict({}, {{{}}}{total}{extra})",
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

fn intersection_rule(
    shape: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    match &shape.kind {
        ShapeKind::Object(object)
            if object
                .fields
                .iter()
                .all(|field| matches!(field.shape.kind, ShapeKind::Unknown)) =>
        {
            Ok(format!(
                "requires {}",
                object
                    .fields
                    .iter()
                    .filter(|field| field.required)
                    .map(|field| field.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
        ShapeKind::Union(members) => Ok(format!(
            "one of ({})",
            members
                .iter()
                .map(|member| intersection_rule(member, names))
                .collect::<Result<Vec<_>, _>>()?
                .join(" | ")
        )),
        _ => Ok(format!("also {}", render_type(shape, names)?)),
    }
}
