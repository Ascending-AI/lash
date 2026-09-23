use lash::sync::MutexExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use lash::rlm::{
    LanguageTraceHost,
    lang::{
        AbilityOp, AbilityResult, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
        LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures,
        LinkedModule, OperationContract, ResourceOperation, ResourceOperationBatchResult,
        ResourceOperationResult, Sleep, State, Value, WorkflowGraph, compile_linked_process,
        from_json,
    },
};
use lash::tracing::TraceLanguageExecutionPayload;
use tokio::sync::mpsc;

use crate::{DisplayDelta, DisplayState, RunEvent, RunStatus};

#[derive(Clone, Copy, Debug)]
pub struct RunTiming {
    pub sleep_cap: Duration,
    pub signal_delay: Duration,
}

impl Default for RunTiming {
    fn default() -> Self {
        Self {
            sleep_cap: Duration::from_secs(2),
            signal_delay: Duration::from_millis(650),
        }
    }
}

pub(crate) struct PreparedRun {
    compiled: lash::rlm::lang::CompiledProgram,
    workflow_version: u64,
    run_id: String,
}

impl PreparedRun {
    pub(crate) fn new(graph: WorkflowGraph, source: &str, workflow_version: u64) -> Result<Self> {
        let program = lash::typescript::parse(source).context("parse saved workflow")?;
        let linked = LinkedModule::link(program, host_environment()).context("link toy tools")?;
        let process_name = graph
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                lash::rlm::lang::WorkflowDeclaration::Process(process) => {
                    Some(process.name.as_str())
                }
                _ => None,
            })
            .ok_or_else(|| anyhow!("saved workflow has no process to run"))?;
        let compiled = compile_linked_process(&linked, process_name)
            .context("compile saved workflow process")?;
        Ok(Self {
            compiled,
            workflow_version,
            run_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub(crate) async fn execute(self, sender: mpsc::Sender<RunEvent>, timing: RunTiming) {
        let host = LanguageTraceHost::new(
            RunHost::new(sender, self.run_id, self.workflow_version, timing),
            RunHost::project_language_trace,
        );
        let environment = ExecutionEnvironment::new(&host).process();
        let result =
            lash::rlm::lang::execute(&self.compiled, &mut State::new(), &environment).await;
        if let Err(error) = result {
            host.host().emit_current_failure(error.to_string());
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "each mock operation registers exactly once under its own module and host \
              operation keys, so the catalog adds cannot conflict"
)]
pub(crate) fn host_environment() -> LashlangHostEnvironment {
    let mut catalog = LashlangHostCatalog::new();
    for operation in crate::display::OPERATIONS {
        let properties = operation
            .fields
            .iter()
            .map(|field| {
                let schema = match (operation.operation, field.name, field.field_type) {
                    ("add_item", "item", _) | ("set_light", "state", _) => {
                        serde_json::json!({ "type": ["string", "number", "boolean"] })
                    }
                    (_, _, "number") => serde_json::json!({ "type": "number" }),
                    _ => serde_json::json!({ "type": "string" }),
                };
                (field.name.to_string(), schema)
            })
            .collect::<serde_json::Map<_, _>>();
        let required = operation
            .fields
            .iter()
            .map(|field| field.name)
            .collect::<Vec<_>>();
        catalog
            .add_module_operation_contract(
                ["display"],
                "ToyDisplay",
                operation.operation,
                operation.operation,
                &OperationContract::new(
                    serde_json::json!({
                        "type": "object",
                        "properties": properties,
                        "required": required,
                        "additionalProperties": false
                    }),
                    serde_json::json!({ "type": "null" }),
                ),
            )
            .expect("host catalog operation must not conflict");
    }
    for operation in crate::mock_tools::OPERATIONS {
        let contract = match operation.output_from_input() {
            Some((input_field, default_schema)) => OperationContract::from_input_field(
                operation.input_schema(),
                input_field,
                default_schema,
            ),
            None => OperationContract::new(operation.input_schema(), operation.output_schema()),
        };
        catalog
            .add_module_operation_contract(
                [operation.module],
                operation.resource_type,
                operation.operation,
                operation.host_operation,
                &contract,
            )
            .expect("host catalog operation must not conflict");
    }
    LashlangHostEnvironment::new(catalog, LashlangAbilities::all())
        .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
}

struct RunHost {
    sender: mpsc::Sender<RunEvent>,
    run_id: String,
    workflow_version: u64,
    sequence: AtomicU64,
    display: Mutex<DisplayState>,
    pending_delta: Mutex<DisplayDelta>,
    current_node: Mutex<Option<String>>,
    timing: RunTiming,
}

impl RunHost {
    fn new(
        sender: mpsc::Sender<RunEvent>,
        run_id: String,
        workflow_version: u64,
        timing: RunTiming,
    ) -> Self {
        Self {
            sender,
            run_id,
            workflow_version,
            sequence: AtomicU64::new(0),
            display: Mutex::new(DisplayState::default()),
            pending_delta: Mutex::new(DisplayDelta::default()),
            current_node: Mutex::new(None),
            timing,
        }
    }

    fn emit(&self, node_id: String, status: RunStatus, delta: DisplayDelta, error: Option<String>) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let display = self.display.lock_recover().clone();
        let _ = self.sender.try_send(RunEvent {
            run_id: self.run_id.clone(),
            workflow_version: self.workflow_version,
            sequence,
            node_id,
            status,
            display_delta: delta,
            display,
            error,
        });
    }

    fn emit_waiting(&self) {
        if let Some(node_id) = self.current_node.lock_recover().clone() {
            self.emit(node_id, RunStatus::Waiting, DisplayDelta::default(), None);
        }
    }

    fn emit_current_failure(&self, error: String) {
        if let Some(node_id) = self.current_node.lock_recover().clone() {
            let delta = std::mem::take(&mut *self.pending_delta.lock_recover());
            self.emit(node_id, RunStatus::Failed, delta, Some(error));
        }
    }

    fn apply_operation(&self, operation: ResourceOperation) -> Result<Value, ExecutionHostError> {
        if crate::mock_tools::is_operation(&operation.receiver, &operation.operation) {
            return crate::mock_tools::apply_tool(
                &operation.receiver,
                &operation.operation,
                &operation.args,
            );
        }
        let mut display = self.display.lock_recover();
        let (value, delta) =
            crate::display::apply_tool(&mut display, &operation.operation, &operation.args)?;
        self.pending_delta.lock_recover().merge(delta);
        Ok(value)
    }

    async fn perform_sleep(&self, sleep: Sleep) -> Result<AbilityResult, ExecutionHostError> {
        self.emit_waiting();
        let requested = duration_from_value(&sleep.value)?;
        tokio::time::sleep(requested.min(self.timing.sleep_cap)).await;
        Ok(AbilityResult::Value(Value::Null))
    }

    fn project_language_trace(&self, payload: TraceLanguageExecutionPayload) {
        match payload {
            TraceLanguageExecutionPayload::NodeStarted { node_id, .. } => {
                *self.current_node.lock_recover() = Some(node_id.clone());
                self.emit(node_id, RunStatus::Started, DisplayDelta::default(), None);
            }
            TraceLanguageExecutionPayload::NodeCompleted { node_id, .. } => {
                let delta = std::mem::take(&mut *self.pending_delta.lock_recover());
                self.emit(node_id.clone(), RunStatus::Succeeded, delta, None);
                let mut current = self.current_node.lock_recover();
                if current.as_deref() == Some(node_id.as_str()) {
                    *current = None;
                }
            }
            TraceLanguageExecutionPayload::NodeFailed {
                node_id, failure, ..
            } => {
                let delta = std::mem::take(&mut *self.pending_delta.lock_recover());
                self.emit(
                    node_id,
                    RunStatus::Failed,
                    delta,
                    Some(failure.message().to_owned()),
                );
            }
            TraceLanguageExecutionPayload::BranchSelected { node_id, .. } => {
                self.emit(
                    node_id.clone(),
                    RunStatus::Started,
                    DisplayDelta::default(),
                    None,
                );
                self.emit(node_id, RunStatus::Succeeded, DisplayDelta::default(), None);
            }
            TraceLanguageExecutionPayload::ChildStarted { .. }
            | TraceLanguageExecutionPayload::NodeWaiting { .. }
            | TraceLanguageExecutionPayload::NodeResumed { .. }
            | TraceLanguageExecutionPayload::NodeCancelled { .. }
            | TraceLanguageExecutionPayload::ExecutionStarted { .. }
            | TraceLanguageExecutionPayload::ExecutionFinished { .. } => {}
        }
    }
}

impl ExecutionHost for RunHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                self.apply_operation(*operation).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = batch
                    .operations
                    .into_iter()
                    .map(|operation| {
                        ResourceOperationResult::from_result(self.apply_operation(operation))
                    })
                    .collect();
                Ok(AbilityResult::ResourceOperationBatch(
                    ResourceOperationBatchResult::settled_in_input_order(results),
                ))
            }
            AbilityOp::Sleep(sleep) => self.perform_sleep(sleep).await,
            AbilityOp::WaitSignal { name, .. } => {
                self.emit_waiting();
                tokio::time::sleep(self.timing.signal_delay).await;
                Ok(AbilityResult::Value(from_json(serde_json::json!({
                    "name": name,
                    "autoFired": true
                }))))
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "the toy workflow host does not support this ability",
            )),
        }
    }
}

fn duration_from_value(value: &Value) -> Result<Duration, ExecutionHostError> {
    match value {
        Value::Number(milliseconds) if milliseconds.is_finite() && *milliseconds >= 0.0 => {
            Ok(Duration::from_secs_f64(*milliseconds / 1_000.0))
        }
        Value::String(value) => parse_duration(value),
        _ => Err(ExecutionHostError::new(
            "sleep duration must be non-negative milliseconds or a string ending in ms or s",
        )),
    }
}

fn parse_duration(value: &str) -> Result<Duration, ExecutionHostError> {
    if let Some(milliseconds) = value.strip_suffix("ms") {
        return milliseconds
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(|value| Duration::from_secs_f64(value / 1_000.0))
            .ok_or_else(|| ExecutionHostError::new(format!("invalid duration `{value}`")));
    }
    if let Some(seconds) = value.strip_suffix('s') {
        return seconds
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .map(Duration::from_secs_f64)
            .ok_or_else(|| ExecutionHostError::new(format!("invalid duration `{value}`")));
    }
    Err(ExecutionHostError::new(format!(
        "invalid duration `{value}`"
    )))
}
