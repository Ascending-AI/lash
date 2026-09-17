use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ast::{ProcessParam, ProcessSignature, ProcessSignatureError, ProcessType};
use crate::{TypeExpr, TypeField};

const MAX_SCHEMA_DEPTH: usize = 32;

/// The JSON Schema keyword that carries the types JSON Schema cannot say.
///
/// JSON Schema has no vocabulary for a callable process or for a trigger
/// handle, so a tool contract that traffics in them used to lose them at the
/// boundary: the exporter erased both to `{}` and the importer widened the
/// `{}` back to [`TypeExpr::Any`]. `x-lash` is the one place those types are
/// spelled, and it is an extension keyword, so a contract that does not carry
/// it reads exactly as it did before.
pub const X_LASH_KEYWORD: &str = "x-lash";

/// The lash-only half of a type, as it rides inside a JSON Schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum XLashType {
    /// A process with an authoritative call signature.
    Process { signature: XLashSignature },
    /// A process the host can only describe as callable.
    ProcessUnknown,
    /// A trigger handle over the payload the trigger delivers.
    Handle { payload: Box<Value> },
}

/// An ordered process-call signature in schema form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XLashSignature {
    pub params: Vec<XLashParam>,
    pub output: Box<Value>,
}

/// One named process parameter in schema form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XLashParam {
    pub name: String,
    pub schema: Value,
}

/// Why a host-declared schema cannot be read as a lash type.
///
/// Widening is still the rule for everything JSON Schema says and lash cannot
/// hold. These are the cases where the schema claims to speak lash and gets it
/// wrong, which is a defect in the contract rather than an expressiveness gap,
/// so the whole contract is refused instead of silently losing the type.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum JsonSchemaError {
    #[error("`{keyword}` at `{path}` is not a valid lash type declaration: {reason}")]
    MalformedLashType {
        keyword: &'static str,
        path: String,
        reason: String,
    },
    #[error("`{keyword}` at `{path}` declares an invalid process signature: {source}")]
    InvalidProcessSignature {
        keyword: &'static str,
        path: String,
        #[source]
        source: ProcessSignatureError,
    },
}

/// Conversion from JSON Schema into Lashlang's value-type vocabulary.
///
/// JSON Schema is more expressive than [`TypeExpr`]. Unsupported or ambiguous
/// constructs deliberately widen to [`TypeExpr::Dict`] or [`TypeExpr::Any`]
/// instead of rejecting a host-declared capability. The one refusal is a
/// malformed [`X_LASH_KEYWORD`]: that keyword exists only because the host
/// meant a specific lash type, so guessing at a broken one would hide the
/// mistake behind a widened `Any`.
pub fn json_schema_to_type_expr(schema: &Value) -> Result<TypeExpr, JsonSchemaError> {
    SchemaImporter { root: schema }.import(schema, 0, String::from("#"))
}

/// Conversion from Lashlang's value-type vocabulary back into JSON Schema.
///
/// This is the counterpart authority to [`json_schema_to_type_expr`] for tool
/// contracts: types JSON Schema can say are said in JSON Schema, and the rest
/// ride in [`X_LASH_KEYWORD`].
pub fn type_expr_to_json_schema(ty: &TypeExpr) -> Value {
    match ty {
        TypeExpr::Any | TypeExpr::Dict => Value::Object(Map::new()),
        TypeExpr::Str => serde_json::json!({ "type": "string" }),
        TypeExpr::Int => serde_json::json!({ "type": "integer" }),
        TypeExpr::Float => serde_json::json!({ "type": "number" }),
        TypeExpr::Bool => serde_json::json!({ "type": "boolean" }),
        TypeExpr::Null => serde_json::json!({ "type": "null" }),
        TypeExpr::Ref(name) => serde_json::json!({ "$ref": name.as_str() }),
        TypeExpr::Enum(values) => serde_json::json!({
            "enum": values.iter().map(|value| value.as_str()).collect::<Vec<_>>()
        }),
        TypeExpr::List(item) => serde_json::json!({
            "type": "array",
            "items": type_expr_to_json_schema(item),
        }),
        TypeExpr::Object(fields) => {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for field in fields {
                properties.insert(field.name.to_string(), type_expr_to_json_schema(&field.ty));
                if !field.optional {
                    required.push(Value::String(field.name.to_string()));
                }
            }
            let mut schema = Map::new();
            schema.insert("type".to_string(), Value::String("object".to_string()));
            schema.insert("properties".to_string(), Value::Object(properties));
            if !required.is_empty() {
                schema.insert("required".to_string(), Value::Array(required));
            }
            schema.insert("additionalProperties".to_string(), Value::Bool(true));
            Value::Object(schema)
        }
        TypeExpr::Union(variants) => serde_json::json!({
            "anyOf": variants.iter().map(type_expr_to_json_schema).collect::<Vec<_>>()
        }),
        TypeExpr::Process(process) => {
            let declaration = match process.as_signature() {
                Some(signature) => XLashType::Process {
                    signature: XLashSignature {
                        params: signature
                            .params()
                            .iter()
                            .map(|param| XLashParam {
                                name: param.name.to_string(),
                                schema: type_expr_to_json_schema(&param.ty),
                            })
                            .collect(),
                        output: Box::new(type_expr_to_json_schema(signature.output())),
                    },
                },
                None => XLashType::ProcessUnknown,
            };
            lash_type_schema(&declaration)
        }
        TypeExpr::TriggerHandle(payload) => lash_type_schema(&XLashType::Handle {
            payload: Box::new(type_expr_to_json_schema(payload)),
        }),
    }
}

#[expect(
    clippy::expect_used,
    reason = "a lash type declaration is a struct of strings and types and always serializes to JSON, per the message"
)]
fn lash_type_schema(declaration: &XLashType) -> Value {
    let mut schema = Map::new();
    schema.insert(
        X_LASH_KEYWORD.to_string(),
        serde_json::to_value(declaration).expect("lash type declaration serializes"),
    );
    Value::Object(schema)
}

struct SchemaImporter<'a> {
    root: &'a Value,
}

impl SchemaImporter<'_> {
    fn import(
        &self,
        schema: &Value,
        depth: usize,
        path: String,
    ) -> Result<TypeExpr, JsonSchemaError> {
        if depth >= MAX_SCHEMA_DEPTH {
            return Ok(TypeExpr::Any);
        }

        let Some(schema) = schema.as_object() else {
            return Ok(TypeExpr::Any);
        };
        if schema.is_empty() {
            return Ok(TypeExpr::Any);
        }

        if let Some(declaration) = schema.get(X_LASH_KEYWORD) {
            return self.import_lash_type(declaration, depth, &path);
        }

        if let Some(all_of) = schema.get("allOf") {
            let Some(branches) = all_of.as_array() else {
                return Ok(TypeExpr::Any);
            };
            return match branches.as_slice() {
                [branch] => self.import(branch, depth + 1, child(&path, "allOf/0")),
                _ => Ok(TypeExpr::Any),
            };
        }

        if let Some(reference) = schema.get("$ref") {
            let Some(reference) = reference.as_str() else {
                return Ok(TypeExpr::Any);
            };
            // A `#`-pointer names a shape inside this document and is inlined.
            // Anything else that reads as a plain name is a named data type the
            // linker resolves; a URL or any other spelling still widens.
            let Some(pointer) = reference.strip_prefix('#') else {
                return Ok(if is_named_type_reference(reference) {
                    TypeExpr::Ref(reference.into())
                } else {
                    TypeExpr::Any
                });
            };
            return match self.root.pointer(pointer) {
                Some(target) => self.import(target, depth + 1, child(&path, reference)),
                None => Ok(TypeExpr::Any),
            };
        }

        let has_any_of = schema.contains_key("anyOf");
        let has_one_of = schema.contains_key("oneOf");
        if has_any_of && has_one_of {
            return Ok(TypeExpr::Any);
        }
        if let Some(branches) = schema.get("anyOf") {
            return self.import_union(branches, depth, &child(&path, "anyOf"));
        }
        if let Some(branches) = schema.get("oneOf") {
            return self.import_union(branches, depth, &child(&path, "oneOf"));
        }

        if let Some(values) = schema.get("enum") {
            return Ok(import_enum(values));
        }
        if let Some(value) = schema.get("const") {
            return Ok(import_enum(&Value::Array(vec![value.clone()])));
        }

        match schema.get("type") {
            Some(Value::String(kind)) => self.import_type(kind, schema, depth, &path),
            Some(Value::Array(kinds)) => {
                let mut variants = Vec::new();
                for kind in kinds {
                    variants.push(match kind.as_str() {
                        Some(kind) => self.import_type(kind, schema, depth, &path)?,
                        None => TypeExpr::Any,
                    });
                }
                Ok(union_type(variants))
            }
            Some(_) => Ok(TypeExpr::Any),
            None if has_object_keywords(schema) => self.import_object(schema, depth, &path),
            None if has_array_keywords(schema) => self.import_array(schema, depth, &path),
            None => Ok(TypeExpr::Any),
        }
    }

    fn import_lash_type(
        &self,
        declaration: &Value,
        depth: usize,
        path: &str,
    ) -> Result<TypeExpr, JsonSchemaError> {
        let declaration: XLashType =
            serde_json::from_value(declaration.clone()).map_err(|error| {
                JsonSchemaError::MalformedLashType {
                    keyword: X_LASH_KEYWORD,
                    path: path.to_string(),
                    reason: error.to_string(),
                }
            })?;
        match declaration {
            XLashType::ProcessUnknown => Ok(TypeExpr::Process(ProcessType::unknown())),
            XLashType::Handle { payload } => Ok(TypeExpr::TriggerHandle(Box::new(self.import(
                &payload,
                depth + 1,
                child(path, "x-lash/payload"),
            )?))),
            XLashType::Process { signature } => {
                let mut params = Vec::new();
                for (index, param) in signature.params.iter().enumerate() {
                    params.push(ProcessParam {
                        name: param.name.as_str().into(),
                        ty: self.import(
                            &param.schema,
                            depth + 1,
                            child(path, &format!("x-lash/signature/params/{index}")),
                        )?,
                    });
                }
                let output = self.import(
                    &signature.output,
                    depth + 1,
                    child(path, "x-lash/signature/output"),
                )?;
                let signature = ProcessSignature::try_new(params, output).map_err(|source| {
                    JsonSchemaError::InvalidProcessSignature {
                        keyword: X_LASH_KEYWORD,
                        path: path.to_string(),
                        source,
                    }
                })?;
                Ok(TypeExpr::Process(ProcessType::known(signature)))
            }
        }
    }

    fn import_union(
        &self,
        branches: &Value,
        depth: usize,
        path: &str,
    ) -> Result<TypeExpr, JsonSchemaError> {
        let Some(branches) = branches.as_array() else {
            return Ok(TypeExpr::Any);
        };
        let mut variants = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            variants.push(self.import(branch, depth + 1, child(path, &index.to_string()))?);
        }
        Ok(union_type(variants))
    }

    fn import_type(
        &self,
        kind: &str,
        schema: &Map<String, Value>,
        depth: usize,
        path: &str,
    ) -> Result<TypeExpr, JsonSchemaError> {
        match kind {
            "string" => Ok(TypeExpr::Str),
            "integer" => Ok(TypeExpr::Int),
            "number" => Ok(TypeExpr::Float),
            "boolean" => Ok(TypeExpr::Bool),
            "null" => Ok(TypeExpr::Null),
            "object" => self.import_object(schema, depth, path),
            "array" => self.import_array(schema, depth, path),
            _ => Ok(TypeExpr::Any),
        }
    }

    fn import_object(
        &self,
        schema: &Map<String, Value>,
        depth: usize,
        path: &str,
    ) -> Result<TypeExpr, JsonSchemaError> {
        if schema.contains_key("patternProperties")
            || !matches!(schema.get("additionalProperties"), Some(Value::Bool(false)))
        {
            return Ok(TypeExpr::Dict);
        }

        let properties = match schema.get("properties") {
            Some(Value::Object(properties)) => properties,
            Some(_) => return Ok(TypeExpr::Dict),
            None => return Ok(TypeExpr::Object(Vec::new())),
        };
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|required| {
                required
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut fields = Vec::new();
        for (name, property) in properties {
            fields.push(TypeField {
                name: name.as_str().into(),
                ty: self.import(
                    property,
                    depth + 1,
                    child(path, &format!("properties/{name}")),
                )?,
                optional: !required.contains(name.as_str()),
            });
        }
        Ok(TypeExpr::Object(fields))
    }

    fn import_array(
        &self,
        schema: &Map<String, Value>,
        depth: usize,
        path: &str,
    ) -> Result<TypeExpr, JsonSchemaError> {
        if schema.contains_key("prefixItems") || schema.get("items").is_some_and(Value::is_array) {
            return Ok(TypeExpr::List(Box::new(TypeExpr::Any)));
        }
        let item = match schema.get("items") {
            Some(item) => self.import(item, depth + 1, child(path, "items"))?,
            None => TypeExpr::Any,
        };
        Ok(TypeExpr::List(Box::new(item)))
    }
}

fn child(path: &str, segment: &str) -> String {
    format!("{path}/{segment}")
}

/// Whether a non-pointer `$ref` reads as a lash named data type.
///
/// Named data types are dotted identifiers (`lash.TriggerRegistration`). A URL
/// or a relative file reference is a JSON Schema construct lash cannot resolve,
/// and keeps widening rather than inventing a name the linker would reject.
fn is_named_type_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference
            .split('.')
            .all(|segment| !segment.is_empty() && segment.chars().all(is_name_char))
        && reference
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
}

fn is_name_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn has_object_keywords(schema: &Map<String, Value>) -> bool {
    [
        "properties",
        "required",
        "additionalProperties",
        "patternProperties",
    ]
    .iter()
    .any(|key| schema.contains_key(*key))
}

fn has_array_keywords(schema: &Map<String, Value>) -> bool {
    schema.contains_key("items") || schema.contains_key("prefixItems")
}

fn import_enum(values: &Value) -> TypeExpr {
    let Some(values) = values.as_array().filter(|values| !values.is_empty()) else {
        return TypeExpr::Any;
    };
    let mut strings = Vec::new();
    let mut has_null = false;
    for value in values {
        match value {
            Value::String(value) => strings.push(value.as_str().into()),
            Value::Null => has_null = true,
            _ => return TypeExpr::Any,
        }
    }

    let mut variants = Vec::new();
    if !strings.is_empty() {
        variants.push(TypeExpr::Enum(strings));
    }
    if has_null {
        variants.push(TypeExpr::Null);
    }
    union_type(variants)
}

fn union_type(variants: Vec<TypeExpr>) -> TypeExpr {
    // `anyOf` containing `Any` widens to `Any` outright; only top-level
    // variants are checked, matching the flatten-below behavior a nested
    // union's `Any` has (it stays a member).
    if variants
        .iter()
        .any(|variant| matches!(variant, TypeExpr::Any))
    {
        return TypeExpr::Any;
    }
    match crate::UnionMembers::deduplicated(variants) {
        Ok(members) => TypeExpr::Union(members),
        // Unlike the linker's empty-list element union, this is the
        // JSON-Schema importer domain: an empty `anyOf` widens to `Any`.
        // The divergence is intentional and pinned separately (FIG-1878).
        Err(unique) => unique.into_iter().next().unwrap_or(TypeExpr::Any),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn import_schema(schema: &Value) -> TypeExpr {
        super::json_schema_to_type_expr(schema).expect("schema imports")
    }

    fn field(name: &str, ty: TypeExpr, optional: bool) -> TypeField {
        TypeField {
            name: name.into(),
            ty,
            optional,
        }
    }

    #[test]
    fn imports_empty_schema_as_any() {
        assert_eq!(import_schema(&json!({})), TypeExpr::Any);
        assert_eq!(import_schema(&Value::Null), TypeExpr::Any);
        assert_eq!(import_schema(&Value::Bool(true)), TypeExpr::Any);
    }

    #[test]
    fn imports_primitive_types_and_drops_refinements() {
        for (schema, expected) in [
            (
                json!({ "type": "string", "minLength": 2, "maxLength": 8, "pattern": "x", "format": "email" }),
                TypeExpr::Str,
            ),
            (
                json!({ "type": "integer", "minimum": 1, "maximum": 5 }),
                TypeExpr::Int,
            ),
            (
                json!({ "type": "number", "exclusiveMinimum": 0 }),
                TypeExpr::Float,
            ),
            (json!({ "type": "boolean" }), TypeExpr::Bool),
            (json!({ "type": "null" }), TypeExpr::Null),
        ] {
            assert_eq!(import_schema(&schema), expected);
        }
    }

    #[test]
    fn imports_closed_object_properties_and_required_fields() {
        let schema = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
                "name": { "type": "string" }
            },
            "required": ["name"],
            "additionalProperties": false
        });
        assert_eq!(
            import_schema(&schema),
            TypeExpr::Object(vec![
                field("count", TypeExpr::Int, true),
                field("name", TypeExpr::Str, false),
            ])
        );
    }

    #[test]
    fn imports_arrays_and_missing_items() {
        assert_eq!(
            import_schema(&json!({ "type": "array", "items": { "type": "string" } })),
            TypeExpr::List(Box::new(TypeExpr::Str))
        );
        assert_eq!(
            import_schema(&json!({ "type": "array" })),
            TypeExpr::List(Box::new(TypeExpr::Any))
        );
    }

    #[test]
    fn imports_string_enums_and_representable_singleton_unions() {
        assert_eq!(
            import_schema(&json!({ "enum": ["fast", "safe"] })),
            TypeExpr::Enum(vec!["fast".into(), "safe".into()])
        );
        assert_eq!(
            import_schema(&json!({ "enum": ["ready", null] })),
            TypeExpr::union(vec![TypeExpr::Enum(vec!["ready".into()]), TypeExpr::Null])
        );
    }

    #[test]
    fn imports_nullable_type_arrays_and_union_keywords() {
        assert_eq!(
            import_schema(&json!({ "type": ["string", "null"] })),
            TypeExpr::union(vec![TypeExpr::Str, TypeExpr::Null])
        );
        for keyword in ["anyOf", "oneOf"] {
            let schema = json!({ (keyword): [{ "type": "integer" }, { "type": "null" }] });
            assert_eq!(
                import_schema(&schema),
                TypeExpr::union(vec![TypeExpr::Int, TypeExpr::Null])
            );
        }
    }

    #[test]
    fn open_and_patterned_objects_degrade_to_dict() {
        for schema in [
            json!({ "type": "object", "properties": { "name": { "type": "string" } } }),
            json!({ "type": "object", "additionalProperties": true }),
            json!({ "type": "object", "additionalProperties": { "type": "string" } }),
            json!({ "type": "object", "patternProperties": { "^x": { "type": "integer" } }, "additionalProperties": false }),
        ] {
            assert_eq!(import_schema(&schema), TypeExpr::Dict);
        }
    }

    #[test]
    fn tuple_arrays_degrade_to_list_any() {
        for schema in [
            json!({ "type": "array", "prefixItems": [{ "type": "string" }] }),
            json!({ "type": "array", "items": [{ "type": "string" }, { "type": "integer" }] }),
        ] {
            assert_eq!(
                import_schema(&schema),
                TypeExpr::List(Box::new(TypeExpr::Any))
            );
        }
    }

    #[test]
    fn intersections_and_non_string_enums_degrade_to_any() {
        for schema in [
            json!({ "allOf": [{ "type": "string" }, { "maxLength": 8 }] }),
            json!({ "enum": [1, 2] }),
            json!({ "enum": ["one", 2] }),
        ] {
            assert_eq!(import_schema(&schema), TypeExpr::Any);
        }
        assert_eq!(
            import_schema(&json!({ "allOf": [{ "type": "string", "pattern": "x" }] })),
            TypeExpr::Str
        );
    }

    #[test]
    fn local_refs_resolve_and_recursive_refs_hit_the_depth_cap() {
        let direct = json!({
            "$defs": { "Name": { "type": "string" } },
            "$ref": "#/$defs/Name"
        });
        assert_eq!(import_schema(&direct), TypeExpr::Str);

        let recursive = json!({
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "next": { "$ref": "#/$defs/Node" } },
                    "required": ["next"],
                    "additionalProperties": false
                }
            },
            "$ref": "#/$defs/Node"
        });
        let mut imported = import_schema(&recursive);
        for _ in 0..MAX_SCHEMA_DEPTH {
            match imported {
                TypeExpr::Object(mut fields) => imported = fields.remove(0).ty,
                TypeExpr::Any => return,
                other => panic!("recursive ref widened to unexpected type: {other:?}"),
            }
        }
        panic!("recursive ref did not widen to any at the depth cap");
    }

    #[test]
    fn malformed_and_external_refs_degrade_without_errors() {
        for schema in [
            json!({ "$ref": "https://example.com/schema.json" }),
            json!({ "$ref": "#/$defs/Missing" }),
            json!({ "anyOf": "not-an-array" }),
            json!({ "type": "unknown" }),
        ] {
            assert_eq!(import_schema(&schema), TypeExpr::Any);
        }
    }

    #[test]
    fn empty_any_of_imports_as_any() {
        assert_eq!(import_schema(&json!({ "anyOf": [] })), TypeExpr::Any);
    }

    #[test]
    fn imports_the_lash_only_types_from_the_x_lash_keyword() {
        assert_eq!(
            import_schema(&json!({ "x-lash": { "kind": "process_unknown" } })),
            TypeExpr::Process(ProcessType::unknown())
        );
        assert_eq!(
            import_schema(&json!({
                "x-lash": { "kind": "handle", "payload": { "type": "string" } }
            })),
            TypeExpr::TriggerHandle(Box::new(TypeExpr::Str))
        );
        assert_eq!(
            import_schema(&json!({
                "x-lash": {
                    "kind": "process",
                    "signature": {
                        "params": [{ "name": "event", "schema": { "type": "string" } }],
                        "output": { "type": "integer" }
                    }
                }
            })),
            TypeExpr::Process(ProcessType::known(
                ProcessSignature::try_new(
                    vec![ProcessParam {
                        name: "event".into(),
                        ty: TypeExpr::Str,
                    }],
                    TypeExpr::Int,
                )
                .expect("signature")
            ))
        );
    }

    #[test]
    fn a_malformed_x_lash_keyword_refuses_the_whole_schema() {
        let unknown_kind = json_schema_to_type_expr(&json!({
            "type": "object",
            "properties": { "target": { "x-lash": { "kind": "proc" } } },
            "required": ["target"],
            "additionalProperties": false
        }))
        .expect_err("an unknown x-lash kind is refused");
        assert!(matches!(
            unknown_kind,
            JsonSchemaError::MalformedLashType { keyword: X_LASH_KEYWORD, ref path, .. }
                if path == "#/properties/target"
        ));

        assert!(matches!(
            json_schema_to_type_expr(&json!({ "x-lash": { "kind": "handle" } }))
                .expect_err("a handle without a payload is refused"),
            JsonSchemaError::MalformedLashType { .. }
        ));
        assert!(matches!(
            json_schema_to_type_expr(&json!({ "x-lash": "process" }))
                .expect_err("a non-object x-lash is refused"),
            JsonSchemaError::MalformedLashType { .. }
        ));
    }

    #[test]
    fn a_process_signature_the_language_rejects_refuses_the_schema() {
        let duplicate = json_schema_to_type_expr(&json!({
            "x-lash": {
                "kind": "process",
                "signature": {
                    "params": [
                        { "name": "event", "schema": {} },
                        { "name": "event", "schema": {} }
                    ],
                    "output": {}
                }
            }
        }))
        .expect_err("a duplicate parameter is refused");
        assert!(matches!(
            duplicate,
            JsonSchemaError::InvalidProcessSignature {
                source: ProcessSignatureError::DuplicateParameter { .. },
                ..
            }
        ));

        assert!(matches!(
            json_schema_to_type_expr(&json!({
                "x-lash": {
                    "kind": "process",
                    "signature": {
                        "params": [{ "name": "not an identifier", "schema": {} }],
                        "output": {}
                    }
                }
            }))
            .expect_err("an unspellable parameter name is refused"),
            JsonSchemaError::InvalidProcessSignature {
                source: ProcessSignatureError::InvalidParameterName { .. },
                ..
            }
        ));
    }

    #[test]
    fn named_data_types_ride_in_a_plain_ref() {
        assert_eq!(
            type_expr_to_json_schema(&TypeExpr::Ref("lash.TriggerRegistration".into())),
            json!({ "$ref": "lash.TriggerRegistration" })
        );
        assert_eq!(
            import_schema(&json!({ "$ref": "lash.TriggerRegistration" })),
            TypeExpr::Ref("lash.TriggerRegistration".into())
        );
    }

    /// The subset of [`TypeExpr`] JSON Schema plus `x-lash` says exactly.
    ///
    /// The excluded variants are excluded for named reasons, not by oversight:
    /// `Any` and `Dict` both export to `{}`, `Object` exports
    /// `additionalProperties: true` while the importer only reads a closed
    /// object back as `Object`, and `Union` is normalised on import (flattened
    /// and deduplicated), so none of the three is a fixed point of the pair.
    fn lash_type_strategy() -> impl Strategy<Value = TypeExpr> {
        let leaf = prop_oneof![
            Just(TypeExpr::Str),
            Just(TypeExpr::Int),
            Just(TypeExpr::Float),
            Just(TypeExpr::Bool),
            Just(TypeExpr::Null),
            proptest::collection::vec("[a-z][a-z0-9_]{0,6}", 1..4).prop_map(|values| {
                TypeExpr::Enum(values.iter().map(|value| value.as_str().into()).collect())
            }),
            "[a-z][a-z0-9_]{0,6}(\\.[a-zA-Z][a-zA-Z0-9_]{0,6}){0,2}"
                .prop_map(|name| TypeExpr::Ref(name.as_str().into())),
            Just(TypeExpr::Process(ProcessType::unknown())),
        ];
        leaf.prop_recursive(4, 16, 3, |inner| {
            prop_oneof![
                inner
                    .clone()
                    .prop_map(|item| TypeExpr::List(Box::new(item))),
                inner
                    .clone()
                    .prop_map(|payload| TypeExpr::TriggerHandle(Box::new(payload))),
                proptest::collection::vec(("[a-z][a-z0-9_]{0,6}", inner), 0..3).prop_filter_map(
                    "a parameter name the language cannot spell",
                    |params| {
                        let mut seen = BTreeSet::new();
                        let params = params
                            .into_iter()
                            .filter(|(name, _)| seen.insert(name.clone()))
                            .map(|(name, ty)| ProcessParam {
                                name: name.as_str().into(),
                                ty,
                            })
                            .collect::<Vec<_>>();
                        let output = params
                            .first()
                            .map_or(TypeExpr::Null, |param| param.ty.clone());
                        // The generated name space is wider than the language's:
                        // `or` matches the pattern and is a keyword, so the
                        // signature cannot be built. Asking `try_new` rather than
                        // re-deriving the rule here keeps this generator honest if
                        // the rule ever moves, and the discard is rare enough to
                        // leave proptest's budget alone.
                        ProcessSignature::try_new(params, output)
                            .ok()
                            .map(|signature| TypeExpr::Process(ProcessType::known(signature)))
                    }
                ),
            ]
        })
    }

    proptest! {
        #[test]
        fn import_of_export_is_the_identity_on_the_lash_type_subset(
            ty in lash_type_strategy()
        ) {
            let schema = type_expr_to_json_schema(&ty);
            prop_assert_eq!(
                json_schema_to_type_expr(&schema).expect("an exported schema imports"),
                ty
            );
        }
    }
}
