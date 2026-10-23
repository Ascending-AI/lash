//! Schema admission plus neutral encoded-data fitting. Runtime Datum and
//! heap decoding belong to the runtime; this checks the JSON result it receives.
use lash_vocabulary::{JsonSchema, KernelDataLimits, Type, ValueMismatch};
use serde_json::Value;

pub(crate) fn admit_value(
    schema: &JsonSchema,
    value: impl FnOnce() -> Value,
) -> Result<(), ValueMismatch> {
    if schema
        .as_value()
        .as_object()
        .is_some_and(serde_json::Map::is_empty)
    {
        return Ok(());
    }
    let value = value();
    schema.validate(&value)?;
    let ty = schema.result_type();
    if matches!(ty, Type::Any | Type::Json) {
        return Ok(());
    }
    let depth = KernelDataLimits::standard().value_depth();
    if !within_depth(&value, depth) {
        return Err(mismatch(format!(
            "it exceeds the configured {depth}-level value depth"
        )));
    }
    fit(&value, ty, depth)
}

fn within_depth(value: &Value, budget: usize) -> bool {
    let Some(budget) = budget.checked_sub(1) else {
        return false;
    };
    match value {
        Value::Array(items) => items.iter().all(|value| within_depth(value, budget)),
        Value::Object(fields) => fields.values().all(|value| within_depth(value, budget)),
        _ => true,
    }
}

// Any/Json decoding takes its own recursive step after the type-fitting step.
fn fit_data(value: &Value, budget: usize) -> Result<(), ValueMismatch> {
    let budget = budget
        .checked_sub(1)
        .ok_or_else(|| mismatch("it exceeds the configured value depth"))?;
    match value {
        Value::Array(items) => items.iter().try_for_each(|value| fit_data(value, budget)),
        Value::Object(fields) => fields
            .values()
            .try_for_each(|value| fit_data(value, budget)),
        Value::Number(number) => {
            let text = number.to_string();
            if (!text.contains(['.', 'e', 'E']) && integral(&text))
                || number.as_f64().is_some_and(f64::is_finite)
            {
                Ok(())
            } else {
                Err(mismatch("the number has no finite nearest float"))
            }
        }
        _ => Ok(()),
    }
}

fn mismatch(message: impl Into<String>) -> ValueMismatch {
    ValueMismatch {
        instance_path: String::new(),
        message: message.into(),
    }
}

// Match the runtime's exact decimal integer admission without creating a heap
// value or rounding the input through binary64.
fn canonical_decimal(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && text != "-0"
}

fn integral(text: &str) -> bool {
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let Ok(exponent) = exponent.parse::<i64>() else {
        return false;
    };
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte == b'0')
    {
        return true;
    }
    let Some(scale) = exponent.checked_sub(fraction.len() as i64) else {
        return false;
    };
    let zeroes = whole
        .bytes()
        .chain(fraction.bytes())
        .rev()
        .take_while(|byte| *byte == b'0')
        .count() as i64;
    let adjusted = scale + zeroes.min(scale.saturating_neg().max(0));
    (0..=4096).contains(&adjusted)
}

fn fit(value: &Value, ty: &Type, budget: usize) -> Result<(), ValueMismatch> {
    let budget = budget
        .checked_sub(1)
        .ok_or_else(|| mismatch("it exceeds the configured value depth"))?;
    let finite = |number: &serde_json::Number| number.as_f64().is_some_and(f64::is_finite);
    match (ty, value) {
        (Type::Union(types), _) => types
            .iter()
            .find_map(|ty| fit(value, ty, budget).ok())
            .ok_or_else(|| mismatch("value fits no member of the union")),
        (Type::Number, Value::Number(number)) => {
            let text = number.to_string();
            if (!text.contains(['.', 'e', 'E']) && integral(&text)) || finite(number) {
                Ok(())
            } else {
                Err(mismatch("the number has no finite nearest float"))
            }
        }
        (Type::Any | Type::Json, _) => fit_data(value, budget),
        (Type::Null, Value::Null)
        | (Type::Bool, Value::Bool(_))
        | (Type::Text, Value::String(_)) => Ok(()),
        (Type::Timestamp, Value::String(text)) if canonical_decimal(text) => Ok(()),
        (Type::Bytes, Value::String(text))
            if text.len().is_multiple_of(2)
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) =>
        {
            Ok(())
        }
        (Type::Int, Value::Number(number)) if integral(&number.to_string()) => Ok(()),
        (Type::Float, Value::Number(number)) if finite(number) => Ok(()),
        (Type::Enum(members), Value::String(text)) if members.contains(text) => Ok(()),
        (Type::Tuple(types), Value::Array(items)) if types.len() == items.len() => items
            .iter()
            .zip(types)
            .try_for_each(|(value, ty)| fit(value, ty, budget)),
        (Type::List(ty) | Type::Set(ty), Value::Array(items)) => {
            items.iter().try_for_each(|value| fit(value, ty, budget))
        }
        (Type::Map(map), Value::Object(fields)) => fields.iter().try_for_each(|(key, value)| {
            fit(&Value::String(key.clone()), &map.key, budget)?;
            fit(value, &map.value, budget)
        }),
        (Type::Record(record), Value::Object(fields)) => {
            for field in &record.fields {
                if !field.optional && !fields.contains_key(&field.name) {
                    return Err(mismatch(format!(
                        "the required field `{}` is missing",
                        field.name
                    )));
                }
            }
            for (name, value) in fields {
                if matches!(record.rest, lash_vocabulary::RecordRest::Drop)
                    && !record.fields.iter().any(|field| field.name == *name)
                {
                    continue;
                }
                let ty = record
                    .fields
                    .iter()
                    .find(|field| field.name == *name)
                    .map(|field| &field.ty)
                    .or(record.rest.kept_type())
                    .ok_or_else(|| mismatch(format!("the record type has no field `{name}`")))?;
                fit(value, ty, budget).map_err(|mut error| {
                    error.instance_path = format!(
                        "/{}{}",
                        name.replace('~', "~0").replace('/', "~1"),
                        error.instance_path
                    );
                    error
                })?;
            }
            Ok(())
        }
        _ => Err(mismatch(format!(
            "value does not fit {}",
            lash_vocabulary::print_type(ty)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::ToolCallOutput;
    use lash_vocabulary::json_schema::JsonSchema;
    use lash_vocabulary::json_schema::ValueMismatch;
    use lash_vocabulary::tool_declaration::DeclarationRefusal;
    use lash_vocabulary::tool_declaration::ToolDeclaration;

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
    /// L-DIALECT-SERVICES/K-EFF-007: ordinary operation schemas retain their
    /// timestamp/bytes types and reject malformed encoded results.
    #[test]
    fn ordinary_service_markers_preserve_typed_encoded_results() {
        for (kind, expected, valid, invalid) in [
            (
                "timestamp",
                lash_vocabulary::Type::Timestamp,
                "12345678901234567890123456789012345678901234567890",
                "1.5",
            ),
            ("bytes", lash_vocabulary::Type::Bytes, "00abcdef", "ABC"),
        ] {
            let schema =
                JsonSchema::admit(json!({"type":"string", "x-lash":{"kind":kind}})).unwrap();
            assert_eq!(schema.result_type(), &expected);
            super::admit_value(&schema, || json!(valid)).unwrap();
            assert!(super::admit_value(&schema, || json!(invalid)).is_err());
        }
    }
}
