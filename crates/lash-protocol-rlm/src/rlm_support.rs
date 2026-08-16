use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, RwLock};

use lash_core::PromptUsage;
use lash_rlm_types::{RlmCreateExtras, RlmTermination};
use lashlang::{
    BudgetedJsonProjectionConfig, BudgetedJsonProjector, Value as FlowValue, ValueProjectionContext,
};

pub(crate) const PRINT_HISTORY_PROJECTION_CONFIG: BudgetedJsonProjectionConfig =
    BudgetedJsonProjectionConfig::new(50 * 1024, 2_000, 6);
pub(crate) const BOUND_VARIABLE_PROJECTION_CONFIG: BudgetedJsonProjectionConfig =
    BudgetedJsonProjectionConfig::new(1024, 40, 3);

pub(crate) type SharedBoundVariablesPrompt = Arc<RwLock<Arc<str>>>;

pub(crate) fn print_history_projector() -> BudgetedJsonProjector {
    BudgetedJsonProjector::new(PRINT_HISTORY_PROJECTION_CONFIG)
}

pub(crate) fn bound_variable_projector() -> BudgetedJsonProjector {
    BudgetedJsonProjector::new(BOUND_VARIABLE_PROJECTION_CONFIG)
}

pub(crate) fn decode_rlm_options(
    options: &lash_core::ProtocolTurnOptions,
) -> Result<RlmCreateExtras, String> {
    if options.is_empty() {
        return Ok(RlmCreateExtras::default());
    }
    options
        .decode()
        .map_err(|err| format!("invalid RLM turn options: {err}"))
}

pub(crate) fn decode_rlm_termination_options(
    options: &lash_core::ProtocolTurnOptions,
) -> Result<RlmTermination, String> {
    decode_rlm_options(options).map(|options| options.termination)
}

/// Render the "Context Budget" line for the volatile turn-tail message.
/// Returns the formatted text (status line + optional escalation tail)
/// or `None` when there's nothing to say. The string is intentionally
/// per-turn dynamic; callers must place it AFTER the cache breakpoint,
/// never inside the cached system prompt.
pub fn format_budget_suffix(
    turn_index: usize,
    usage: Option<&PromptUsage>,
    max_budget_tokens: Option<usize>,
) -> Option<String> {
    let max = max_budget_tokens?;
    let usage = usage?;
    let used = usage.context_budget_tokens;
    if used == 0 {
        return None;
    }
    let pct = used.saturating_mul(100) / max.max(1);
    let mut content =
        format!("Turn: {turn_index} · Tokens: {used} · frame switch threshold: {max} ({pct}%).");
    if pct >= 60 {
        let tail = if used >= max {
            "Past the frame switch threshold. End this block with `control.continue_as(...)` now; do not call `finish` or do more work after it. Pack only what the new frame needs into `task` + `seed`."
        } else if pct >= 90 {
            "Budget tight — finish only the current step, then end the block with `control.continue_as(...)`."
        } else {
            "Look for a clean frame switch point; when you switch, make `control.continue_as(...)` the terminal action in the block."
        };
        content.push('\n');
        content.push_str(tail);
    }
    Some(content)
}

/// Render the "Bound Variables" prompt section from the live execution
/// namespace: the model's own scratch variables and any seeded computed
/// globals, shown the same way (value inline when small, type + size hint when
/// large). Read-only values are excluded by the caller and render in their own
/// section without value previews.
#[derive(Debug, Default)]
pub(crate) struct BoundVariableRenderCache {
    entries: BTreeMap<String, BoundVariableRenderCacheEntry>,
}

#[derive(Clone, Debug)]
struct BoundVariableRenderCacheEntry {
    /// Cheap structural hash of the variable's value. When it matches, the
    /// expensive rebuild (serialize / shape inference / preview) is skipped.
    value_hash: u64,
    shape: Option<JsonShape>,
    inline: Option<String>,
    size_hint: Option<String>,
    preview: Option<String>,
}

/// The expensive-to-compute parts of a variable's rendering.
struct BuiltRow {
    inline: Option<String>,
    shape: Option<JsonShape>,
    size_hint: Option<String>,
    preview: Option<String>,
}

/// One variable resolved for this render pass, either from cached value-derived
/// data or a fresh build. Registry-dependent lines are always rendered after
/// sorting so schema names remain deterministic.
struct WorkRow {
    name: String,
    value_hash: u64,
    shape: Option<JsonShape>,
    inline: Option<String>,
    size_hint: Option<String>,
    preview: Option<String>,
}

pub(crate) struct ReadOnlyVariableDoc {
    pub(crate) name: String,
    pub(crate) descriptor_type: String,
    pub(crate) value: Option<serde_json::Value>,
}

impl ReadOnlyVariableDoc {
    pub(crate) fn from_flow_value(name: String, value: &FlowValue) -> Self {
        let (descriptor_type, value) = match value {
            FlowValue::Projected(projected) => (
                projected.type_name().to_string(),
                serde_json::to_value(projected.materialize()).ok(),
            ),
            other => (
                flow_value_descriptor_type(other).to_string(),
                serde_json::to_value(other).ok(),
            ),
        };
        Self {
            name,
            descriptor_type,
            value,
        }
    }

    pub(crate) fn descriptor_only(name: String, descriptor_type: String) -> Self {
        Self {
            name,
            descriptor_type,
            value: None,
        }
    }
}

pub(crate) fn render_read_only_variables(mut docs: Vec<ReadOnlyVariableDoc>) -> String {
    docs.sort_by(|left, right| left.name.cmp(&right.name));
    let shapes = docs
        .iter()
        .map(|doc| (doc.name.clone(), doc.value.as_ref().map(infer_json_shape)))
        .collect::<BTreeMap<_, _>>();
    let mut registry = SchemaRegistry::default();
    for (name, shape) in &shapes {
        if let Some(shape) = shape {
            registry.register_root(name, shape);
        }
    }

    let mut lines = vec![
        "These read-only values are already in scope. Access them directly in `<lashlang>` blocks; do not recreate them manually.".to_string(),
        String::new(),
        "Read-only variables:".to_string(),
    ];
    for doc in &docs {
        let type_text = shapes
            .get(&doc.name)
            .and_then(Option::as_ref)
            .map(|shape| render_shape_inline(shape, &registry))
            .unwrap_or_else(|| normalize_descriptor_type(&doc.descriptor_type));
        lines.push(render_read_only_line(
            &doc.name,
            &type_text,
            &doc.descriptor_type,
        ));
    }
    append_schema_registry(&mut lines, &registry);
    lines.join("\n")
}

pub(crate) fn render_bound_variables(
    cache: &mut BoundVariableRenderCache,
    globals: &[(String, FlowValue)],
) -> Arc<str> {
    let mut lines = vec![
        "These variables are already bound in lashlang. Access them directly in `<lashlang>` blocks; do not recreate them manually.".to_string(),
        "Small values are shown in full; larger ones show only a truncated preview (record keys, or the head and tail of a list/string) — but the variable still holds its COMPLETE value. A short preview never means state was lost; `print` the variable (or the part you need) to see the rest.".to_string(),
    ];

    // Drop cache slots for variables that no longer exist.
    cache
        .entries
        .retain(|name, _| globals.iter().any(|(global, _)| global == name));

    // Reuse the cached build for an unchanged variable (detected by a cheap
    // structural value hash); only rebuild changed/new ones. Rebuilding —
    // serialize, shape inference, preview — is the expensive part, so this is
    // the win for globals that are stable across prompt builds.
    let mut rows: Vec<WorkRow> = Vec::with_capacity(globals.len());
    for (name, value) in globals {
        let hash = value_hash(value);
        match cache.entries.get(name) {
            Some(entry) if entry.value_hash == hash => rows.push(WorkRow {
                name: name.clone(),
                value_hash: hash,
                shape: entry.shape.clone(),
                inline: entry.inline.clone(),
                size_hint: entry.size_hint.clone(),
                preview: entry.preview.clone(),
            }),
            _ => {
                let built = build_bound_variable_row(value);
                rows.push(WorkRow {
                    name: name.clone(),
                    value_hash: hash,
                    shape: built.shape,
                    inline: built.inline,
                    size_hint: built.size_hint,
                    preview: built.preview,
                });
            }
        }
    }

    rows.sort_by(|left, right| left.name.cmp(&right.name));

    // Rebuild the schema registry from every row's shape in name order so its
    // generated type names are deterministic.
    let mut registry = SchemaRegistry::default();
    for row in &rows {
        if let Some(shape) = &row.shape {
            registry.register_root(&row.name, shape);
        }
    }

    lines.push(String::new());
    lines.push("Available variables:".to_string());
    lines.push(render_read_only_line(
        "history",
        "list[HistoryItem]",
        "list",
    ));
    for row in &rows {
        let line = render_row_line(row, &registry);
        cache.entries.insert(
            row.name.clone(),
            BoundVariableRenderCacheEntry {
                value_hash: row.value_hash,
                shape: row.shape.clone(),
                inline: row.inline.clone(),
                size_hint: row.size_hint.clone(),
                preview: row.preview.clone(),
            },
        );
        lines.push(line);
    }

    lines.push(String::new());
    lines.push("Schema:".to_string());
    lines.push("```text".to_string());
    lines.extend(history_item_type_definition());
    if !registry.definitions.is_empty() {
        lines.push(String::new());
    }
    for (idx, (name, shape)) in registry.definitions.iter().enumerate() {
        if idx > 0 {
            lines.push(String::new());
        }
        lines.extend(render_type_definition(name, shape, &registry));
    }
    lines.push("```".to_string());

    Arc::from(lines.join("\n"))
}

fn append_schema_registry(lines: &mut Vec<String>, registry: &SchemaRegistry) {
    if !registry.definitions.is_empty() {
        lines.push(String::new());
        lines.push("Schema:".to_string());
        lines.push("```text".to_string());
        for (idx, (name, shape)) in registry.definitions.iter().enumerate() {
            if idx > 0 {
                lines.push(String::new());
            }
            lines.extend(render_type_definition(name, shape, registry));
        }
        lines.push("```".to_string());
    }
}

fn render_read_only_line(name: &str, type_text: &str, descriptor_type: &str) -> String {
    format!(
        "- `{name}`: `{type_text}`, read-only (descriptor: `{}`)",
        normalize_descriptor_type(descriptor_type)
    )
}

fn normalize_descriptor_type(type_name: &str) -> String {
    match type_name {
        "string" => "str".to_string(),
        "number" => "float".to_string(),
        "integer" => "int".to_string(),
        "boolean" => "bool".to_string(),
        "object" | "record" => "record".to_string(),
        "array" | "list" => "list[any]".to_string(),
        "null" => "null".to_string(),
        other if !other.trim().is_empty() => other.to_string(),
        _ => "any".to_string(),
    }
}

fn flow_value_descriptor_type(value: &FlowValue) -> &'static str {
    match value {
        FlowValue::Null => "null",
        FlowValue::Bool(_) => "boolean",
        FlowValue::Number(_) => "number",
        FlowValue::String(_) => "string",
        FlowValue::Image(_) => "image",
        FlowValue::Resource(_) => "resource",
        FlowValue::Tuple(_) | FlowValue::List(_) => "list",
        FlowValue::Record(_) => "record",
        FlowValue::Projected(_) => "projected",
        FlowValue::Ref(_) => {
            unreachable!("VM heap references must be materialized before schema rendering")
        }
    }
}

fn history_item_type_definition() -> Vec<String> {
    vec![
        "type HistoryItem =".to_string(),
        "  | { kind: \"message\", id: str, role: enum[\"user\", \"system\", \"assistant\", \"event\"], content: str, attachments?: list[HistoryAttachment] }".to_string(),
        "  | { kind: \"lashlang_step\", id: str, protocol_iteration: int, code: str, output: list[str], images?: list[HistoryImage], error?: str | null, final_output?: any | null }".to_string(),
        "type HistoryAttachment = { id: str, media_type?: str | null, label?: str | null, source: str, reference: str }".to_string(),
        "type HistoryImage = { id: str, media_type: str, width?: int | null, height?: int | null, bytes: int, label?: str | null }".to_string(),
    ]
}

fn render_row_line(row: &WorkRow, registry: &SchemaRegistry) -> String {
    if let Some(inline) = &row.inline {
        // Value shown explicitly; no type/size hint needed.
        return format!("- `{}` = {inline}", row.name);
    }
    let shape = row
        .shape
        .as_ref()
        .expect("hinted variable has an inferred shape");
    let type_text = render_shape_inline(shape, registry);
    let mut line = match &row.size_hint {
        Some(size_hint) => format!("- `{}`: `{type_text}`, {size_hint}", row.name),
        None => format!("- `{}`: `{type_text}`", row.name),
    };
    if let Some(preview) = &row.preview {
        line.push_str(&format!(" ≈ {preview}"));
    }
    if row.size_hint.is_some() {
        // The preview is display-only; reassure the value is still live so the
        // model `print`s it rather than concluding state was lost.
        line.push_str(&format!(" (full value live — `print {}`)", row.name));
    }
    line
}

fn build_bound_variable_row(value: &FlowValue) -> BuiltRow {
    let projected = bound_variable_projector().project_blocking(ValueProjectionContext::new(value));
    let full =
        BudgetedJsonProjector::unbounded().project_blocking(ValueProjectionContext::new(value));
    if projected == full {
        return BuiltRow {
            inline: Some(projected),
            shape: None,
            size_hint: None,
            preview: None,
        };
    }

    let json = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    BuiltRow {
        inline: None,
        shape: Some(infer_json_shape(&json)),
        size_hint: render_value_size_hint(&json),
        preview: Some(projected),
    }
}

/// Cheap structural hash of a JSON value for change detection. Walks the value
/// but allocates nothing — unlike serializing it or inferring its shape, which
/// is exactly the work this lets us skip when the value is unchanged.
fn value_hash(value: &FlowValue) -> u64 {
    let mut hasher = DefaultHasher::new();
    let json = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    hash_json_value(&json, &mut hasher);
    hasher.finish()
}

fn hash_json_value<H: Hasher>(value: &serde_json::Value, hasher: &mut H) {
    match value {
        serde_json::Value::Null => 0u8.hash(hasher),
        serde_json::Value::Bool(flag) => {
            1u8.hash(hasher);
            flag.hash(hasher);
        }
        serde_json::Value::Number(number) => {
            2u8.hash(hasher);
            if let Some(int) = number.as_i64() {
                0u8.hash(hasher);
                int.hash(hasher);
            } else if let Some(uint) = number.as_u64() {
                1u8.hash(hasher);
                uint.hash(hasher);
            } else {
                2u8.hash(hasher);
                number.as_f64().unwrap_or(0.0).to_bits().hash(hasher);
            }
        }
        serde_json::Value::String(text) => {
            3u8.hash(hasher);
            text.hash(hasher);
        }
        serde_json::Value::Array(items) => {
            4u8.hash(hasher);
            items.len().hash(hasher);
            for item in items {
                hash_json_value(item, hasher);
            }
        }
        serde_json::Value::Object(map) => {
            5u8.hash(hasher);
            map.len().hash(hasher);
            for (key, val) in map {
                key.hash(hasher);
                hash_json_value(val, hasher);
            }
        }
    }
}

fn render_value_size_hint(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(_) => None,
        serde_json::Value::Number(_) => None,
        serde_json::Value::String(text) => {
            let lines = text.lines().count();
            if lines > 1 {
                Some(format!("len={}, lines={lines}", text.chars().count()))
            } else {
                Some(format!("len={}", text.chars().count()))
            }
        }
        serde_json::Value::Array(values) => Some(format!("len={}", values.len())),
        serde_json::Value::Object(map) => Some(format!("keys={}", map.len())),
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum JsonShape {
    Any,
    Null,
    Bool,
    Int,
    Float,
    Str,
    List(Box<JsonShape>),
    Record(BTreeMap<String, JsonShape>),
    Union(Vec<JsonShape>),
}

#[derive(Default)]
struct SchemaRegistry {
    names_by_key: BTreeMap<String, String>,
    definitions: Vec<(String, JsonShape)>,
    used_names: BTreeSet<String>,
}

impl SchemaRegistry {
    fn register_root(&mut self, root_name: &str, shape: &JsonShape) {
        self.register_shape(shape, &[root_name.to_string()]);
    }

    fn register_shape(&mut self, shape: &JsonShape, hint_segments: &[String]) {
        match shape {
            JsonShape::Record(fields) => {
                let key = canonical_shape_key(shape);
                if self.names_by_key.contains_key(&key) {
                    return;
                }
                for (field, child) in fields {
                    let child_segments = vec![singularize_segment(field)];
                    self.register_nested_shape(child, &child_segments);
                }
                let name = self.allocate_name(type_name_from_segments(hint_segments));
                self.names_by_key.insert(key, name.clone());
                self.definitions.push((name, shape.clone()));
            }
            JsonShape::List(item) => self.register_nested_list_item(item, hint_segments),
            JsonShape::Union(items) => {
                for item in items {
                    self.register_shape(item, hint_segments);
                }
            }
            JsonShape::Any
            | JsonShape::Null
            | JsonShape::Bool
            | JsonShape::Int
            | JsonShape::Float
            | JsonShape::Str => {}
        }
    }

    fn register_nested_shape(&mut self, shape: &JsonShape, hint_segments: &[String]) {
        match shape {
            JsonShape::Record(_) => self.register_shape(shape, hint_segments),
            JsonShape::List(item) => self.register_nested_list_item(item, hint_segments),
            JsonShape::Union(items) => {
                for item in items {
                    self.register_nested_shape(item, hint_segments);
                }
            }
            JsonShape::Any
            | JsonShape::Null
            | JsonShape::Bool
            | JsonShape::Int
            | JsonShape::Float
            | JsonShape::Str => {}
        }
    }

    fn register_nested_list_item(&mut self, item: &JsonShape, hint_segments: &[String]) {
        match item {
            JsonShape::Record(_) => {
                let mut item_segments = hint_segments.to_vec();
                item_segments.push("item".to_string());
                self.register_shape(item, &item_segments);
            }
            JsonShape::List(inner) => self.register_nested_list_item(inner, hint_segments),
            JsonShape::Union(items) => {
                for item in items {
                    self.register_nested_list_item(item, hint_segments);
                }
            }
            JsonShape::Any
            | JsonShape::Null
            | JsonShape::Bool
            | JsonShape::Int
            | JsonShape::Float
            | JsonShape::Str => {}
        }
    }

    fn allocate_name(&mut self, base: String) -> String {
        let base = if base.is_empty() {
            "Value".to_string()
        } else {
            base
        };
        if self.used_names.insert(base.clone()) {
            return base;
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format!("{base}{suffix}");
            if self.used_names.insert(candidate.clone()) {
                return candidate;
            }
            suffix += 1;
        }
    }
}

fn infer_json_shape(value: &serde_json::Value) -> JsonShape {
    match value {
        serde_json::Value::Null => JsonShape::Null,
        serde_json::Value::Bool(_) => JsonShape::Bool,
        serde_json::Value::Number(n) => {
            if n.is_f64() && n.as_f64().is_some_and(|v| v.fract() != 0.0) {
                JsonShape::Float
            } else {
                JsonShape::Int
            }
        }
        serde_json::Value::String(_) => JsonShape::Str,
        serde_json::Value::Array(values) => {
            let item_shape = values
                .iter()
                .map(infer_json_shape)
                .reduce(merge_shapes)
                .unwrap_or(JsonShape::Any);
            JsonShape::List(Box::new(item_shape))
        }
        serde_json::Value::Object(map) => JsonShape::Record(
            map.iter()
                .map(|(key, value)| (key.clone(), infer_json_shape(value)))
                .collect(),
        ),
    }
}

fn merge_shapes(left: JsonShape, right: JsonShape) -> JsonShape {
    use JsonShape as Shape;
    match (left, right) {
        (Shape::Any, _) | (_, Shape::Any) => Shape::Any,
        (left, right) if left == right => left,
        (Shape::Int, Shape::Float) | (Shape::Float, Shape::Int) => Shape::Float,
        (Shape::List(left), Shape::List(right)) => {
            Shape::List(Box::new(merge_shapes(*left, *right)))
        }
        (Shape::Record(left), Shape::Record(right)) if left.keys().eq(right.keys()) => {
            let merged = left
                .into_iter()
                .map(|(key, left_shape)| {
                    let right_shape = right.get(&key).cloned().unwrap_or(JsonShape::Any);
                    (key, merge_shapes(left_shape, right_shape))
                })
                .collect();
            Shape::Record(merged)
        }
        (Shape::Union(left), Shape::Union(right)) => {
            flatten_union(left.into_iter().chain(right).collect())
        }
        (Shape::Union(mut union), other) | (other, Shape::Union(mut union)) => {
            union.push(other);
            flatten_union(union)
        }
        (left, right) => flatten_union(vec![left, right]),
    }
}

fn flatten_union(shapes: Vec<JsonShape>) -> JsonShape {
    let mut flattened = Vec::new();
    for shape in shapes {
        match shape {
            JsonShape::Union(items) => flattened.extend(items),
            other => flattened.push(other),
        }
    }
    let mut by_key = BTreeMap::new();
    for shape in flattened {
        by_key.insert(canonical_shape_key(&shape), shape);
    }
    let deduped = by_key.into_values().collect::<Vec<_>>();
    if deduped.len() == 1 {
        deduped.into_iter().next().unwrap_or(JsonShape::Any)
    } else {
        JsonShape::Union(deduped)
    }
}

fn canonical_shape_key(shape: &JsonShape) -> String {
    match shape {
        JsonShape::Any => "any".to_string(),
        JsonShape::Null => "null".to_string(),
        JsonShape::Bool => "bool".to_string(),
        JsonShape::Int => "int".to_string(),
        JsonShape::Float => "float".to_string(),
        JsonShape::Str => "str".to_string(),
        JsonShape::List(item) => format!("list[{}]", canonical_shape_key(item)),
        JsonShape::Record(fields) => {
            let body = fields
                .iter()
                .map(|(field, shape)| format!("{field}:{}", canonical_shape_key(shape)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        JsonShape::Union(items) => {
            let mut parts = items.iter().map(canonical_shape_key).collect::<Vec<_>>();
            parts.sort();
            format!("union({})", parts.join("|"))
        }
    }
}

fn render_shape_inline(shape: &JsonShape, registry: &SchemaRegistry) -> String {
    match shape {
        JsonShape::Any => "any".to_string(),
        JsonShape::Null => "null".to_string(),
        JsonShape::Bool => "bool".to_string(),
        JsonShape::Int => "int".to_string(),
        JsonShape::Float => "float".to_string(),
        JsonShape::Str => "str".to_string(),
        JsonShape::List(item) => format!("list[{}]", render_shape_inline(item, registry)),
        JsonShape::Record(_) => registry
            .names_by_key
            .get(&canonical_shape_key(shape))
            .cloned()
            .unwrap_or_else(|| "record".to_string()),
        JsonShape::Union(items) => items
            .iter()
            .map(|item| render_shape_inline(item, registry))
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn render_type_definition(name: &str, shape: &JsonShape, registry: &SchemaRegistry) -> Vec<String> {
    match shape {
        JsonShape::Record(fields) => {
            let mut lines = vec![format!("type {name} = {{")];
            for (field, shape) in fields {
                lines.push(format!(
                    "  {field}: {},",
                    render_shape_inline(shape, registry)
                ));
            }
            lines.push("}".to_string());
            lines
        }
        _ => vec![format!(
            "type {name} = {}",
            render_shape_inline(shape, registry)
        )],
    }
}

fn type_name_from_segments(segments: &[String]) -> String {
    let joined = segments
        .iter()
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.trim_matches('_'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let mut out = String::new();
    for part in joined
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .filter(|part| !part.is_empty())
    {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            for ch in chars {
                out.push(ch.to_ascii_lowercase());
            }
        }
    }
    out
}

fn singularize_segment(segment: &str) -> String {
    if let Some(prefix) = segment.strip_suffix("ies") {
        return format!("{prefix}y");
    }
    if segment.len() > 1 && segment.ends_with('s') && !segment.ends_with("ss") {
        return segment[..segment.len() - 1].to_string();
    }
    segment.to_string()
}

#[cfg(test)]
mod bound_variable_tests {
    use super::*;
    use serde_json::json;

    fn globals(value: serde_json::Value) -> Vec<(String, FlowValue)> {
        value
            .as_object()
            .expect("object")
            .iter()
            .map(|(key, value)| (key.clone(), lashlang::from_json(value.clone())))
            .collect()
    }

    fn render_with_cache(cache: &mut BoundVariableRenderCache, value: serde_json::Value) -> String {
        let globals = globals(value);
        render_bound_variables(cache, &globals).to_string()
    }

    #[test]
    fn small_values_render_inline_without_type_or_size() {
        let g = globals(json!({ "inventory": ["lantern", "sword"], "count": 3 }));
        let mut cache = BoundVariableRenderCache::default();
        let rendered = render_bound_variables(&mut cache, &g);
        let s = &rendered;
        assert!(s.contains("- `inventory` = [\"lantern\",\"sword\"]"), "{s}");
        assert!(s.contains("- `count` = 3"), "{s}");
        // Inline values carry no redundant type/size hint.
        assert!(!s.contains("`inventory`:"), "{s}");
        assert!(!s.contains("len="), "{s}");
    }

    #[test]
    fn large_values_fall_back_to_type_and_size_hint() {
        let big: Vec<String> = (0..500).map(|i| format!("item-{i}")).collect();
        let g = globals(json!({ "big": big }));
        let mut cache = BoundVariableRenderCache::default();
        let rendered = render_bound_variables(&mut cache, &g);
        let s = &rendered;
        assert!(s.contains("- `big`:"), "{s}");
        assert!(s.contains("len=500"), "{s}");
        assert!(s.contains("items omitted"), "{s}");
    }

    #[test]
    fn large_record_degrades_to_keys_preview() {
        let rooms = serde_json::Value::Object(
            (0..30)
                .map(|i| (format!("room_{i:02}"), json!({ "exits": ["n", "s"] })))
                .collect(),
        );
        let g = globals(json!({ "map": rooms }));
        let mut cache = BoundVariableRenderCache::default();
        let s = render_bound_variables(&mut cache, &g).to_string();
        assert!(s.contains("`map`:"), "{s}"); // type still shown
        assert!(s.contains("keys=30"), "{s}"); // size still shown
        assert!(s.contains("≈ {"), "{s}"); // preview present
        assert!(s.contains("room_00"), "{s}"); // some keys shown
        assert!(s.contains("fields omitted"), "{s}"); // and the rest elided
    }

    #[test]
    fn large_list_degrades_to_head_and_tail() {
        let items: Vec<_> = (0..40).map(|i| json!(format!("note-{i:02}"))).collect();
        let g = globals(json!({ "notes": items }));
        let mut cache = BoundVariableRenderCache::default();
        let s = render_bound_variables(&mut cache, &g).to_string();
        assert!(s.contains("len=40"), "{s}");
        assert!(s.contains("note-00"), "{s}"); // head retained
        assert!(s.contains("note-39"), "{s}"); // tail retained
        assert!(s.contains("items omitted"), "{s}"); // middle elided
    }

    #[test]
    fn history_is_listed_without_volatile_length() {
        let mut cache = BoundVariableRenderCache::default();
        let s = render_with_cache(&mut cache, json!({ "task": "ship" }));

        assert!(
            s.contains("- `history`: `list[HistoryItem]`, read-only"),
            "{s}"
        );
        assert!(s.contains("type HistoryItem ="), "{s}");
        assert!(s.contains("kind: \"message\""), "{s}");
        assert!(s.contains("kind: \"lashlang_step\""), "{s}");
        assert!(
            !s.contains("- `history`: `list[HistoryItem]`, read-only, 7 entries"),
            "{s}"
        );

        assert!(!s.contains("Runtime notes:"), "{s}");
        assert!(!s.contains("currently has"), "{s}");
    }

    #[test]
    fn rows_remain_sorted_by_name_when_values_change() {
        let mut cache = BoundVariableRenderCache::default();
        let _ = render_with_cache(
            &mut cache,
            json!({ "a": "old", "b": "steady", "c": "steady" }),
        );

        let s = render_with_cache(
            &mut cache,
            json!({ "a": "new", "b": "steady", "c": "steady" }),
        );

        let a_idx = s.find("- `a` = new").expect("a row");
        let b_idx = s.find("- `b` = steady").expect("b row");
        let c_idx = s.find("- `c` = steady").expect("c row");
        assert!(a_idx < b_idx, "{s}");
        assert!(b_idx < c_idx, "{s}");
    }

    #[test]
    fn new_rows_are_inserted_in_name_order() {
        let mut cache = BoundVariableRenderCache::default();
        let _ = render_with_cache(&mut cache, json!({ "b": 1 }));

        let s = render_with_cache(&mut cache, json!({ "a": 2, "b": 1 }));

        let b_idx = s.find("- `b` = 1").expect("b row");
        let a_idx = s.find("- `a` = 2").expect("a row");
        assert!(a_idx < b_idx, "{s}");
    }

    #[test]
    fn new_rows_recompute_schema_names_from_stable_name_order() {
        let mut cache = BoundVariableRenderCache::default();
        let large = "z".repeat(2_000);
        let _ = render_with_cache(&mut cache, json!({ "z": { "id": 1, "body": large } }));

        let large = "z".repeat(2_000);
        let s = render_with_cache(
            &mut cache,
            json!({ "a": { "id": 2, "body": large }, "z": { "id": 1, "body": large } }),
        );

        let a_idx = s.find("- `a`: `A`, keys=2").expect("a row");
        let z_idx = s.find("- `z`: `A`, keys=2").expect("z row");
        assert!(a_idx < z_idx, "{s}");
    }

    #[test]
    fn summarized_shape_or_count_changes_update_row_hash() {
        let mut cache = BoundVariableRenderCache::default();
        let _ = render_with_cache(
            &mut cache,
            json!({ "big": (0..40).collect::<Vec<_>>(), "steady": "same" }),
        );

        let s = render_with_cache(
            &mut cache,
            json!({ "big": (0..41).collect::<Vec<_>>(), "steady": "same" }),
        );

        let big_idx = s.find("- `big`: `list[int]`, len=41").expect("big row");
        let steady_idx = s.find("- `steady` = same").expect("steady row");
        assert!(big_idx < steady_idx, "{s}");
    }

    #[test]
    fn removed_rows_do_not_affect_name_order() {
        let mut cache = BoundVariableRenderCache::default();
        let _ = render_with_cache(&mut cache, json!({ "a": 1, "b": 2 }));
        let _ = render_with_cache(&mut cache, json!({ "b": 2 }));

        let s = render_with_cache(&mut cache, json!({ "a": 1, "b": 2 }));

        let a_idx = s.find("- `a` = 1").expect("a row");
        let b_idx = s.find("- `b` = 2").expect("b row");
        assert!(a_idx < b_idx, "{s}");
    }
}
