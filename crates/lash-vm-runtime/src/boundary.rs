//! What a host offers a document: effects and handle kinds, in the kernel
//! type grammar.

use std::collections::{BTreeMap, BTreeSet};

pub use lash_kernel_check::{SchemaConflict, SchemaProjectionRefusal, type_of_schema};
use lash_kernel_doc::{EffectName, Signature, Type};
use lash_protocol_types::ToolId;

/// One effect a document may perform, and the tool that answers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEffect {
    pub tool: ToolId,
    /// The signature a document's manifest must state to perform it.
    pub signature: Signature,
    /// The turn-ending controls the tool declares its result may be.
    pub controls: lash_protocol_types::TurnControls,
}

/// Why a boundary could not be stated.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BoundaryError {
    #[error("`{name}` is not an effect name: {message}")]
    EffectName { name: String, message: String },
    #[error("the effect `{effect}` is offered twice")]
    DuplicateEffect { effect: EffectName },
}

/// The effects and the handle kinds a host offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostBoundary {
    effects: BTreeMap<EffectName, HostEffect>,
    handles: BTreeSet<String>,
}

impl HostBoundary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers `effect` under `name`.
    ///
    /// # Errors
    ///
    /// [`BoundaryError`] when the name is no effect name or is taken.
    pub fn offer(&mut self, name: &str, effect: HostEffect) -> Result<(), BoundaryError> {
        let name = EffectName::new(name).map_err(|error| BoundaryError::EffectName {
            name: name.to_owned(),
            message: error.to_string(),
        })?;
        if self.effects.contains_key(&name) {
            return Err(BoundaryError::DuplicateEffect { effect: name });
        }
        self.effects.insert(name, effect);
        Ok(())
    }

    /// Offers a tool as an effect that takes the tool's input as one record
    /// and gives its output, both typed from the tool's JSON schemas.
    ///
    /// # Errors
    ///
    /// [`BoundaryError`].
    pub fn offer_tool(
        &mut self,
        name: &str,
        tool: ToolId,
        input: &lash_protocol_types::JsonSchema,
        output: &lash_protocol_types::JsonSchema,
        controls: lash_protocol_types::TurnControls,
    ) -> Result<(), BoundaryError> {
        self.offer(
            name,
            HostEffect {
                tool,
                signature: lash_protocol_types::tool_signature(input, output),
                controls,
            },
        )
    }

    /// Admits projection handles of `kind`.
    pub fn handle_kind(&mut self, kind: impl Into<String>) {
        self.handles.insert(kind.into());
    }

    /// The effect a document performs under `name`.
    pub fn effect(&self, name: &EffectName) -> Option<&HostEffect> {
        self.effects.get(name)
    }

    /// Every effect with its signature: what a front end lowers against.
    pub fn signatures(&self) -> BTreeMap<EffectName, Signature> {
        self.effects
            .iter()
            .map(|(name, effect)| (name.clone(), effect.signature.clone()))
            .collect()
    }

    /// The controls each control-declaring effect declares, as a front end
    /// lowers against them: an effect that declares none is absent.
    pub fn controls(&self) -> BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>> {
        self.effects
            .iter()
            .filter(|(_, effect)| !effect.controls.is_empty())
            .map(|(name, effect)| {
                let controls = effect
                    .controls
                    .iter()
                    .map(|kind| match kind {
                        lash_protocol_types::TurnControlKind::Finish => {
                            lash_kernel_dialect::EffectControl::Finish
                        }
                        lash_protocol_types::TurnControlKind::SwitchAgentFrame => {
                            lash_kernel_dialect::EffectControl::SwitchAgentFrame
                        }
                    })
                    .collect();
                (name.clone(), controls)
            })
            .collect()
    }

    /// Whether the host answers reads through handles of `kind`.
    pub fn admits_handle(&self, kind: &str) -> bool {
        self.handles.contains(kind)
    }
}

/// A tool call's input as the tool reads it, from the operands the call
/// performs the tool with: a tool takes its input as one record
/// ([`HostBoundary::offer_tool`]), and a call that omits it gives exactly
/// the empty object, never null or the schema's defaults. Cells and
/// processes both read a tool call's operands through this one rule.
///
/// # Errors
///
/// What `project` refuses for the one operand, or what `excess` makes of the
/// count of more than one.
pub fn tool_input<T, E>(
    args: &[T],
    project: impl FnOnce(&T) -> Result<serde_json::Value, E>,
    excess: impl FnOnce(usize) -> E,
) -> Result<serde_json::Value, E> {
    match args {
        [] => Ok(serde_json::Value::Object(serde_json::Map::new())),
        [input] => project(input),
        more => Err(excess(more.len())),
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
                    Some(rest) => json_serves(rest, value)
                        .map_err(|message| format!("field `{name}`: {message}"))?,
                    None => return Err(format!("the record has no field `{name}`")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use lash_kernel_doc::{RecordType, RecordTypeField};

    /// OPEN-RESULT-FITS (law 18, D4): a tool's result projects to an open
    /// record, so a tool whose result adds a field serves a document checked
    /// against the earlier schema-projected result, and the runtime check
    /// takes the extra field. A manually authored closed result stays
    /// closed and refuses it. Input projection is unchanged: a closed record.
    #[test]
    fn a_result_that_adds_a_field_fits_a_projected_result_and_not_a_closed_one() {
        let input = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["q"],
            "properties": { "q": { "type": "string" } },
        });
        let result = |fields: serde_json::Value| {
            serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["hits"],
                "properties": fields,
            })
        };
        let earlier = result(serde_json::json!({ "hits": { "type": "integer" } }));
        let later = result(serde_json::json!({
            "hits": { "type": "integer" },
            "took_ms": { "type": "integer" },
        }));
        let admit = |schema: &serde_json::Value| {
            lash_protocol_types::JsonSchema::admit(schema.clone()).expect("an admissible schema")
        };
        let expected = lash_protocol_types::tool_signature(&admit(&input), &admit(&earlier));
        let provided = lash_protocol_types::tool_signature(&admit(&input), &admit(&later));
        assert!(
            lash_kernel_check::signature_serves(&provided, &expected),
            "the added field fits the projected result"
        );
        let closed = Signature {
            result: type_of_schema(&earlier).expect("a result type"),
            ..expected.clone()
        };
        assert!(
            !lash_kernel_check::signature_serves(&provided, &closed),
            "a closed result refuses the open one"
        );
        assert_eq!(
            provided.params[0].ty,
            type_of_schema(&input).expect("an input type"),
            "input stays a closed record"
        );
        json_serves(
            &expected.result,
            &serde_json::json!({ "hits": 3, "took_ms": 12 }),
        )
        .expect("the runtime check takes the extra field");
    }

    /// Target 4: a tool's signature is stated in the kernel type grammar. A
    /// closed object schema is a record with its optional fields marked, a
    /// schema the grammar has no exact type for is `Json`, and a composition
    /// with no member has none.
    #[test]
    fn a_tool_schema_is_a_kernel_type() {
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["path"],
            "properties": {
                "path": {"type": "string"},
                "lines": {"type": "array", "items": {"type": "integer"}},
                "mode": {"enum": ["read", "write"]},
            },
        });
        let field = |name: &str, ty: Type, optional: bool| RecordTypeField {
            name: name.to_owned(),
            ty,
            optional,
        };
        assert_eq!(
            type_of_schema(&schema).expect("a record type"),
            Type::Record(RecordType {
                fields: vec![
                    field("lines", Type::List(Box::new(Type::Int)), true),
                    field(
                        "mode",
                        Type::Enum(vec!["read".into(), "write".into()]),
                        true
                    ),
                    field("path", Type::Text, false),
                ],
                rest: None,
            })
        );
        assert_eq!(
            type_of_schema(&serde_json::json!({"type": "object"})),
            Ok(Type::Json)
        );
        assert!(type_of_schema(&serde_json::json!({"oneOf": []})).is_err());
    }

    /// Law 11 (FIG-5781): a process cannot end a session turn, so the
    /// boundary a process runs under offers none of its catalog's
    /// turn-ending tools. A document that performs one names an effect
    /// the boundary does not offer, and admission refuses it.
    #[test]
    fn a_process_boundary_offers_no_turn_ending_tool() {
        use crate::{ToolBinding, ToolDefinitionBindingExt as _};
        let ordinary = lash_core::ToolDefinition::raw(
            "tool:lookup",
            "lookup",
            "Looks a value up.",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30))
        .with_tool_binding(ToolBinding::new(["tools"], "lookup"));
        let finish = lash_core::ToolDefinition::control(
            "tool:finish",
            "finish",
            "Ends the turn.",
            serde_json::json!({}),
            lash_core::TurnControls::finish(lash_core::JsonSchema::any()),
        )
        .expect("valid declared tool schema")
        .with_execution(std::time::Duration::from_secs(30))
        .with_tool_binding(ToolBinding::new(["control"], "finish"));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![ordinary, finish]);

        let offered = HostBoundary::of_catalog(&catalog)
            .expect("the catalog's tools are bound")
            .signatures();
        assert_eq!(
            offered.keys().map(EffectName::as_str).collect::<Vec<_>>(),
            ["tools.lookup"]
        );
    }
}
