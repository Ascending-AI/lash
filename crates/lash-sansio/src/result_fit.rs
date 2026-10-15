//! A tool's result admitted against its output schema by the rule a code
//! cell reads it by: the schema's kernel result type
//! ([`lash_kernel_check::result_type_of_schema`]) decoding the value the
//! cell would receive (`K-EFF-007`), not JSON Schema validation. A value
//! this admits is one the cell's `perform` of the tool decodes.

use lash_kernel_doc::{Datum, Decoder, KernelDataLimits, NumberPolicy, NumberToken, Type};
use serde_json::Value;

use crate::{JsonSchema, ValueMismatch};

/// Whether `value`, a tool's projected result, fits `output_schema`.
///
/// # Errors
///
/// [`ValueMismatch`]: the JSON pointer of the record field that does not
/// fit, and why.
pub(crate) fn fit_result(
    output_schema: &JsonSchema,
    value: impl FnOnce() -> Value,
) -> Result<(), ValueMismatch> {
    let ty = lash_kernel_check::result_type_of_schema(output_schema.as_value());
    // Every value a cell can receive decodes as `Any`.
    if ty == Type::Any {
        return Ok(());
    }
    let depth = KernelDataLimits::standard().value_depth();
    let datum = datum_of(&value(), depth).ok_or_else(|| ValueMismatch {
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
