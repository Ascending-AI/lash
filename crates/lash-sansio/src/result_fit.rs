//! A tool's value admitted against the schema its contract declares. The
//! declared schema is the contract: the value must be an instance of it
//! ([`JsonSchema::validate`]), so maps, open objects and numeric bounds are
//! checked. The schema's kernel result type
//! ([`lash_kernel_check::result_type_of_schema`]) is the program's typed
//! view of the value, and the value must also decode by it (`K-EFF-007`):
//! a value this admits is one the cell's `perform` of the tool decodes.

use lash_kernel_doc::{Datum, Decoder, KernelDataLimits, NumberPolicy, NumberToken, Type};
use serde_json::Value;

use crate::{JsonSchema, ValueMismatch};

/// Whether `value` is admitted by `schema`: an instance of it, and
/// decodable by its kernel result type.
///
/// # Errors
///
/// [`ValueMismatch`]: the JSON pointer of the part of `value` that does not
/// fit, and why.
pub(crate) fn admit_value(
    schema: &JsonSchema,
    value: impl FnOnce() -> Value,
) -> Result<(), ValueMismatch> {
    // The unconstrained schema admits every value: skip building it.
    if schema
        .as_value()
        .as_object()
        .is_some_and(serde_json::Map::is_empty)
    {
        return Ok(());
    }
    let value = value();
    schema.validate(&value)?;
    let ty = lash_kernel_check::result_type_of_schema(schema.as_value());
    // Every value a cell can receive decodes as `Any`.
    if ty == Type::Any {
        return Ok(());
    }
    let depth = KernelDataLimits::standard().value_depth();
    let datum = datum_of(&value, depth).ok_or_else(|| ValueMismatch {
        instance_path: String::new(),
        message: format!("it exceeds the configured {depth}-level value depth"),
    })?;
    let decoder = Decoder {
        max_depth: depth,
        // The policy that decodes the most numbers: a cell's own policy
        // admits no number this one refuses.
        policy: NumberPolicy::BySpelling,
        declared: &|_| false,
    };
    decoder
        .decode(&datum, &ty)
        .map(drop)
        .map_err(|mismatch| ValueMismatch {
            instance_path: mismatch
                .path
                .iter()
                .map(|field| format!("/{}", field.replace('~', "~0").replace('/', "~1")))
                .collect(),
            message: mismatch.problem,
        })
}

/// The datum a cell reads `value` as, as its JSON reader builds it; `None`
/// past `budget` levels.
fn datum_of(value: &Value, budget: usize) -> Option<Datum> {
    let budget = budget.checked_sub(1)?;
    Some(match value {
        Value::Null => Datum::Null,
        Value::Bool(flag) => Datum::Bool(*flag),
        Value::Number(number) => Datum::Number(NumberToken::new(number.to_string()).ok()?),
        Value::String(text) => Datum::Text(text.clone()),
        Value::Array(items) => Datum::List(
            items
                .iter()
                .map(|item| datum_of(item, budget))
                .collect::<Option<_>>()?,
        ),
        Value::Object(fields) => Datum::Record(
            fields
                .iter()
                .map(|(name, value)| Some((name.clone(), datum_of(value, budget)?)))
                .collect::<Option<_>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{DeclarationRefusal, JsonSchema, ToolCallOutput, ToolDeclaration, ValueMismatch};

    /// How a tool's success of `value` settles under `schema`.
    fn settle(schema: Value, value: Value) -> Result<(), ValueMismatch> {
        let schema = JsonSchema::admit(schema).expect("an admissible schema");
        match ToolCallOutput::success(value).settled(&ToolDeclaration::default(), &schema) {
            Ok(_) => Ok(()),
            Err(DeclarationRefusal::OutputValueMismatch { mismatch, .. }) => Err(*mismatch),
            Err(refusal) => panic!("an output mismatch, not {refusal:?}"),
        }
    }

    /// FIG-6330: a numeric bound the kernel type cannot state is the
    /// contract all the same.
    #[test]
    fn a_number_above_its_declared_maximum_is_refused() {
        let schema =
            json!({"type": "object", "properties": {"noul": {"type": "number", "maximum": 1}}});
        let mismatch = settle(schema, json!({"noul": 1.5})).expect_err("1.5 exceeds the maximum");
        assert_eq!(mismatch.instance_path, "/noul", "{mismatch:?}");
    }

    /// FIG-6330: an open object schema still requires its `required` fields.
    #[test]
    fn an_open_object_missing_a_required_field_is_refused() {
        let schema = json!({
            "type": "object",
            "required": ["diagnostics", "results"],
            "properties": {"diagnostics": {"type": "object"}, "results": {"type": "array"}}
        });
        let mismatch =
            settle(schema, json!({"results": []})).expect_err("`diagnostics` is missing");
        assert_eq!(mismatch.instance_path, "", "{mismatch:?}");
        assert!(mismatch.message.contains("diagnostics"), "{mismatch:?}");
    }

    /// FIG-6330: every entry of a map (`additionalProperties: {schema}`)
    /// is checked against the entry schema.
    #[test]
    fn a_map_with_an_entry_its_schema_refuses_is_refused() {
        let schema = json!({
            "type": "object",
            "properties": {"answers": {"type": "object", "additionalProperties": {"type": "integer"}}}
        });
        let mismatch = settle(schema, json!({"answers": {"a": 1, "b": "two"}}))
            .expect_err("`b` is not an integer");
        assert_eq!(mismatch.instance_path, "/answers/b", "{mismatch:?}");
    }

    /// FIG-6330: a value the declared schema admits settles as the success
    /// it is.
    #[test]
    fn a_value_its_declared_schema_admits_settles() {
        let schema = json!({
            "type": "object",
            "required": ["noul", "answers"],
            "properties": {
                "noul": {"type": "number", "maximum": 1},
                "answers": {"type": "object", "additionalProperties": {"type": "integer"}}
            }
        });
        settle(
            schema,
            json!({"noul": 0.5, "answers": {"a": 1}, "extra": true}),
        )
        .expect("the value fits its schema");
    }
}
