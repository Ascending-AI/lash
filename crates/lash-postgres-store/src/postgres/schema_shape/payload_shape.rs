//! Rust-type-derived shapes for structured JSON payload columns.
//!
//! PostgreSQL can describe a `TEXT` column but cannot introspect the Rust value
//! serialized into it. This registry supplies that missing half of the schema:
//! each registered column is paired with the `schemars` shape of the type Lash
//! writes there. The artifact renderer records only structural JSON Schema
//! facts (field names, types, requiredness, references, composition, and
//! decode-controlling literals and bounds), never descriptions, examples, or
//! defaults.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// JSON Schema annotations that cannot change whether Serde accepts stored
/// bytes. Everything else is captured by default: an unfamiliar assertion is
/// safer as a noisy version bump than as an invisible decode break.
const NON_DECODING_ANNOTATIONS: &[&str] = &[
    "$comment",
    "$id",
    "$schema",
    "default",
    "deprecated",
    "description",
    "examples",
    "readOnly",
    "title",
    "writeOnly",
];

/// Structural JSON payload shape for one registered column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PayloadShape {
    /// Rust type that owns the serialized payload.
    pub(super) rust_type: String,
    /// Sorted JSON-Schema paths and their structural type facts.
    pub(super) entries: BTreeMap<String, String>,
}

impl PayloadShape {
    /// Derives a payload shape from the Rust type rather than from a sample row.
    #[cfg(test)]
    pub(super) fn of<T: JsonSchema>() -> Self {
        let schema = serde_json::to_value(schemars::schema_for!(T))
            .expect("schemars root schemas are serializable");
        Self::from_schema::<T>(&schema)
    }

    fn from_schema<T: JsonSchema>(schema: &Value) -> Self {
        let mut entries = BTreeMap::new();
        collect_shape_entries(schema, "", &mut entries);
        Self {
            rust_type: T::schema_name(),
            entries,
        }
    }

    fn include_persisted_projection(
        &mut self,
        backend: &str,
        carrier: &str,
        projection: PayloadShape,
    ) {
        let prefix = child_path(
            &child_path(&child_path("", "persisted-by"), backend),
            carrier,
        );
        self.entries
            .insert(child_path(&prefix, "rust-type"), projection.rust_type);
        for (path, value) in projection.entries {
            self.entries.insert(format!("{prefix}{path}"), value);
        }
    }
}

struct PayloadRegistration {
    shape: PayloadShape,
    #[cfg(test)]
    fingerprints: BTreeMap<(String, String, String), String>,
}

impl PayloadRegistration {
    fn of<T: JsonSchema>(_backend: &str, _carrier: &str) -> Self {
        let schema = serde_json::to_value(schemars::schema_for!(T))
            .expect("schemars root schemas are serializable");
        let shape = PayloadShape::from_schema::<T>(&schema);
        Self {
            shape,
            #[cfg(test)]
            fingerprints: [(
                (_backend.to_string(), _carrier.to_string(), T::schema_name()),
                unfiltered_schema_fingerprint(&schema),
            )]
            .into_iter()
            .collect(),
        }
    }

    fn include_persisted_projection(&mut self, backend: &str, carrier: &str, projection: Self) {
        self.shape
            .include_persisted_projection(backend, carrier, projection.shape);
        #[cfg(test)]
        self.fingerprints.extend(projection.fingerprints);
    }
}

/// PostgreSQL blob columns selected for Rust-type-derived component-version
/// gating. The first registered carrier is the concrete FIG-1219 blind spot.
///
/// Keep this intentionally explicit: registering a column is a durability
/// decision, and an accidental broad scan of every `*_json` column would mix
/// versioned records, enums, and intentionally opaque JSON values into this
/// component-version gate.
pub(super) fn registered_payload_shapes() -> BTreeMap<(String, String), PayloadShape> {
    registered_payloads()
        .into_iter()
        .map(|(identity, registration)| (identity, registration.shape))
        .collect()
}

fn registered_payloads() -> BTreeMap<(String, String), PayloadRegistration> {
    let mut session_meta = PayloadRegistration::of::<lash_core::SessionMeta>(
        "postgres",
        "lash_session_meta.meta_json",
    );
    session_meta.include_persisted_projection(
        "sqlite",
        "session_meta.relation_json",
        PayloadRegistration::of::<lash_core::SessionRelation>(
            "sqlite",
            "session_meta.relation_json",
        ),
    );
    [(
        ("lash_session_meta".to_string(), "meta_json".to_string()),
        session_meta,
    )]
    .into_iter()
    .collect()
}

#[cfg(test)]
fn registered_payload_fingerprints() -> BTreeMap<(String, String, String), String> {
    registered_payloads()
        .into_values()
        .flat_map(|registration| registration.fingerprints)
        .collect()
}

/// Projects a JSON Schema document into stable shape-only path entries.
///
/// The projection is deliberately conservative: every schema assertion and
/// applicator is retained unless it is in [`NON_DECODING_ANNOTATIONS`]. Enum,
/// const, range, format, length, and similar values can all distinguish bytes a
/// decoder accepts from bytes it rejects. Conjunctive branches and branches
/// with unique required tag literals use durable wire identities; ambiguous
/// alternatives retain ordinal position because Serde's untagged decoding may
/// use first-match declaration order.
fn collect_shape_entries(value: &Value, path: &str, entries: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if NON_DECODING_ANNOTATIONS.contains(&key.as_str()) {
                    continue;
                }
                match key.as_str() {
                    "$ref" => {
                        let reference = child.as_str().unwrap_or_default();
                        entries.insert(
                            child_path(path, "$ref"),
                            reference
                                .rsplit('/')
                                .next()
                                .unwrap_or(reference)
                                .to_string(),
                        );
                    }
                    "type" => {
                        entries.insert(child_path(path, "type"), type_list(child));
                    }
                    "required" => {
                        if let Some(required) = child.as_array() {
                            for field in required.iter().filter_map(Value::as_str) {
                                entries.insert(
                                    child_path(&child_path(path, "required"), field),
                                    "field".into(),
                                );
                            }
                        }
                    }
                    "enum" => {
                        if let Some(values) = child.as_array() {
                            entries
                                .insert(child_path(path, "enum-types"), value_types(values.iter()));
                            entries.insert(child_path(path, "enum-values"), literal_values(values));
                        }
                    }
                    "const" => {
                        entries.insert(
                            child_path(path, "const-type"),
                            json_value_type(child).to_string(),
                        );
                        entries.insert(child_path(path, "const-value"), literal_value(child));
                    }
                    "allOf" | "anyOf" | "oneOf" => {
                        if let Some(children) = child.as_array() {
                            collect_composition_branches(children, path, key, entries);
                        }
                    }
                    "properties" => {
                        if let Some(children) = child.as_object() {
                            for (name, schema) in children {
                                let property_path = child_path(&child_path(path, key), name);
                                if schema.as_object().is_some_and(serde_json::Map::is_empty) {
                                    entries.insert(property_path, "schema".into());
                                } else {
                                    collect_shape_entries(schema, &property_path, entries);
                                }
                            }
                        }
                    }
                    "$defs" | "definitions" | "dependencies" | "dependentRequired"
                    | "dependentSchemas" | "patternProperties" => {
                        if let Some(children) = child.as_object() {
                            for (name, schema) in children {
                                collect_shape_entries(
                                    schema,
                                    &child_path(&child_path(path, key), name),
                                    entries,
                                );
                            }
                        }
                    }
                    _ => collect_shape_entries(child, &child_path(path, key), entries),
                }
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                collect_shape_entries(child, &child_path(path, &index.to_string()), entries);
            }
        }
        _ => {
            entries.insert(path.to_string(), literal_value(value));
        }
    }
}

fn collect_composition_branches(
    children: &[Value],
    path: &str,
    keyword: &str,
    entries: &mut BTreeMap<String, String>,
) {
    let identities = if keyword == "allOf" {
        Some(children.iter().map(branch_identity).collect::<Vec<_>>())
    } else {
        tagged_branch_identities(children)
    };
    let Some(identities) = identities else {
        for (index, child) in children.iter().enumerate() {
            let branch = child_path(&child_path(path, keyword), &index.to_string());
            entries.insert(child_path(&branch, "branch"), "schema".into());
            collect_shape_entries(child, &branch, entries);
        }
        return;
    };

    let mut counts = BTreeMap::new();
    for identity in &identities {
        *counts.entry(identity.clone()).or_insert(0_usize) += 1;
    }

    for (child, identity) in children.iter().zip(identities) {
        let identity = if counts[&identity] == 1 {
            identity
        } else {
            format!("{identity}:schema={}", schema_fingerprint(child))
        };
        let branch = child_path(&child_path(path, keyword), &identity);
        entries.insert(child_path(&branch, "branch"), "schema".into());
        collect_shape_entries(child, &branch, entries);
    }
}

fn tagged_branch_identities(children: &[Value]) -> Option<Vec<String>> {
    if let Some(literal_fields) = children
        .iter()
        .map(required_literal_fields)
        .collect::<Option<Vec<_>>>()
    {
        let common_fields = literal_fields
            .first()?
            .keys()
            .filter(|name| {
                literal_fields
                    .iter()
                    .all(|fields| fields.contains_key(*name))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !common_fields.is_empty() {
            let identities = literal_fields
                .iter()
                .map(|fields| {
                    common_fields
                        .iter()
                        .map(|name| format!("tag:{}={}", path_token(name), fields[name]))
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .collect::<Vec<_>>();
            let unique = identities.iter().collect::<std::collections::BTreeSet<_>>();
            if unique.len() == identities.len() {
                return Some(identities);
            }
        }
    }

    let domains = children
        .iter()
        .map(branch_type_domain)
        .collect::<Option<Vec<_>>>()?;
    for (index, left) in domains.iter().enumerate() {
        if domains
            .iter()
            .skip(index + 1)
            .any(|right| type_domains_overlap(left, right))
        {
            return None;
        }
    }
    Some(children.iter().map(branch_identity).collect())
}

fn branch_type_domain(value: &Value) -> Option<std::collections::BTreeSet<String>> {
    let object = value.as_object()?;
    let kinds = if let Some(instance_type) = object.get("type") {
        match instance_type {
            Value::String(kind) => vec![kind.clone()],
            Value::Array(kinds) => kinds
                .iter()
                .map(|kind| kind.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
            _ => return None,
        }
    } else if let Some(constant) = object.get("const") {
        vec![json_value_type(constant).to_string()]
    } else {
        let values = object.get("enum").and_then(Value::as_array)?;
        values
            .iter()
            .map(|value| json_value_type(value).to_string())
            .collect()
    };
    Some(kinds.into_iter().collect())
}

fn type_domains_overlap(
    left: &std::collections::BTreeSet<String>,
    right: &std::collections::BTreeSet<String>,
) -> bool {
    left.iter().any(|left| {
        right.iter().any(|right| {
            left == right
                || matches!(
                    (left.as_str(), right.as_str()),
                    ("integer", "number") | ("number", "integer")
                )
        })
    })
}

fn required_tag_identity(value: &Value) -> Option<String> {
    let fields = required_literal_fields(value)?;
    (!fields.is_empty()).then(|| {
        fields
            .into_iter()
            .map(|(name, literal)| format!("tag:{}={literal}", path_token(&name)))
            .collect::<Vec<_>>()
            .join("+")
    })
}

fn required_literal_fields(value: &Value) -> Option<BTreeMap<String, String>> {
    let object = value.as_object()?;
    let properties = object.get("properties")?.as_object()?;
    let required = object.get("required")?.as_array()?;
    let required = required
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    Some(
        properties
            .iter()
            .filter(|(name, _)| required.contains(&name.as_str()))
            .filter_map(|(name, schema)| {
                single_literal(schema)
                    .map(|literal| (name.clone(), path_token(&typed_literal(literal))))
            })
            .collect(),
    )
}

fn branch_identity(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return format!("schema={}", schema_fingerprint(value));
    };

    if let Some(identity) = required_tag_identity(value) {
        return identity;
    }

    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        return format!(
            "ref={}",
            path_token(reference.rsplit('/').next().unwrap_or(reference))
        );
    }
    if let Some(constant) = object.get("const") {
        return format!("const={}", path_token(&typed_literal(constant)));
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        let mut values = values.iter().map(typed_literal).collect::<Vec<_>>();
        values.sort_unstable();
        return format!("enum={}", path_token(&values.join(",")));
    }
    if let Some(instance_type) = object.get("type") {
        return format!("type={}", path_token(&type_list(instance_type)));
    }
    format!("schema={}", schema_fingerprint(value))
}

fn single_literal(schema: &Value) -> Option<&Value> {
    let object = schema.as_object()?;
    object.get("const").or_else(|| {
        let values = object.get("enum")?.as_array()?;
        (values.len() == 1).then(|| &values[0])
    })
}

fn typed_literal(value: &Value) -> String {
    format!("{}:{}", json_value_type(value), literal_value(value))
}

fn path_token(value: &str) -> String {
    let mut token = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'=' | b':' | b',') {
            token.push(char::from(byte));
        } else {
            token.push_str(&format!("%{byte:02X}"));
        }
    }
    token
}

fn schema_fingerprint(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_json(value, &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{digest:x}")
}

/// Hashes the complete schemars document for the author-time safety gate.
///
/// The human artifact above is filtered for diagnosis; this fingerprint is
/// deliberately unfiltered for safety. Object keys are sorted recursively and
/// arrays retain their schema order, then SHA-256 is applied to the compact JSON
/// bytes. This remains only as complete as schemars: a handwritten `Serialize`
/// implementation or a serde attribute schemars does not model is invisible.
#[cfg(test)]
fn unfiltered_schema_fingerprint(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical_unfiltered_json(value, &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    format!("{digest:x}")
}

#[cfg(test)]
fn write_canonical_unfiltered_json(value: &Value, output: &mut String) {
    match value {
        Value::Object(object) => {
            output.push('{');
            let mut fields = object.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| key.as_str());
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(&literal_value(&Value::String(key.clone())));
                output.push(':');
                write_canonical_unfiltered_json(value, output);
            }
            output.push('}');
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_canonical_unfiltered_json(value, output);
            }
            output.push(']');
        }
        _ => output.push_str(&literal_value(value)),
    }
}

#[cfg(test)]
fn render_registered_payload_fingerprints() -> String {
    let mut output = String::from(
        "# lash durable JSON payload schema fingerprints.\n\
#\n\
# Generated artifact -- never edit by hand. It is checked only at author time;\n\
# lash neither stores these hashes in a database nor checks them at store open.\n\
# Each SHA-256 covers complete schemars JSON canonicalized by recursively sorting\n\
# object keys, retaining array order, and emitting compact JSON. The filtered\n\
# schema-shape.txt remains the human-readable explanation of payload changes.\n\
# Regenerate with LASH_UPDATE_PAYLOAD_SCHEMA_FINGERPRINTS=1 cargo test -p\n\
# lash-postgres-store committed_fingerprints_match_every_registered_carrier.\n",
    );
    for ((backend, carrier, rust_type), fingerprint) in registered_payload_fingerprints() {
        output.push_str(&format!(
            "payload-fingerprint {backend} {carrier} {rust_type} sha256:{fingerprint}\n"
        ));
    }
    output
}

fn write_canonical_json(value: &Value, output: &mut String) {
    match value {
        Value::Object(object) => {
            output.push('{');
            let mut fields = object
                .iter()
                .filter(|(key, _)| !NON_DECODING_ANNOTATIONS.contains(&key.as_str()))
                .collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(key, _)| key.as_str());
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(&literal_value(&Value::String(key.clone())));
                output.push(':');
                if matches!(key.as_str(), "allOf" | "anyOf" | "oneOf") {
                    write_canonical_unordered_array(value, output);
                } else {
                    write_canonical_json(value, output);
                }
            }
            output.push('}');
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        _ => output.push_str(&literal_value(value)),
    }
}

fn write_canonical_unordered_array(value: &Value, output: &mut String) {
    let Some(values) = value.as_array() else {
        write_canonical_json(value, output);
        return;
    };
    let mut rendered = values
        .iter()
        .map(|value| {
            let mut item = String::new();
            write_canonical_json(value, &mut item);
            item
        })
        .collect::<Vec<_>>();
    rendered.sort_unstable();
    output.push('[');
    output.push_str(&rendered.join(","));
    output.push(']');
}

fn child_path(parent: &str, child: &str) -> String {
    let escaped = child.replace('~', "~0").replace('/', "~1");
    if parent.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{parent}/{escaped}")
    }
}

fn type_list(value: &Value) -> String {
    match value {
        Value::String(kind) => kind.clone(),
        Value::Array(kinds) => {
            let mut kinds = kinds
                .iter()
                .map(|kind| {
                    kind.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| json_value_type(kind).to_string())
                })
                .collect::<Vec<_>>();
            kinds.sort_unstable();
            kinds.dedup();
            kinds.join("|")
        }
        other => json_value_type(other).to_string(),
    }
}

fn value_types<'a>(values: impl Iterator<Item = &'a Value>) -> String {
    let mut types = values.map(json_value_type).collect::<Vec<_>>();
    types.sort_unstable();
    types.dedup();
    types.join("|")
}

fn literal_values(values: &[Value]) -> String {
    let mut values = values.iter().map(literal_value).collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    format!("[{}]", values.join(","))
}

fn literal_value(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON Schema literals are serializable")
}

fn json_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_meta_shape_comes_from_all_fields_even_without_a_sample() {
        let shape = PayloadShape::of::<lash_core::SessionMeta>();
        assert_eq!(shape.rust_type, "SessionMeta");
        assert_eq!(
            shape.entries.get("/properties/session_id/type"),
            Some(&"string".to_string())
        );
        assert_eq!(
            shape.entries.get("/properties/relation/$ref"),
            Some(&"SessionRelation".to_string())
        );
        assert!(shape.entries.contains_key("/required/session_id"));
        assert!(shape.entries.contains_key("/required/relation"));
        let registered = registered_payload_shapes()
            .remove(&("lash_session_meta".to_string(), "meta_json".to_string()))
            .expect("SessionMeta carrier is registered");
        assert_eq!(
            registered
                .entries
                .get("/persisted-by/sqlite/session_meta.relation_json/rust-type"),
            Some(&"SessionRelation".to_string())
        );
    }

    #[test]
    fn schema_projection_records_decode_literals_but_not_annotations() {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct Example {
            present: String,
            optional: Option<u64>,
        }

        let shape = PayloadShape::of::<Example>();
        let rendered = format!("{:?}", shape.entries);
        assert!(rendered.contains("present"));
        assert!(rendered.contains("optional"));
        assert_eq!(
            PayloadShape::of::<lash_core::SessionMeta>()
                .entries
                .get(
                    "/definitions/SessionRelation/oneOf/tag:kind=string:%22root%22/properties/kind/enum-values"
                ),
            Some(&r#"["root"]"#.to_string())
        );
        assert!(!rendered.contains("description"));
        assert!(!rendered.contains("default"));
    }

    #[test]
    fn unfiltered_fingerprint_is_key_order_independent_but_keeps_annotations() {
        let first: Value = serde_json::from_str(
            r#"{"title":"First","properties":{"b":{"type":"string"},"a":{}}}"#,
        )
        .expect("valid schema");
        let reordered: Value = serde_json::from_str(
            r#"{"properties":{"a":{},"b":{"type":"string"}},"title":"First"}"#,
        )
        .expect("valid schema");
        let annotation_changed: Value = serde_json::from_str(
            r#"{"properties":{"a":{},"b":{"type":"string"}},"title":"Second"}"#,
        )
        .expect("valid schema");

        assert_eq!(
            unfiltered_schema_fingerprint(&first),
            unfiltered_schema_fingerprint(&reordered)
        );
        assert_ne!(
            unfiltered_schema_fingerprint(&first),
            unfiltered_schema_fingerprint(&annotation_changed)
        );
    }

    #[test]
    fn committed_fingerprints_match_every_registered_carrier() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("payload-schema-fingerprints.txt");
        let committed = std::fs::read_to_string(&path).expect("read fingerprint artifact");
        let generated = render_registered_payload_fingerprints();
        if committed != generated
            && std::env::var("LASH_UPDATE_PAYLOAD_SCHEMA_FINGERPRINTS").as_deref() == Ok("1")
        {
            std::fs::write(&path, &generated).expect("rewrite fingerprint artifact");
            panic!(
                "regenerated {} -- rerun the test to confirm",
                path.display()
            );
        }
        assert_eq!(
            committed,
            generated,
            "{} must be regenerated whenever a registered schemars schema changes",
            path.display()
        );
    }

    #[test]
    fn numeric_width_is_part_of_decode_shape() {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct Wide {
            sequence: u64,
        }
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct Narrow {
            sequence: u32,
        }

        let wide = PayloadShape::of::<Wide>();
        let narrow = PayloadShape::of::<Narrow>();
        assert_ne!(wide.entries, narrow.entries);
        assert_eq!(
            wide.entries.get("/properties/sequence/format"),
            Some(&r#""uint64""#.to_string())
        );
        assert_eq!(
            narrow.entries.get("/properties/sequence/format"),
            Some(&r#""uint32""#.to_string())
        );
    }

    #[test]
    fn nullable_type_union_records_its_member_types() {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct Example {
            sequence: Option<u64>,
        }

        assert_eq!(
            PayloadShape::of::<Example>()
                .entries
                .get("/properties/sequence/type"),
            Some(&"integer|null".to_string())
        );
    }

    #[test]
    fn empty_property_schema_still_records_the_property_name() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "opaque": {}
            }
        });
        let mut entries = BTreeMap::new();

        collect_shape_entries(&schema, "", &mut entries);

        assert_eq!(
            entries.get("/properties/opaque"),
            Some(&"schema".to_string())
        );
    }

    #[test]
    fn tagged_enum_variant_order_is_not_part_of_decode_shape() {
        #[derive(JsonSchema)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Before {
            Turn { turn_id: String },
            Effect { effect_id: String },
        }
        #[derive(JsonSchema)]
        #[serde(tag = "type", rename_all = "snake_case")]
        #[allow(dead_code)]
        enum After {
            Effect { effect_id: String },
            Turn { turn_id: String },
        }

        assert_eq!(
            PayloadShape::of::<Before>().entries,
            PayloadShape::of::<After>().entries
        );
    }

    #[test]
    fn distinct_literal_fields_do_not_form_a_shared_discriminator() {
        let left = serde_json::json!({
            "type": "object",
            "properties": { "left_kind": { "const": "left" } },
            "required": ["left_kind"]
        });
        let right = serde_json::json!({
            "type": "object",
            "properties": { "right_kind": { "const": "right" } },
            "required": ["right_kind"]
        });
        let before = serde_json::json!({ "oneOf": [left.clone(), right.clone()] });
        let after = serde_json::json!({ "oneOf": [right, left] });
        let mut before_entries = BTreeMap::new();
        let mut after_entries = BTreeMap::new();

        collect_shape_entries(&before, "", &mut before_entries);
        collect_shape_entries(&after, "", &mut after_entries);

        assert_ne!(before_entries, after_entries);
        assert!(before_entries.contains_key("/oneOf/0/properties/left_kind/const-value"));
        assert!(after_entries.contains_key("/oneOf/0/properties/right_kind/const-value"));
    }

    #[test]
    fn overlapping_untagged_variant_order_remains_part_of_decode_shape() {
        #[derive(JsonSchema)]
        #[serde(untagged)]
        #[allow(dead_code)]
        enum Before {
            Broad { value: String },
            Narrow { value: String, qualifier: String },
        }
        #[derive(JsonSchema)]
        #[serde(untagged)]
        #[allow(dead_code)]
        enum After {
            Narrow { value: String, qualifier: String },
            Broad { value: String },
        }

        assert_ne!(
            PayloadShape::of::<Before>().entries,
            PayloadShape::of::<After>().entries
        );
    }

    #[test]
    fn disjoint_externally_tagged_variant_order_is_not_part_of_decode_shape() {
        #[derive(JsonSchema)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Before {
            All,
            Only(Vec<String>),
        }
        #[derive(JsonSchema)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum After {
            Only(Vec<String>),
            All,
        }

        assert_eq!(
            PayloadShape::of::<Before>().entries,
            PayloadShape::of::<After>().entries
        );
    }
}
