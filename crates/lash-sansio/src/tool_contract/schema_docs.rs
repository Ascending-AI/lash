//! JSON-Schema -> tool signature/doc projector.
//!
//! Self-contained projection of `schemars`-generated JSON Schema into the
//! compact parameter/field signature docs surfaced to models. Split out of
//! `tool_contract.rs`; only the entry points used by the contract types are
//! `pub(crate)`.

use std::borrow::Cow;

use super::*;

const COMPACT_SCHEMA_MAX_DEPTH: usize = 8;

pub fn schema_for<T>() -> serde_json::Value
where
    T: schemars::JsonSchema,
{
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| serde_json::json!({}))
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParameterDoc {
    pub(crate) name: String,
    pub(crate) type_label: String,
    pub(crate) required: bool,
    pub(crate) nullable: bool,
    pub(crate) description: Option<String>,
    pub(crate) default_value: Option<serde_json::Value>,
    pub(crate) enum_values: Vec<serde_json::Value>,
    pub(crate) minimum: Option<serde_json::Value>,
    pub(crate) maximum: Option<serde_json::Value>,
    pub(crate) min_length: Option<u64>,
    pub(crate) max_length: Option<u64>,
    pub(crate) min_items: Option<u64>,
    pub(crate) max_items: Option<u64>,
    pub(crate) item_type: Option<String>,
}

impl ParameterDoc {
    pub(crate) fn signature_fragment(&self) -> String {
        let mut out = if self.required {
            format!("{}: {}", self.name, self.type_label)
        } else {
            format!("{}?: {}", self.name, self.type_label)
        };
        let constraints = self.constraint_fragments();
        if !constraints.is_empty() {
            out.push(' ');
            out.push_str(&constraints.join(" "));
        }
        if let Some(default) = &self.default_value {
            out.push_str(" = ");
            out.push_str(&display_default_value(default));
        }
        out
    }

    fn constraint_fragments(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.enum_values.is_empty() && !self.type_label.starts_with("enum[") {
            out.push(format!(
                "in {}",
                self.enum_values
                    .iter()
                    .map(display_default_value)
                    .collect::<Vec<_>>()
                    .join("|")
            ));
        }
        if let Some(minimum) = &self.minimum {
            out.push(format!(">= {}", display_default_value(minimum)));
        }
        if let Some(maximum) = &self.maximum {
            out.push(format!("<= {}", display_default_value(maximum)));
        }
        if let Some(min_length) = self.min_length {
            out.push(format!("min_len {min_length}"));
        }
        if let Some(max_length) = self.max_length {
            out.push(format!("max_len {max_length}"));
        }
        if let Some(min_items) = self.min_items {
            out.push(format!("min_items {min_items}"));
        }
        if let Some(max_items) = self.max_items {
            out.push(format!("max_items {max_items}"));
        }
        out
    }

    pub(crate) fn into_value(self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("name".to_string(), serde_json::json!(self.name));
        out.insert("type".to_string(), serde_json::json!(self.type_label));
        out.insert("required".to_string(), serde_json::json!(self.required));
        if self.nullable {
            out.insert("nullable".to_string(), serde_json::json!(true));
        }
        if let Some(description) = self.description.filter(|value| !value.trim().is_empty()) {
            out.insert("description".to_string(), serde_json::json!(description));
        }
        if let Some(default_value) = self.default_value {
            out.insert("default".to_string(), default_value);
        }
        if !self.enum_values.is_empty() {
            out.insert("enum".to_string(), serde_json::json!(self.enum_values));
        }
        if let Some(value) = self.minimum {
            out.insert("minimum".to_string(), value);
        }
        if let Some(value) = self.maximum {
            out.insert("maximum".to_string(), value);
        }
        if let Some(value) = self.min_length {
            out.insert("min_length".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.max_length {
            out.insert("max_length".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.min_items {
            out.insert("min_items".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.max_items {
            out.insert("max_items".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.item_type {
            out.insert("items".to_string(), serde_json::json!(value));
        }
        out.insert(
            "signature".to_string(),
            serde_json::json!(parameter_signature_from_value(&out)),
        );
        serde_json::Value::Object(out)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct FieldDoc {
    path: String,
    type_label: String,
    required: bool,
    nullable: bool,
    description: Option<String>,
    enum_values: Vec<serde_json::Value>,
    minimum: Option<serde_json::Value>,
    maximum: Option<serde_json::Value>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    min_items: Option<u64>,
    max_items: Option<u64>,
    item_type: Option<String>,
}

impl FieldDoc {
    fn from_schema(path: String, schema: &serde_json::Value, required: bool) -> Self {
        let (type_label, nullable) = schema_type_label_and_nullability(schema);
        Self {
            path,
            type_label,
            required,
            nullable,
            description: schema
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            enum_values: schema
                .get("enum")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|value| !value.is_null())
                .collect(),
            minimum: schema
                .get("minimum")
                .or_else(|| schema.get("exclusiveMinimum"))
                .cloned(),
            maximum: schema
                .get("maximum")
                .or_else(|| schema.get("exclusiveMaximum"))
                .cloned(),
            min_length: schema.get("minLength").and_then(serde_json::Value::as_u64),
            max_length: schema.get("maxLength").and_then(serde_json::Value::as_u64),
            min_items: schema.get("minItems").and_then(serde_json::Value::as_u64),
            max_items: schema.get("maxItems").and_then(serde_json::Value::as_u64),
            item_type: schema
                .get("items")
                .map(schema_type_label)
                .filter(|value| value != "any"),
        }
    }

    fn into_value(self) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        out.insert("path".to_string(), serde_json::json!(self.path));
        out.insert("type".to_string(), serde_json::json!(self.type_label));
        out.insert("required".to_string(), serde_json::json!(self.required));
        if self.nullable {
            out.insert("nullable".to_string(), serde_json::json!(true));
        }
        if let Some(description) = self.description.filter(|value| !value.trim().is_empty()) {
            out.insert("description".to_string(), serde_json::json!(description));
        }
        if !self.enum_values.is_empty() {
            out.insert("enum".to_string(), serde_json::json!(self.enum_values));
        }
        if let Some(value) = self.minimum {
            out.insert("minimum".to_string(), value);
        }
        if let Some(value) = self.maximum {
            out.insert("maximum".to_string(), value);
        }
        if let Some(value) = self.min_length {
            out.insert("min_length".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.max_length {
            out.insert("max_length".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.min_items {
            out.insert("min_items".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.max_items {
            out.insert("max_items".to_string(), serde_json::json!(value));
        }
        if let Some(value) = self.item_type {
            out.insert("items".to_string(), serde_json::json!(value));
        }
        out.insert(
            "signature".to_string(),
            serde_json::json!(field_signature_from_value(&out)),
        );
        serde_json::Value::Object(out)
    }
}

pub(crate) fn schema_parameter_docs(schema: &serde_json::Value) -> Vec<ParameterDoc> {
    let schema = resolve_schema_refs(schema);
    let required_order = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    let required = required_order
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let mut params = properties
        .iter()
        .map(|(name, schema)| parameter_doc(name, schema, required.contains(name.as_str())))
        .collect::<Vec<_>>();
    params.sort_by(|left, right| {
        match (
            required_order
                .iter()
                .position(|name| *name == left.name.as_str()),
            required_order
                .iter()
                .position(|name| *name == right.name.as_str()),
        ) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.name.cmp(&right.name),
        }
    });
    params
}

pub(crate) fn return_field_metadata(schema: &serde_json::Value) -> Vec<serde_json::Value> {
    let schema = resolve_schema_refs(schema);
    let mut fields = Vec::new();
    collect_return_fields("", &schema, true, &mut fields);
    merge_return_fields(fields)
        .into_iter()
        .map(FieldDoc::into_value)
        .collect()
}

fn resolve_schema_refs(schema: &serde_json::Value) -> Cow<'_, serde_json::Value> {
    if !schema_contains_ref(schema) {
        return Cow::Borrowed(schema);
    }
    let mut resolving = Vec::new();
    Cow::Owned(resolve_schema_ref_value(schema, schema, &mut resolving))
}

fn schema_contains_ref(schema: &serde_json::Value) -> bool {
    match schema {
        serde_json::Value::Object(map) => {
            map.contains_key("$ref") || map.values().any(schema_contains_ref)
        }
        serde_json::Value::Array(values) => values.iter().any(schema_contains_ref),
        _ => false,
    }
}

fn resolve_schema_ref_value(
    root: &serde_json::Value,
    schema: &serde_json::Value,
    resolving: &mut Vec<String>,
) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str)
                && let Some(pointer) = reference.strip_prefix('#')
            {
                if resolving.iter().any(|active| active == reference) {
                    return serde_json::json!({});
                }
                if let Some(target) = root.pointer(pointer) {
                    resolving.push(reference.to_string());
                    let mut resolved = resolve_schema_ref_value(root, target, resolving);
                    resolving.pop();

                    let sibling_count = map.keys().filter(|key| key.as_str() != "$ref").count();
                    if sibling_count == 0 {
                        return resolved;
                    }
                    if let serde_json::Value::Object(resolved_map) = &mut resolved {
                        for (key, value) in map {
                            if key == "$ref" {
                                continue;
                            }
                            resolved_map.insert(
                                key.clone(),
                                resolve_schema_ref_value(root, value, resolving),
                            );
                        }
                        return resolved;
                    }
                }
            }

            serde_json::Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            resolve_schema_ref_value(root, value, resolving),
                        )
                    })
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| resolve_schema_ref_value(root, value, resolving))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn collect_return_fields(
    path: &str,
    schema: &serde_json::Value,
    required: bool,
    fields: &mut Vec<FieldDoc>,
) {
    if let Some(any_of) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(serde_json::Value::as_array)
    {
        if should_emit_return_field(path, schema) {
            fields.push(FieldDoc::from_schema(path.to_string(), schema, required));
        }
        for subschema in any_of {
            collect_return_fields(path, subschema, required, fields);
        }
        return;
    }

    let schema_type = schema
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            schema
                .get("type")
                .and_then(serde_json::Value::as_array)
                .and_then(|types| {
                    let non_null = types
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .filter(|ty| *ty != "null")
                        .collect::<Vec<_>>();
                    if non_null.len() == 1 {
                        Some(non_null[0].to_string())
                    } else {
                        None
                    }
                })
        });

    match schema_type.as_deref() {
        Some("object") => {
            if should_emit_return_field(path, schema) {
                fields.push(FieldDoc::from_schema(path.to_string(), schema, required));
            }
            let required_properties = schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            if let Some(properties) = schema
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                for (name, property_schema) in properties {
                    collect_return_fields(
                        &join_compact_path(path, name),
                        property_schema,
                        required_properties.contains(name.as_str()),
                        fields,
                    );
                }
            }
        }
        Some("array") => {
            if should_emit_return_field(path, schema) {
                fields.push(FieldDoc::from_schema(path.to_string(), schema, required));
            }
            if let Some(items) = schema.get("items") {
                collect_return_fields(&format!("{path}[]"), items, true, fields);
            }
        }
        _ => {
            if !path.is_empty() {
                fields.push(FieldDoc::from_schema(path.to_string(), schema, required));
            }
        }
    }
}

fn should_emit_return_field(path: &str, schema: &serde_json::Value) -> bool {
    !path.is_empty()
        && (schema
            .get("description")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
            || schema.get("enum").is_some()
            || schema.get("minimum").is_some()
            || schema.get("maximum").is_some()
            || schema.get("minLength").is_some()
            || schema.get("maxLength").is_some()
            || schema.get("minItems").is_some()
            || schema.get("maxItems").is_some())
}

fn join_compact_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}

fn merge_return_fields(fields: Vec<FieldDoc>) -> Vec<FieldDoc> {
    let mut merged = Vec::<FieldDoc>::new();
    for field in fields {
        if let Some(existing) = merged
            .iter_mut()
            .find(|existing| existing.path == field.path)
        {
            existing.merge(field);
        } else {
            merged.push(field);
        }
    }
    merged
}

impl FieldDoc {
    fn merge(&mut self, other: FieldDoc) {
        self.type_label = merge_type_labels(&self.type_label, &other.type_label);
        self.required |= other.required;
        self.nullable |= other.nullable || type_label_is_nullable(&other.type_label);
        if self.nullable && !type_label_is_nullable(&self.type_label) {
            self.type_label = merge_type_labels(&self.type_label, "null");
        }
        if self
            .description
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        {
            self.description = other.description;
        }
        for value in other.enum_values {
            if !self.enum_values.iter().any(|existing| existing == &value) {
                self.enum_values.push(value);
            }
        }
        if self.minimum.is_none() {
            self.minimum = other.minimum;
        }
        if self.maximum.is_none() {
            self.maximum = other.maximum;
        }
        if self.min_length.is_none() {
            self.min_length = other.min_length;
        }
        if self.max_length.is_none() {
            self.max_length = other.max_length;
        }
        if self.min_items.is_none() {
            self.min_items = other.min_items;
        }
        if self.max_items.is_none() {
            self.max_items = other.max_items;
        }
        if self.item_type.is_none() {
            self.item_type = other.item_type;
        }
    }
}

fn merge_type_labels(left: &str, right: &str) -> String {
    let mut labels = Vec::<String>::new();
    for label in left.split(" | ").chain(right.split(" | ")) {
        let label = label.trim();
        if label.is_empty() || label == "any" && (!left.is_empty() || !right.is_empty()) {
            continue;
        }
        if !labels.iter().any(|existing| existing == label) {
            labels.push(label.to_string());
        }
    }
    if labels.is_empty() {
        return "any".to_string();
    }
    labels.sort_by(|left, right| match (*left == "null", *right == "null") {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => std::cmp::Ordering::Equal,
    });
    labels.join(" | ")
}

fn type_label_is_nullable(label: &str) -> bool {
    label.split(" | ").any(|part| part.trim() == "null")
}

fn parameter_doc(name: &str, schema: &serde_json::Value, required: bool) -> ParameterDoc {
    let type_label = compact_schema_label(schema);
    let nullable = type_label_is_nullable(&type_label);
    ParameterDoc {
        name: name.to_string(),
        type_label,
        required,
        nullable,
        description: schema
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        default_value: schema.get("default").cloned(),
        enum_values: schema
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|value| !value.is_null())
            .collect(),
        minimum: schema
            .get("minimum")
            .or_else(|| schema.get("exclusiveMinimum"))
            .cloned(),
        maximum: schema
            .get("maximum")
            .or_else(|| schema.get("exclusiveMaximum"))
            .cloned(),
        min_length: schema.get("minLength").and_then(serde_json::Value::as_u64),
        max_length: schema.get("maxLength").and_then(serde_json::Value::as_u64),
        min_items: schema.get("minItems").and_then(serde_json::Value::as_u64),
        max_items: schema.get("maxItems").and_then(serde_json::Value::as_u64),
        item_type: schema
            .get("items")
            .map(compact_schema_label)
            .filter(|value| value != "any"),
    }
}

pub(crate) fn compact_doc_line(value: &serde_json::Value) -> Option<String> {
    let signature = value.get("signature")?.as_str()?.trim();
    if signature.is_empty() {
        return None;
    }
    let description = value
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match description {
        Some(description) => format!("- `{signature}` — {description}"),
        None => format!("- `{signature}`"),
    })
}

fn parameter_signature_from_value(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let name = map
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    doc_signature_from_value(name, map)
}

fn field_signature_from_value(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let path = map
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    doc_signature_from_value(path, map)
}

fn doc_signature_from_value(
    name: &str,
    map: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let ty = map
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("any");
    let required = map
        .get("required")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mut out = if required {
        format!("{name}: {ty}")
    } else {
        format!("{name}?: {ty}")
    };

    let mut constraints = Vec::new();
    if let Some(values) = map.get("enum").and_then(serde_json::Value::as_array)
        && !ty.starts_with("enum[")
    {
        constraints.push(format!(
            "in {}",
            values
                .iter()
                .map(display_default_value)
                .collect::<Vec<_>>()
                .join("|")
        ));
    }
    if let Some(value) = map.get("minimum") {
        constraints.push(format!(">= {}", display_default_value(value)));
    }
    if let Some(value) = map.get("maximum") {
        constraints.push(format!("<= {}", display_default_value(value)));
    }
    if let Some(value) = map.get("min_length").and_then(serde_json::Value::as_u64) {
        constraints.push(format!("min_len {value}"));
    }
    if let Some(value) = map.get("max_length").and_then(serde_json::Value::as_u64) {
        constraints.push(format!("max_len {value}"));
    }
    if let Some(value) = map.get("min_items").and_then(serde_json::Value::as_u64) {
        constraints.push(format!("min_items {value}"));
    }
    if let Some(value) = map.get("max_items").and_then(serde_json::Value::as_u64) {
        constraints.push(format!("max_items {value}"));
    }
    if !constraints.is_empty() {
        out.push(' ');
        out.push_str(&constraints.join(" "));
    }
    if let Some(default) = map.get("default") {
        out.push_str(" = ");
        out.push_str(&display_default_value(default));
    }
    out
}

fn schema_type_label(schema: &serde_json::Value) -> String {
    schema_type_label_and_nullability(schema).0
}

pub(crate) fn compact_schema_label(schema: &serde_json::Value) -> String {
    let schema = resolve_schema_refs(schema);
    compact_schema_label_resolved(&schema)
}

fn compact_schema_label_resolved(schema: &serde_json::Value) -> String {
    compact_schema_label_at_depth(schema, 0)
}

fn compact_schema_label_at_depth(schema: &serde_json::Value, depth: usize) -> String {
    if let Some(any_of) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(serde_json::Value::as_array)
    {
        let labels = any_of
            .iter()
            .map(|schema| compact_schema_label_at_depth(schema, depth))
            .collect::<std::collections::BTreeSet<_>>();
        let joined = labels.into_iter().collect::<Vec<_>>().join(" | ");
        return if joined.is_empty() {
            "any".to_string()
        } else {
            joined
        };
    }

    if let Some(types) = schema.get("type").and_then(serde_json::Value::as_array) {
        let labels = types
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|ty| *ty != "null")
            .map(|ty| compact_schema_label_at_depth(&serde_json::json!({ "type": ty }), depth))
            .collect::<std::collections::BTreeSet<_>>();
        let mut out = if labels.is_empty() {
            "any".to_string()
        } else {
            labels.into_iter().collect::<Vec<_>>().join(" | ")
        };
        if types.iter().any(|value| value.as_str() == Some("null")) {
            out.push_str(" | null");
        }
        return out;
    }

    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("array") => schema
            .get("items")
            .map(|schema| compact_schema_label_at_depth(schema, depth.saturating_add(1)))
            .filter(|value| !value.is_empty())
            .map(|item| format!("list[{item}]"))
            .unwrap_or_else(|| "list[any]".to_string()),
        Some("object") if depth < COMPACT_SCHEMA_MAX_DEPTH => compact_record_label(schema, depth),
        Some("object") => "record".to_string(),
        _ => schema_type_label(schema),
    }
}

fn compact_record_label(schema: &serde_json::Value, depth: usize) -> String {
    let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return "record".to_string();
    };
    if properties.is_empty() {
        return "record".to_string();
    }

    let required_order = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    let required = required_order
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let mut fields = properties
        .iter()
        .map(|(name, field_schema)| {
            let suffix = if required.contains(name.as_str()) {
                ""
            } else {
                "?"
            };
            (
                name,
                format!(
                    "{name}{suffix}: {}",
                    compact_schema_label_at_depth(field_schema, depth.saturating_add(1))
                ),
            )
        })
        .collect::<Vec<_>>();
    fields.sort_by(|(left, _), (right, _)| {
        match (
            required_order.iter().position(|name| name == left),
            required_order.iter().position(|name| name == right),
        ) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.cmp(right),
        }
    });
    format!(
        "record{{{}}}",
        fields
            .into_iter()
            .map(|(_, field)| field)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(crate) fn compact_examples(examples: &[String], limit: usize) -> Vec<String> {
    examples
        .iter()
        .map(|example| example.trim())
        .filter(|example| !example.is_empty())
        .take(limit)
        .map(|example| {
            if example.chars().count() <= COMPACT_TOOL_EXAMPLE_CHAR_LIMIT {
                return example.to_string();
            }
            let mut out = example
                .chars()
                .take(COMPACT_TOOL_EXAMPLE_CHAR_LIMIT.saturating_sub(3))
                .collect::<String>();
            out.push_str("...");
            out
        })
        .collect()
}

fn schema_type_label_and_nullability(schema: &serde_json::Value) -> (String, bool) {
    if let Some(values) = schema.get("enum").and_then(serde_json::Value::as_array) {
        let variants = values
            .iter()
            .filter(|value| !value.is_null())
            .map(display_default_value)
            .collect::<Vec<_>>();
        let nullable = values.iter().any(serde_json::Value::is_null);
        if !variants.is_empty() {
            let mut label = format!("enum[{}]", variants.join(", "));
            if nullable {
                label.push_str(" | null");
            }
            return (label, nullable);
        }
    }

    if let Some(types) = schema.get("type").and_then(serde_json::Value::as_array) {
        let nullable = types.iter().any(|value| value.as_str() == Some("null"));
        let non_null = types
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|ty| *ty != "null")
            .map(schema_type_name)
            .collect::<Vec<_>>();
        let mut label = if non_null.is_empty() {
            "any".to_string()
        } else {
            non_null.join(" | ")
        };
        if nullable {
            label.push_str(" | null");
        }
        return (label, nullable);
    }

    if let Some(any_of) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(serde_json::Value::as_array)
    {
        let mut nullable = false;
        let mut labels = Vec::new();
        for subschema in any_of {
            let (label, is_nullable) = schema_type_label_and_nullability(subschema);
            nullable |= is_nullable || label == "null";
            if label != "null" && !labels.iter().any(|existing| existing == &label) {
                labels.push(label);
            }
        }
        let mut label = if labels.is_empty() {
            "any".to_string()
        } else {
            labels.join(" | ")
        };
        if nullable {
            label.push_str(" | null");
        }
        return (label, nullable);
    }

    let nullable = schema.get("type").and_then(serde_json::Value::as_str) == Some("null");
    let label = match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("array") => {
            let item = schema
                .get("items")
                .map(schema_type_label)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "any".to_string());
            format!("list[{item}]")
        }
        Some(ty) => schema_type_name(ty),
        None => "any".to_string(),
    };
    (label, nullable)
}

fn schema_type_name(ty: &str) -> String {
    match ty {
        "string" => "str".to_string(),
        "integer" => "int".to_string(),
        "number" => "float".to_string(),
        "boolean" => "bool".to_string(),
        "object" => "record".to_string(),
        "array" => "list".to_string(),
        "null" => "null".to_string(),
        _ => "any".to_string(),
    }
}

fn display_default_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::String(v) => format!("{v:?}"),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}

#[cfg(test)]
mod schema_doc_tests {
    use super::*;

    #[test]
    fn resolve_schema_refs_borrows_schema_without_refs() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            }
        });

        match resolve_schema_refs(&schema) {
            Cow::Borrowed(value) => assert!(std::ptr::eq(value, &schema)),
            Cow::Owned(_) => panic!("schema without refs should not be cloned"),
        }
    }

    #[test]
    fn resolve_schema_refs_owns_schema_with_local_ref_expansion() {
        let schema = serde_json::json!({
            "$defs": {
                "path": { "type": "string" }
            },
            "type": "object",
            "properties": {
                "path": { "$ref": "#/$defs/path" }
            }
        });

        let resolved = resolve_schema_refs(&schema);
        assert!(matches!(resolved, Cow::Owned(_)));
        assert_eq!(
            resolved
                .get("properties")
                .and_then(|value| value.get("path"))
                .and_then(|value| value.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("string")
        );
    }

    #[test]
    fn compact_schema_labels_cap_recursive_object_depth() {
        let mut schema = serde_json::json!({ "type": "string" });
        for _ in 0..(COMPACT_SCHEMA_MAX_DEPTH + 3) {
            schema = serde_json::json!({
                "type": "object",
                "properties": { "child": schema },
                "required": ["child"]
            });
        }

        let label = compact_schema_label(&schema);
        assert_eq!(
            label.matches("record{").count(),
            COMPACT_SCHEMA_MAX_DEPTH,
            "{label}"
        );
        assert!(label.contains("child: record"), "{label}");
    }
}
