//! Code authoring reads one accepted Package Advertisement projection.
use crate::process::{KernelDocuments, WorkflowEnvironmentRequest};
use lash_execution_contracts::process::linking::EnginePortError;
use {lash_kernel_value::Type, lash_vocabulary::package::NamespaceSnapshot};
use {lash_kernel_value::package::Attachment, lash_kernel_value::package::PublicationContext};
#[derive(Clone, Debug)]
pub struct HostBoundary {
    pub snapshot: NamespaceSnapshot,
    pub context: PublicationContext,
    pub attachment: Attachment,
}
impl HostBoundary {
    pub async fn namespaces(
        &self,
        documents: &KernelDocuments,
    ) -> Result<lash_kernel_dialect::Namespaces, EnginePortError> {
        documents
            .namespace(&WorkflowEnvironmentRequest {
                snapshot: self.snapshot.clone(),
                context: self.context.clone(),
                attachment: self.attachment.clone(),
            })
            .await
    }
}
pub fn effect_controls(
    controls: &lash_kernel_value::TurnControls,
) -> std::collections::BTreeSet<lash_kernel_dialect::EffectControl> {
    controls
        .iter()
        .map(|kind| match kind {
            lash_kernel_value::TurnControlKind::Finish => {
                lash_kernel_dialect::EffectControl::Finish
            }
            lash_kernel_value::TurnControlKind::SwitchAgentFrame => {
                lash_kernel_dialect::EffectControl::SwitchAgentFrame
            }
        })
        .collect()
}
pub use {
    lash_kernel_value::schema::MAX_PROJECTED_TYPE_NODES,
    lash_kernel_value::schema::MAX_RESOLVED_REFERENCES, lash_kernel_value::schema::SchemaConflict,
    lash_kernel_value::schema::SchemaProjectionRefusal, lash_kernel_value::schema::type_of_schema,
};

pub use lash_core_execution::kernel_runner::tool_input;

/// Deferred Imports carry the source call's positional operands as one Json
/// list until discovery pins the target. Unpack that list only at the tool
/// input boundary; ordinary operation operands retain their declared shape.
pub fn tool_arguments(
    args: &[lash_kernel_doc::Datum],
    deferred: bool,
) -> Result<&[lash_kernel_doc::Datum], String> {
    if !deferred {
        return Ok(args);
    }
    match args {
        [lash_kernel_doc::Datum::List(positional)] => Ok(positional),
        _ => Err("a deferred operation requires one packed argument list".into()),
    }
}

/// Whether a JSON value is one a parameter of type `ty` takes, as far as
/// JSON can say: a value whose type has no JSON spelling (bytes, a
/// timestamp, a handle) is left to the body's own strict operations.
///
/// # Errors
///
/// What the value is and what the type takes.
pub(crate) fn json_serves(ty: &Type, value: &serde_json::Value) -> Result<(), String> {
    use serde_json::Value;
    let mismatch = |expected: &str| Err(format!("expected {expected}, found `{value}`"));
    match (ty, value) {
        (Type::Null, Value::Null)
        | (Type::Bool, Value::Bool(_))
        | (Type::Text, Value::String(_))
        | (Type::Float | Type::Number, Value::Number(_)) => Ok(()),
        (Type::Int, Value::Number(number)) if number.is_i64() || number.is_u64() => Ok(()),
        (Type::Null, _) => mismatch("null"),
        (Type::Bool, _) => mismatch("a boolean"),
        (Type::Text, _) => mismatch("a text"),
        (Type::Int, _) => mismatch("an integer"),
        (Type::Float | Type::Number, _) => mismatch("a number"),
        (Type::Enum(texts), Value::String(text)) if texts.contains(text) => Ok(()),
        (Type::Enum(texts), _) => mismatch(&format!("one of {texts:?}")),
        (Type::List(item) | Type::Set(item), Value::Array(items)) => {
            items.iter().try_for_each(|value| json_serves(item, value))
        }
        (Type::List(_) | Type::Set(_), _) => mismatch("a list"),
        (Type::Tuple(members), Value::Array(items)) if members.len() == items.len() => members
            .iter()
            .zip(items)
            .try_for_each(|(ty, value)| json_serves(ty, value)),
        (Type::Tuple(members), _) => mismatch(&format!("a tuple of {}", members.len())),
        (Type::Record(record), Value::Object(fields)) => {
            for field in &record.fields {
                match fields.get(&field.name) {
                    Some(value) => json_serves(&field.ty, value)
                        .map_err(|message| format!("field `{}`: {message}", field.name))?,
                    None if field.optional => {}
                    None => return Err(format!("field `{}` is missing", field.name)),
                }
            }
            for (name, value) in fields {
                if record.fields.iter().any(|field| field.name == *name) {
                    continue;
                }
                match &record.rest {
                    lash_kernel_value::RecordRest::Keep(rest) => json_serves(rest, value)
                        .map_err(|message| format!("field `{name}`: {message}"))?,
                    lash_kernel_value::RecordRest::Closed => {
                        return Err(format!("the record has no field `{name}`"));
                    }
                    lash_kernel_value::RecordRest::Drop => {}
                }
            }
            Ok(())
        }
        (Type::Record(_), _) => mismatch("a record"),
        (Type::Union(types), _) => {
            if types.iter().any(|ty| json_serves(ty, value).is_ok()) {
                Ok(())
            } else {
                mismatch("a value of the union")
            }
        }
        _ => Ok(()),
    }
}
