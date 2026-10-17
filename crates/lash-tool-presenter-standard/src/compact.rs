//! Public building blocks for compact graph documentation.

use std::collections::BTreeMap;

use lash_tool_presenter_api::PresentationRefusal;
use lash_tool_shape::{DefinitionId, ExtraKeys, SchemaDocument, SchemaShape, ShapeKind};

use crate::STANDARD_TOOL_PRESENTER_ID;

/// One path-only documentation row. Types live in the signature or a named
/// definition, so a row carries only annotations that those types cannot say.
///
/// ```rust,ignore
/// let row = CompactNoteRow { path: "input.query".into(), notes: vec![], description: Some("Query".into()) };
/// assert_eq!(compact_note_line(&row), "- `input.query` — Query");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactNoteRow {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

pub(crate) fn refusal(reason: impl Into<String>) -> PresentationRefusal {
    PresentationRefusal::Tool {
        presenter: STANDARD_TOOL_PRESENTER_ID.to_string(),
        tool: "schema".to_string(),
        reason: reason.into(),
    }
}

/// Spell a shape using the names allocated for its owning document.
/// Explicitly unconstrained schemas are `any`; missing names and unsupported
/// kinds refuse instead of widening a contract.
///
/// ```rust,ignore
/// let document = SchemaDocument::from_json_schema(&json!({"type":"string"}), 8)?;
/// assert_eq!(compact_type(&document.roots[0], &BTreeMap::new())?, "str");
/// ```
///
/// # Errors
/// Refuses references without names and kinds this notation cannot express.
pub fn compact_type(
    shape: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    Ok(match &shape.kind {
        ShapeKind::Unknown => "any".to_string(),
        ShapeKind::Never => "never".to_string(),
        ShapeKind::Ref(id) => names
            .get(id)
            .cloned()
            .ok_or_else(|| refusal("unnamed reference"))?,
        ShapeKind::Null => "null".to_string(),
        ShapeKind::Bool => "bool".to_string(),
        ShapeKind::Int => "int".to_string(),
        ShapeKind::Float => "float".to_string(),
        ShapeKind::Str => "str".to_string(),
        ShapeKind::Literals(values) => format!(
            "enum[{}]",
            values
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::List(item) => format!("list[{}]", compact_type(item, names)?),
        ShapeKind::Tuple(items) => format!("tuple[{}]", types(items, names)?.join(", ")),
        ShapeKind::Object(object) => {
            let mut members = object
                .fields
                .iter()
                .map(|field| {
                    Ok(format!(
                        "{}{}: {}",
                        key(&field.name),
                        if field.required { "" } else { "?" },
                        compact_type(&field.shape, names)?
                    ))
                })
                .collect::<Result<Vec<_>, PresentationRefusal>>()?;
            match &object.extra_keys {
                ExtraKeys::Open(extra) => match extra.kind {
                    ShapeKind::Unknown if members.is_empty() => {}
                    ShapeKind::Unknown => members.push("...".to_string()),
                    _ => members.push(format!("...: {}", compact_type(extra, names)?)),
                },
                ExtraKeys::Closed | ExtraKeys::Unstated => {}
                _ => return Err(refusal("unsupported extra-key policy")),
            }
            if members.is_empty() {
                "record".to_string()
            } else {
                format!("record{{{}}}", members.join(", "))
            }
        }
        ShapeKind::Union(members) => types(members, names)?.join(" | "),
        ShapeKind::Intersection(members) => types(members, names)?
            .into_iter()
            .map(|text| format!("({text})"))
            .collect::<Vec<_>>()
            .join(" & "),
        ShapeKind::Named(name) => name.clone(),
        ShapeKind::Process(None) => "process".to_string(),
        ShapeKind::Process(Some(signature)) => format!(
            "process[({}) -> {}]",
            signature
                .params
                .iter()
                .map(|param| Ok(format!(
                    "{}: {}",
                    key(&param.name),
                    compact_type(&param.shape, names)?
                )))
                .collect::<Result<Vec<_>, PresentationRefusal>>()?
                .join(", "),
            compact_type(&signature.output, names)?
        ),
        _ => return Err(refusal("unsupported shape kind")),
    })
}

fn types(
    shapes: &[SchemaShape],
    names: &BTreeMap<DefinitionId, String>,
) -> Result<Vec<String>, PresentationRefusal> {
    shapes
        .iter()
        .map(|shape| compact_type(shape, names))
        .collect()
}

fn key(name: &str) -> String {
    if !name.is_empty()
        && name.chars().enumerate().all(|(index, c)| {
            c == '_' || c.is_ascii_alphabetic() || (index > 0 && c.is_ascii_digit())
        })
    {
        name.to_string()
    } else {
        serde_json::Value::String(name.to_string()).to_string()
    }
}

/// Emit the call part of a tool signature with names from its document context.
/// Spell the result with [`compact_type`].
/// Use alongside [`compact_definition`] for every reachable definition;
/// [`crate::compact_contract`] returns that self-contained bundle directly.
///
/// ```rust,ignore
/// let signature = compact_signature("lookup", &input, &names)?;
/// assert!(signature.starts_with("lookup("));
/// ```
///
/// # Errors
/// Refuses a missing reference name or unsupported shape.
pub fn compact_signature(
    call_path: &str,
    input: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    Ok(format!("{call_path}({})", compact_arguments(input, names)?))
}

pub(crate) fn compact_arguments(
    input: &SchemaShape,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    if matches!(input.kind, ShapeKind::Object(_)) {
        let label = compact_type(input, names)?;
        if label == "record" {
            Ok("{}".to_string())
        } else {
            Ok(format!("{{ {} }}", &label[7..label.len() - 1]))
        }
    } else {
        compact_type(input, names)
    }
}

/// Emit one named body and its path-only notes, without expanding references.
/// Allocate the whole reachable closure before emitting recursive definitions.
///
/// ```rust,ignore
/// let definition = compact_definition(&document, id, &names)?;
/// assert!(definition.starts_with("type Filter = "));
/// ```
///
/// # Errors
/// Refuses an unknown definition or a missing allocated name.
pub fn compact_definition(
    document: &SchemaDocument,
    id: DefinitionId,
    names: &BTreeMap<DefinitionId, String>,
) -> Result<String, PresentationRefusal> {
    let definition = document
        .definition(id)
        .ok_or_else(|| refusal("missing definition"))?;
    let name = names
        .get(&id)
        .ok_or_else(|| refusal("unnamed definition"))?;
    let mut lines = vec![format!(
        "type {name} = {}",
        compact_type(&definition.shape, names)?
    )];
    lines.extend(
        compact_note_rows(&definition.shape, name)
            .iter()
            .map(compact_note_line),
    );
    Ok(lines.join("\n"))
}

/// Emit only notes belonging to `shape`, qualified by `path`. A reference
/// edge carries its use-site annotations; definition notes are emitted with
/// their definition. Branch paths remain distinct and recursive edges stop.
/// The rows contain `path`, constraint/default `notes`, and `description`.
///
/// ```rust,ignore
/// let rows = compact_note_rows(&document.roots[0], "input");
/// // {"path":"input.limit", "notes":["<= 10", "default 5"]}
/// ```
pub fn compact_note_rows(shape: &SchemaShape, path: &str) -> Vec<CompactNoteRow> {
    fn walk(shape: &SchemaShape, path: &str, rows: &mut Vec<CompactNoteRow>) {
        if shape.has_notes() {
            let mut notes = shape.constraints.notes();
            if let Some(default) = &shape.default {
                notes.push(format!("default {default}"));
            }
            rows.push(CompactNoteRow {
                path: path.to_string(),
                notes,
                description: shape.description.clone(),
            });
        }
        let field_path = |name: &str| {
            let encoded = key(name);
            if encoded == name {
                format!("{path}.{name}")
            } else {
                format!("{path}[{encoded}]")
            }
        };
        match &shape.kind {
            ShapeKind::Object(object) => {
                for field in &object.fields {
                    walk(&field.shape, &field_path(&field.name), rows);
                }
                if let ExtraKeys::Open(extra) = &object.extra_keys {
                    walk(extra, &format!("{path}.*"), rows);
                }
            }
            ShapeKind::List(item) => walk(item, &format!("{path}[]"), rows),
            ShapeKind::Tuple(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{index}]"), rows);
                }
            }
            ShapeKind::Union(items) => {
                for (item, label) in items.iter().zip(shape.union_arm_labels()) {
                    walk(item, &format!("{path}|{label}"), rows);
                }
            }
            ShapeKind::Intersection(items) => {
                for item in items {
                    walk(item, path, rows);
                }
            }
            ShapeKind::Process(Some(process)) => {
                for param in &process.params {
                    walk(&param.shape, &format!("{path}({})", key(&param.name)), rows);
                }
                walk(&process.output, &format!("{path}.return"), rows);
            }
            _ => {}
        }
    }
    let mut rows = Vec::new();
    walk(shape, path, &mut rows);
    rows
}

/// Render a path-only metadata row from [`compact_note_rows`] for Markdown.
/// Backticks and line breaks in paths, defaults and prose are escaped.
///
/// ```rust,ignore
/// let row = CompactNoteRow { path: "input.q".into(), notes: vec![], description: Some("Query".into()) };
/// assert_eq!(compact_note_line(&row), "- `input.q` — Query");
/// ```
pub fn compact_note_line(row: &CompactNoteRow) -> String {
    let mut line = format!("- `{}`", escape(&row.path));
    if !row.notes.is_empty() {
        line.push_str(&format!(
            " ({})",
            row.notes
                .iter()
                .map(|note| escape(note))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(description) = &row.description {
        line.push_str(&format!(" — {}", escape(description)));
    }
    line
}

fn escape(text: &str) -> String {
    text.replace('`', "\\u0060").replace(['\n', '\r'], " ")
}
