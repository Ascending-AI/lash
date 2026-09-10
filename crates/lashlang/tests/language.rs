use lash_sansio::sync::MutexExt;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Record,
    RuntimeError, State, TypeExpr, Value, parse,
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
            AbilityOp::ResourceOperation(operation) => self
                .perform_resource_operation(operation)
                .await
                .map(AbilityResult::Value),
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = futures::future::join_all(
                    batch
                        .operations
                        .into_iter()
                        .map(|operation| self.perform_resource_operation(operation)),
                )
                .await
                .into_iter()
                .map(lashlang::ResourceOperationResult::from_result)
                .collect();
                Ok(AbilityResult::ResourceOperationBatch(
                    lashlang::ResourceOperationBatchResult::settled_in_input_order(results),
                ))
            }
            // A started process is a real handle record, never its bare
            // result: awaiting a resolved value is a guest error (FIG-2764).
            AbilityOp::StartProcess(start) => {
                let value = self.call_tool(&start.process_name, &start.args).await?;
                let mut handle = Record::new();
                handle.insert("__handle__".to_string(), Value::String("process".into()));
                handle.insert("value".to_string(), value);
                Ok(AbilityResult::Value(Value::Record(Arc::new(handle))))
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
    lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::all())
}

fn program_len(program: &lashlang::Program) -> usize {
    match &program.main {
        lashlang::Expr::Block(expressions) => expressions.len(),
        _ => 1,
    }
}

async fn runtime_error(source: &str) -> RuntimeError {
    let host = TestHost::default();
    let mut state = State::new();
    match execute(source, &mut state, &host)
        .await
        .expect_err("execution should fail")
    {
        ExecuteError::Runtime(error) => error,
        ExecuteError::Parse(error) => panic!("expected runtime error, got parse error: {error:?}"),
        ExecuteError::Link(error) => panic!("expected runtime error, got link error: {error:?}"),
    }
}

mod language_aggregate_await_comprehensions;
mod language_basics;
mod language_control_flow;
mod language_function_declarations;
mod language_resources_and_processes;
mod language_support;

use language_support::expect_string;
