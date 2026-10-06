//! The parent's half of projection reads (ADR 0132 §9).
//!
//! A projection value is plain data, so nothing here is registered per run:
//! a worker's read names the resource it reads, and the parent answers it
//! through the provider the catalog holds for the resource's type, on
//! whichever node runs the actor.
use crate::{ProjectionAnswer, ProjectionDescription, ProjectionRead};
use lashlang::{
    ProjectedBindings, ProjectedReadRequest, ProjectedValue, ProjectionCatalog, Record,
    ResourceRef, Value,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// The bound a value walk refuses past, as the effect wire does.
const MAX_DEPTH: usize = 64;

/// A run's projection providers, answering its worker's reads.
pub struct Projections {
    catalog: ProjectionCatalog,
}

impl Projections {
    pub fn new(catalog: ProjectionCatalog) -> Self {
        Self { catalog }
    }

    /// The bindings as the worker installs them.
    pub fn describe(bindings: &ProjectedBindings) -> Vec<ProjectionDescription> {
        bindings
            .names()
            .filter_map(|name| {
                let value = bindings.get(&name)?;
                Some(ProjectionDescription {
                    name,
                    value: Value::Projected(value),
                })
            })
            .collect()
    }

    /// Answer one projection frame: every request in it, in order.
    pub async fn read(&self, read: ProjectionRead) -> ProjectionAnswer {
        self.catalog.answer(&read.resource, read.requests).await
    }

    /// `op` with each resource projection in its values materialized through
    /// its provider, so the host reads plain values. A projection whose
    /// provider is missing, fails or does not answer `Materialize` stays as it
    /// is, and the host's own read of it refuses typed.
    pub async fn materialize_operation(
        &self,
        mut op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityOp, String> {
        let mut resources = HashSet::new();
        for value in operation_values(&mut op) {
            collect_resources(value, 0, &mut resources)?;
        }
        if resources.is_empty() {
            return Ok(op);
        }
        let mut materialized = HashMap::new();
        for resource in resources {
            if let Ok(mut answers) = self
                .catalog
                .answer(&resource, vec![ProjectedReadRequest::Materialize])
                .await
                && let Some(Some(answer)) = answers.pop()
            {
                materialized.insert(resource, Value::from(answer));
            }
        }
        for value in operation_values(&mut op) {
            *value = substitute(value.clone(), &materialized);
        }
        Ok(op)
    }
}

/// Every value an operation carries.
fn operation_values(op: &mut lashlang::AbilityOp) -> Vec<&mut Value> {
    use lashlang::AbilityOp;
    let mut values = Vec::new();
    match op {
        AbilityOp::ResourceOperation(operation) => {
            values.push(&mut operation.receiver);
            values.extend(operation.args.iter_mut());
        }
        AbilityOp::ResourceOperationBatch(batch) => {
            for leaf in &mut batch.leaves {
                match leaf {
                    lashlang::ResourceOperationBatchLeaf::Operation(operation) => {
                        values.push(&mut operation.receiver);
                        values.extend(operation.args.iter_mut());
                    }
                    lashlang::ResourceOperationBatchLeaf::Timer(sleep) => {
                        values.push(&mut sleep.value);
                    }
                }
            }
        }
        AbilityOp::Await(argument)
        | AbilityOp::Print(argument)
        | AbilityOp::Finish(argument)
        | AbilityOp::Fail(argument) => values.push(argument),
        AbilityOp::ProcessEvent(event) => values.push(&mut event.value),
        AbilityOp::Sleep(sleep) => values.push(&mut sleep.value),
        AbilityOp::WaitSignal { .. } => {}
    }
    values
}

fn collect_resources(
    value: &Value,
    depth: usize,
    resources: &mut HashSet<ResourceRef>,
) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("projection request exceeds depth bound".into());
    }
    match value {
        Value::Projected(projected) => {
            if let Some(resource) = projected.resource_ref() {
                resources.insert(resource.clone());
            }
        }
        Value::List(values) | Value::Tuple(values) => {
            for value in values.iter() {
                collect_resources(value, depth + 1, resources)?;
            }
        }
        Value::Record(record) => {
            for value in record.values() {
                collect_resources(value, depth + 1, resources)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn substitute(value: Value, materialized: &HashMap<ResourceRef, Value>) -> Value {
    if !value.contains_projected() {
        return value;
    }
    match value {
        Value::Projected(projected) => match projected
            .resource_ref()
            .and_then(|resource| materialized.get(resource))
        {
            Some(value) => Value::Projected(ProjectedValue::scalar(
                projected.name().to_owned(),
                value.clone(),
            )),
            None => Value::Projected(projected),
        },
        Value::List(values) => Value::List(
            values
                .iter()
                .cloned()
                .map(|value| substitute(value, materialized))
                .collect::<Vec<_>>()
                .into(),
        ),
        Value::Tuple(values) => Value::Tuple(
            values
                .iter()
                .cloned()
                .map(|value| substitute(value, materialized))
                .collect::<Vec<_>>()
                .into(),
        ),
        Value::Record(record) => Value::Record(Arc::new(
            record
                .iter()
                .map(|(key, value)| (key.to_string(), substitute(value.clone(), materialized)))
                .collect::<Record>(),
        )),
        other => other,
    }
}
