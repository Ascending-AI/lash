use lash_sansio::sync::MutexExt;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Record,
    RuntimeError, State, TypeExpr, Value,
};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::execute_support::{self, ExecuteError};

#[derive(Default)]
struct TestHost {
    files: HashMap<String, String>,
    globs: HashMap<String, Vec<String>>,
    observations: std::sync::Mutex<Vec<Value>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    calls: std::sync::Mutex<Vec<String>>,
}

impl TestHost {
    fn with_file(mut self, path: &str, content: &str) -> Self {
        self.files.insert(path.to_string(), content.to_string());
        self
    }
}

impl ExecutionHost for TestHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            // A started process is a real handle record, never its bare
            // result: awaiting a resolved value is a guest error (FIG-2764).
            AbilityOp::ResourceOperation(operation) if operation.operation == "start" => {
                // The `definition` slot carries a process value, not a tool
                // name the host could dispatch on, and every fixture process
                // here echoes its input: the handle settles to the `value` the
                // start passed in `args`.
                let args = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("args"))
                    .and_then(Value::as_record)
                    .cloned()
                    .unwrap_or_default();
                let value = args.get("value").cloned().unwrap_or(Value::Null);
                let mut handle = Record::new();
                handle.insert(
                    lash_sansio::handle::HANDLE_FIELD.to_string(),
                    Value::String(lash_sansio::handle::HANDLE_KIND.into()),
                );
                handle.insert(
                    "id".to_string(),
                    Value::String(
                        lash_sansio::handle::HandleId::process(&lash_sansio::ProcessId::fixture(
                            "language",
                        ))
                        .as_str()
                        .into(),
                    ),
                );
                handle.insert("value".to_string(), value);
                Ok(AbilityResult::Value(Value::Record(Arc::new(handle))))
            }
            AbilityOp::ResourceOperation(operation) => self
                .perform_resource_operation(*operation)
                .await
                .map(AbilityResult::Value),
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = futures::future::join_all(batch.leaves.iter().map(|leaf| async {
                    match leaf {
                        lashlang::ResourceOperationBatchLeaf::Operation(operation) => {
                            lashlang::ResourceOperationResult::from_result(
                                self.perform_resource_operation(operation.clone()).await,
                            )
                        }
                        lashlang::ResourceOperationBatchLeaf::Timer(_) => {
                            lashlang::ResourceOperationResult::Value(Value::Undefined)
                        }
                    }
                }))
                .await;
                Ok(AbilityResult::ResourceOperationBatch(
                    batch.answer_in_leaf_order(results),
                ))
            }
            AbilityOp::Await(handle) => handle
                .as_record()
                .filter(|record| record.get("__handle__").is_some())
                .and_then(|record| record.get("value").cloned())
                .map(AbilityResult::Value)
                .ok_or_else(|| ExecutionHostError::new("expected handle record")),
            AbilityOp::Print(value) => {
                self.observations.lock_recover().push(value);
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

impl TestHost {
    async fn perform_resource_operation(
        &self,
        operation: lashlang::ResourceOperation,
    ) -> Result<Value, ExecutionHostError> {
        let empty = Record::new();
        let args = operation
            .args
            .first()
            .and_then(Value::as_record)
            .map_or(&empty, |record| record);
        let name = test_host_operation(&operation)?;
        self.call_tool(&name, args).await
    }

    async fn call_tool(&self, name: &str, args: &Record) -> Result<Value, ExecutionHostError> {
        match name {
            "read_file" => {
                let path = expect_string(args, "path")?;
                match self.files.get(path) {
                    Some(content) => Ok(Value::String(content.clone().into())),
                    None => Err(ExecutionHostError::new(format!("missing file: {path}"))),
                }
            }
            "glob" => {
                let pattern = expect_string(args, "pattern")?;
                let values: Vec<_> = self
                    .globs
                    .get(pattern)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|value| Value::String(value.into()))
                    .collect();
                Ok(Value::List(values.into()))
            }
            "sleep_echo" => {
                self.calls
                    .lock_recover()
                    .push(expect_string(args, "value")?.to_string());
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                loop {
                    let max = self.max_active.load(Ordering::SeqCst);
                    if active <= max {
                        break;
                    }
                    if self
                        .max_active
                        .compare_exchange(max, active, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(Value::String(
                    expect_string(args, "value")?.to_string().into(),
                ))
            }
            _ => Err(ExecutionHostError::new(format!("unknown tool: {name}"))),
        }
    }
}

fn test_host_operation(
    operation: &lashlang::ResourceOperation,
) -> Result<String, ExecutionHostError> {
    match &operation.receiver {
        Value::Resource(receiver) => test_host_environment()
            .resources
            .resolve_module_operation(
                &receiver.resource_type,
                &receiver.alias,
                &operation.operation,
            )
            .map(|binding| binding.host_operation.to_string())
            .ok_or_else(|| {
                ExecutionHostError::new(format!(
                    "module `{}` of type `{}` does not expose operation `{}`",
                    receiver.alias, receiver.resource_type, operation.operation
                ))
            }),
        _ => Ok(operation.operation.clone()),
    }
}

fn finished(outcome: ExecutionOutcome) -> Value {
    match outcome {
        ExecutionOutcome::Finished(value) => value,
        ExecutionOutcome::Continued => panic!("expected `finish`"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

async fn execute<H: ExecutionHost>(
    source: &str,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, ExecuteError> {
    execute_support::execute(source, state, host, test_host_environment()).await
}

#[expect(
    clippy::expect_used,
    reason = "fixture catalog registers each host operation once into a fresh catalog, per each message"
)]
fn test_host_environment() -> lashlang::LashlangHostEnvironment {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["files"],
            "Files",
            "read",
            "read_file",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["files"],
            "Files",
            "glob",
            "glob",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["agents"],
            "Agents",
            "spawn",
            "spawn_agent",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "sleep_echo",
            "sleep_echo",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    // FIG-2999: starting, signalling and yielding are leaf tools now. `start`
    // types its `definition` slot as a process, and that expected type is what
    // lifts a process literal out of the argument.
    resources
        .add_module_operation(
            ["processes"],
            "Processes",
            "start",
            "start",
            TypeExpr::Object(vec![lashlang::TypeField {
                name: "definition".into(),
                ty: TypeExpr::Process(lashlang::ProcessType::unknown()),
                optional: false,
            }]),
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    for operation in ["signal", "cancel", "emit"] {
        resources
            .add_module_operation(
                ["processes"],
                "Processes",
                operation,
                operation,
                TypeExpr::Any,
                TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");
    }
    lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::all())
}

mod language_aggregate_await_comprehensions;
mod language_basics;
mod language_control_flow;
mod language_function_declarations;
mod language_resources_and_processes;
mod language_support;

use language_support::expect_string;
