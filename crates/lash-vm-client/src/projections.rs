use crate::{ProjectionDescription, ProjectionRead};
use lashlang::{ProjectedBindings, ProjectedReadResponse, ProjectedValue, Record, Value};
use std::sync::{Arc, Mutex};

/// Host descriptors are retained only for this run. Tokens are descriptions,
/// never authority, and another run cannot accidentally reuse their keys.
pub struct Projections {
    values: Mutex<Vec<ProjectedValue>>,
    namespace: String,
    max_nodes: usize,
}
impl Projections {
    pub fn new(
        bindings: &ProjectedBindings,
        max_nodes: usize,
    ) -> Result<(Self, Vec<ProjectionDescription>), String> {
        let values = bindings
            .names()
            .filter_map(|name| bindings.get(&name).map(|v| (name, v)))
            .collect::<Vec<_>>();
        if values.len() > max_nodes {
            return Err("projection registry exceeds its bound".into());
        }
        let descriptions = values
            .iter()
            .enumerate()
            .map(|(key, (name, value))| ProjectionDescription {
                name: name.clone(),
                key,
                type_name: value.type_name().to_string(),
                scalar: value.scalar_value().cloned(),
            })
            .collect();
        Ok((
            Self {
                values: Mutex::new(values.into_iter().map(|(_, value)| value).collect()),
                namespace: uuid::Uuid::new_v4().to_string(),
                max_nodes,
            },
            descriptions,
        ))
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn read(&self, request: ProjectionRead) -> Result<Option<ProjectedReadResponse>, String> {
        let value = self
            .values
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(request.key)
            .cloned();
        let Some(value) = value else {
            return Err("projection key is outside the admitted registry".into());
        };
        let Some(response) = value
            .host_descriptor()
            .and_then(|descriptor| descriptor.read_one(request.request))
        else {
            return Ok(None);
        };
        Ok(Some(match response {
            ProjectedReadResponse::Value(value) => {
                ProjectedReadResponse::Value(self.export(value)?)
            }
            other => other,
        }))
    }
    pub fn import_operation(
        &self,
        mut op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityOp, String> {
        use lashlang::AbilityOp;
        let value = |value: &mut Value| -> Result<(), String> {
            *value = self.import(value.clone(), 0)?;
            Ok(())
        };
        match &mut op {
            AbilityOp::ResourceOperation(operation) => {
                value(&mut operation.receiver)?;
                for argument in &mut operation.args {
                    value(argument)?;
                }
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                for leaf in &mut batch.leaves {
                    match leaf {
                        lashlang::ResourceOperationBatchLeaf::Operation(operation) => {
                            value(&mut operation.receiver)?;
                            for argument in &mut operation.args {
                                value(argument)?;
                            }
                        }
                        lashlang::ResourceOperationBatchLeaf::Timer(sleep) => {
                            value(&mut sleep.value)?
                        }
                    }
                }
            }
            AbilityOp::Await(argument)
            | AbilityOp::Print(argument)
            | AbilityOp::Finish(argument)
            | AbilityOp::Fail(argument) => value(argument)?,
            AbilityOp::ProcessEvent(event) => value(&mut event.value)?,
            AbilityOp::Sleep(sleep) => value(&mut sleep.value)?,
            AbilityOp::WaitSignal { .. } => {}
        }
        Ok(op)
    }
    fn import(&self, value: Value, depth: usize) -> Result<Value, String> {
        if depth > 64 {
            return Err("projection request exceeds depth bound".into());
        }
        Ok(match value {
            Value::Projected(value) => {
                if let Some(rest) = value.name().strip_prefix("worker-projection/") {
                    let (namespace, rest) = rest
                        .split_once('/')
                        .ok_or("malformed projection namespace")?;
                    if namespace != self.namespace {
                        return Err("projection belongs to another run".into());
                    }
                    let (key, _) = rest.split_once('/').ok_or("malformed projection key")?;
                    let key = key.parse::<usize>().map_err(|_| "invalid projection key")?;
                    let descriptor = self
                        .values
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(key)
                        .cloned()
                        .ok_or("projection key is outside admitted registry")?;
                    match value.scalar_value() {
                        Some(scalar) => Value::Projected(ProjectedValue::scalar(
                            value.name().to_owned(),
                            self.import(scalar.clone(), depth + 1)?,
                        )),
                        None => Value::Projected(descriptor),
                    }
                } else {
                    Value::Projected(value)
                }
            }
            Value::List(values) => Value::List(
                values
                    .iter()
                    .cloned()
                    .map(|value| self.import(value, depth + 1))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            Value::Tuple(values) => Value::Tuple(
                values
                    .iter()
                    .cloned()
                    .map(|value| self.import(value, depth + 1))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            Value::Record(values) => Value::Record(Arc::new(
                values
                    .iter()
                    .map(|(key, value)| {
                        self.import(value.clone(), depth + 1)
                            .map(|value| (key.to_string(), value))
                    })
                    .collect::<Result<Record, _>>()?,
            )),
            other => other,
        })
    }
    pub fn export_outcome(
        &self,
        outcome: lashlang::AbilityOutcome,
    ) -> Result<lashlang::AbilityOutcome, String> {
        use lashlang::{
            AbilityOutcome, ResourceOperationBatchOutcome as Batch,
            ResourceOperationOutcome as Leaf,
        };
        let leaf = |leaf| match leaf {
            Leaf::Value(value) => self.export(value).map(Leaf::Value),
            other => Ok(other),
        };
        Ok(match outcome {
            AbilityOutcome::Value(value) => AbilityOutcome::Value(self.export(value)?),
            AbilityOutcome::ResourceOperationBatch(Batch::AllResults(values)) => {
                AbilityOutcome::ResourceOperationBatch(Batch::AllResults(
                    values.into_iter().map(leaf).collect::<Result<_, _>>()?,
                ))
            }
            AbilityOutcome::ResourceOperationBatch(Batch::Selected {
                leaf: index,
                result,
            }) => AbilityOutcome::ResourceOperationBatch(Batch::Selected {
                leaf: index,
                result: leaf(result)?,
            }),
            other => other,
        })
    }
    pub fn export(&self, value: Value) -> Result<Value, String> {
        let mut remaining = self.max_nodes;
        self.export_at(value, 0, &mut remaining)
    }
    fn export_at(
        &self,
        value: Value,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<Value, String> {
        if depth > 64 || *remaining == 0 {
            return Err("projection result exceeds its structural bound".into());
        }
        *remaining -= 1;
        let mut items = |values: &[Value]| {
            values
                .iter()
                .cloned()
                .map(|value| self.export_at(value, depth + 1, remaining))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(match value {
            Value::Projected(value) if value.scalar_value().is_none() => {
                let name = value.name().to_owned();
                let mut values = self
                    .values
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if values.len() >= self.max_nodes {
                    return Err("projection registry exceeds its bound".into());
                }
                let key = values.len();
                values.push(value.clone());
                Value::Projected(ProjectedValue::custom(
                    format!("worker-projection/{}/{key}/{name}", self.namespace),
                    Arc::new(Shape(value.type_name().to_owned())),
                ))
            }
            Value::List(values) => Value::List(items(&values)?.into()),
            Value::Tuple(values) => Value::Tuple(items(&values)?.into()),
            Value::Record(values) => Value::Record(Arc::new(
                values
                    .iter()
                    .map(|(key, value)| {
                        self.export_at(value.clone(), depth + 1, remaining)
                            .map(|value| (key.to_string(), value))
                    })
                    .collect::<Result<Record, _>>()?,
            )),
            other => other,
        })
    }
}
struct Shape(String);
impl lashlang::ProjectedHostDescriptor for Shape {
    fn type_name(&self) -> &str {
        &self.0
    }
    fn read_one(&self, _: lashlang::ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        None
    }
}
