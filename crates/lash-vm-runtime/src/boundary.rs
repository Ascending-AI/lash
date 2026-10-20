//! What a host offers a document: effects and handle kinds, in the kernel
//! type grammar.

use std::collections::{BTreeMap, BTreeSet};

pub use lash_kernel_check::{
    MAX_PROJECTED_TYPE_NODES, MAX_RESOLVED_REFERENCES, SchemaConflict, SchemaProjectionRefusal,
    type_of_schema,
};
use lash_kernel_dialect::{EffectControl, Offered, OfferedSource, Operations, QualifiedOperation};
use lash_kernel_doc::{EffectName, OperationRequirement, Type};
use lash_protocol_types::ToolId;

/// Why a boundary could not be stated.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BoundaryError {
    #[error("`{name}` is not an operation: {message}")]
    EffectName { name: String, message: String },
    #[error("the effect `{effect}` is offered twice")]
    DuplicateEffect { effect: EffectName },
}

/// The operations and the handle kinds a host offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostBoundary {
    operations: Operations,
    handles: BTreeSet<String>,
}

/// The turn-ending controls `controls` names, as a front end lowers against
/// them.
#[must_use]
pub fn effect_controls(controls: &lash_protocol_types::TurnControls) -> BTreeSet<EffectControl> {
    controls
        .iter()
        .map(|kind| match kind {
            lash_protocol_types::TurnControlKind::Finish => EffectControl::Finish,
            lash_protocol_types::TurnControlKind::SwitchAgentFrame => {
                EffectControl::SwitchAgentFrame
            }
        })
        .collect()
}

/// What the package that owns `effect`'s path in `view` declares of it
/// (PKG-RUNNER); `None` on a path no package of the view owns.
#[must_use]
pub fn owned_operation(
    view: &lash_kernel_package::PackageView<'_>,
    effect: &EffectName,
) -> Option<lash_execution_contracts::process::OwnedOperation> {
    let operation = QualifiedOperation::from_effect(effect).ok()?;
    let export = view.operation(&operation)?;
    Some(lash_execution_contracts::process::OwnedOperation {
        package: operation.package.to_string(),
        requirement: export.requirement.clone(),
        intents: export
            .intents
            .iter()
            .map(|intent| intent.0.clone())
            .collect(),
    })
}

/// A tool as an offered operation: it takes the tool's input as one record
/// and gives its output, both typed from the tool's JSON schemas, under the
/// contract the tool serves.
#[must_use]
pub fn offered_tool(
    manifest: &lash_protocol_types::ToolManifest,
    contract: &lash_protocol_types::ToolContract,
    source: OfferedSource,
) -> Offered {
    Offered {
        requirement: OperationRequirement {
            signature: lash_protocol_types::tool_signature(
                &contract.input_schema.canonical,
                &contract.output_schema.canonical,
            ),
            contract: manifest.contract_id(contract),
        },
        controls: effect_controls(&manifest.declaration().controls),
        read: manifest.execution_policy == lash_protocol_types::ExecutionPolicy::Read,
        source,
    }
}

impl HostBoundary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers `offered` as `operation`.
    ///
    /// # Errors
    ///
    /// [`BoundaryError::DuplicateEffect`] when the operation is taken.
    pub fn offer_operation(
        &mut self,
        operation: QualifiedOperation,
        offered: Offered,
    ) -> Result<(), BoundaryError> {
        if self.operations.contains_key(&operation) {
            return Err(BoundaryError::DuplicateEffect {
                effect: operation.effect_name(),
            });
        }
        self.operations.insert(operation, offered);
        Ok(())
    }

    /// Every tool of `catalog` as its operation: what a process of that
    /// catalog may perform. A tool that declares a turn control is not
    /// offered: a process cannot end a session turn, so a process document
    /// that performs one is refused at admission as an effect its catalog
    /// does not offer.
    ///
    /// # Errors
    ///
    /// [`BoundaryError`] when two tools take one operation.
    pub fn of_catalog(catalog: &lash_core::ToolCatalog) -> Result<Self, BoundaryError> {
        let mut boundary = Self::new();
        for entry in catalog.entries() {
            if !entry.manifest.declaration().controls.is_empty() {
                continue;
            }
            boundary.offer_operation(
                entry.manifest.operation.clone(),
                offered_tool(
                    &entry.manifest,
                    &entry.contract,
                    OfferedSource::Catalog {
                        tool: entry.manifest.id.to_string(),
                    },
                ),
            )?;
        }
        Ok(boundary)
    }

    /// Admits projection handles of `kind`.
    pub fn handle_kind(&mut self, kind: impl Into<String>) {
        self.handles.insert(kind.into());
    }

    /// The operation a document performs as `effect`.
    pub fn effect(&self, effect: &EffectName) -> Option<(&QualifiedOperation, &Offered)> {
        let operation = QualifiedOperation::from_effect(effect).ok()?;
        self.operations.get_key_value(&operation)
    }

    /// The tool that answers `effect`, when a catalog tool or a deferred
    /// grant does.
    pub fn tool(&self, effect: &EffectName) -> Option<ToolId> {
        match &self.effect(effect)?.1.source {
            OfferedSource::Catalog { tool } | OfferedSource::Deferred { tool } => {
                Some(ToolId::new(tool.as_str()))
            }
            OfferedSource::Unavailable { .. } => None,
        }
    }

    /// The offered operations: what a front end lowers against.
    pub fn operations(&self) -> &Operations {
        &self.operations
    }

    /// The controls each control-declaring operation declares, by effect: a
    /// view of [`Self::operations`] for a run's start, which saves what a
    /// cell's functions perform.
    pub fn controls(&self) -> BTreeMap<EffectName, BTreeSet<EffectControl>> {
        self.operations
            .iter()
            .filter(|(_, offered)| !offered.controls.is_empty())
            .map(|(operation, offered)| (operation.effect_name(), offered.controls.clone()))
            .collect()
    }

    /// Whether the host answers reads through handles of `kind`.
    pub fn admits_handle(&self, kind: &str) -> bool {
        self.handles.contains(kind)
    }
}

/// A tool call's input as the tool reads it, from the operands the call
/// performs the tool with: a tool takes its input as one record
/// ([`offered_tool`]), and a call that omits it gives exactly
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
    use lash_kernel_doc::{RecordType, RecordTypeField, Signature};

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

    /// PKG-RUNNER: on a path a package owns, a tool runs the operation only
    /// when it declares the package's contract id and intents and serves
    /// its signature. On the `dataframe` fixture's `run`, a runner that
    /// declares `dataframe.run/1` and takes the plan, its callback as a
    /// function slot, serves it; one whose input admits no function, one
    /// declaring another id, or one deriving its id, is refused, naming the
    /// tool and `dataframe`.
    #[test]
    fn a_runner_on_a_packaged_path_serves_the_packages_contract() {
        let lash = lash_vm_releases::built().expect("lash's packages");
        let kernel = lash
            .iter()
            .find(|package| package.path().to_string() == "kernel")
            .expect("the kernel package");
        let dataframe =
            lash_kernel_package::testing::dataframe(kernel).expect("the fixture builds");
        let held =
            lash_kernel_package::HeldPackages::install(lash.iter().cloned().chain([dataframe]))
                .expect("one registration");
        let view = held.view(&held.own()).expect("the node's own set");
        let effect = EffectName::new("dataframe.run").expect("an effect name");
        let owner = owned_operation(&view, &effect).expect("the fixture owns `run`");
        assert_eq!(owner.package, "dataframe");
        assert_eq!(
            owner.requirement.contract,
            lash_kernel_doc::ContractId::new("dataframe.run/1")
        );
        let plan = serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "step": { "type": "string" },
                    "exprs": { "type": "array" },
                    "callback": { "x-lash": { "kind": "function" } },
                    "n": { "type": "integer" }
                },
                "required": ["step", "exprs"],
                "additionalProperties": false
            }
        });
        let runner_of = |input: serde_json::Value, contract: Option<&str>| {
            let draft = lash_core::ToolDefinition::raw(
                "tool:dataframe_runner",
                "dataframe.run",
                "Runs a plan.",
                input,
                serde_json::json!({ "type": "array" }),
            )
            .expect("valid declared tool schemas");
            match contract {
                Some(id) => draft.declared_contract(lash_kernel_doc::ContractId::new(id)),
                None => draft,
            }
            .with_execution(std::time::Duration::from_secs(30))
        };
        let runner = |contract: Option<&str>| runner_of(plan.clone(), contract);
        let check = |runner: lash_core::ToolDefinition| {
            lash_execution_contracts::process::check_runner(
                &effect,
                &owner,
                &runner.manifest,
                &runner.contract,
            )
        };
        check(runner(Some("dataframe.run/1"))).expect("the declared runner serves `run`");
        // An input that admits no function does not take the plan's
        // callback, so its signature is what refuses it.
        let data_only = check(runner_of(serde_json::json!({}), Some("dataframe.run/1")));
        assert!(
            matches!(
                &data_only,
                Err(lash_execution_contracts::process::ProcessEnvRefusal::RunnerNotPackaged { reason, .. })
                    if reason.starts_with("its signature")
            ),
            "{data_only:?}"
        );
        for refused in [runner(Some("dataframe.run/2")), runner(None)] {
            let refusal = check(refused).expect_err("another or a derived id is refused");
            let rendered = refusal.to_string();
            assert!(
                matches!(
                    refusal,
                    lash_execution_contracts::process::ProcessEnvRefusal::RunnerNotPackaged { .. }
                ) && rendered.contains("tool:dataframe_runner")
                    && rendered.contains("dataframe"),
                "{rendered}"
            );
        }
    }

    /// Law 11 (FIG-5781): a process cannot end a session turn, so the
    /// boundary a process runs under offers none of its catalog's
    /// turn-ending tools. A document that performs one names an effect
    /// the boundary does not offer, and admission refuses it.
    #[test]
    fn a_process_boundary_offers_no_turn_ending_tool() {
        let ordinary = lash_core::ToolDefinition::raw(
            "tool:lookup",
            "tools.lookup",
            "Looks a value up.",
            serde_json::json!({ "type": "object" }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(30));
        let finish = lash_core::ToolDefinition::control(
            "tool:finish",
            "control.finish",
            "Ends the turn.",
            serde_json::json!({}),
            lash_core::TurnControls::finish(lash_core::JsonSchema::any()),
        )
        .expect("valid declared tool schema")
        .with_execution(std::time::Duration::from_secs(30));
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![ordinary, finish]);

        let offered = HostBoundary::of_catalog(&catalog)
            .expect("the catalog's tools are offered")
            .operations()
            .clone();
        assert_eq!(
            offered
                .keys()
                .map(|operation| operation.effect_name().to_string())
                .collect::<Vec<_>>(),
            ["tools.lookup"]
        );
    }
}
