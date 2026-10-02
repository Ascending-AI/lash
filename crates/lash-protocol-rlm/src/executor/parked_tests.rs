use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_lashlang_runtime::LashlangSurface;

use super::RlmExecutionState;
use super::host_bridge::{HostBridge, HostBridgeConfig};

#[derive(Debug)]
pub(crate) struct ParkedCellEvidence {
    pub(crate) finish: serde_json::Value,
    pub(crate) continuation_bytes: usize,
    pub(crate) closure_root: bool,
}

struct ParkedCellHost<'run> {
    bridge: HostBridge<'run>,
}

impl lashlang::ExecutionHost for ParkedCellHost<'_> {
    fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> impl std::future::Future<
        Output = Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError>,
    > + Send {
        self.bridge.perform(op)
    }

    fn execution_mode(&self) -> lashlang::ExecutionMode {
        lashlang::ExecutionMode::Process
    }

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        self.bridge.cancel_checkpoint(checkpoint).await;
    }
}

pub(crate) struct ParkToolProvider;

pub(crate) fn park_tool_definition() -> lash_core::ToolDefinition {
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};

    lash_core::ToolDefinition::raw(
        "tool:cell_park",
        "cell_park",
        "Test-only effect used to park a cell continuation.",
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "number" } },
            "required": ["value"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "number" }),
    )
    .with_tool_binding(ToolBinding::new(["cell"], "park"))
}

pub(crate) fn parked_cell_context_for_tests<'run>(
    ports: impl Into<lash_core::testing::TestExecutionPorts<'run>>,
) -> lash_core::RuntimeExecutionContext<'run> {
    lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
        ports,
        Arc::new(ParkToolProvider),
        lash_core::ToolCatalog::from_tool_definitions(vec![park_tool_definition()]),
    )
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ParkToolProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![park_tool_definition().manifest]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "cell_park").then(|| Arc::new(park_tool_definition().contract))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(call.args["value"].clone()) })
            .await
            .into()
    }
}

pub(crate) async fn execute_parked_cell_for_tests(
    state: &mut RlmExecutionState,
    ctx: lash_core::RuntimeExecutionContext<'_>,
    language: &str,
    code: &str,
    break_retention: bool,
) -> Result<ParkedCellEvidence, String> {
    use lash_vm_broker::{BrokeredEnd, CodeCallIdentities};
    use lash_vm_protocol::{
        FrameEpoch, OpaqueVmState, ProgramSource, StartState, VmOwner, VmStateKind,
    };
    if language != "typescript" {
        return Err(format!("unsupported parked-cell dialect {language}"));
    }
    let service = state.vm.state().service().clone();
    let mut host_environment = LashlangSurface::default()
        .host_environment(ctx.tool_catalog().as_ref())
        .map_err(|e| e.to_string())?;
    host_environment =
        host_environment.with_globals(state.vm.state().binding_names().map(str::to_string));
    let cell = Arc::new(super::cell_run::CellRun::open(&ctx));
    let identities: CodeCallIdentities = cell
        .as_ref()
        .as_ref()
        .map_err(|e| e.to_string())?
        .identities()
        .code()
        .clone();
    let host = ParkedCellHost {
        bridge: HostBridge::new(HostBridgeConfig {
            cell,
            ctx,
            prints: Arc::new(std::sync::Mutex::new(Vec::new())),
            lashlang_execution_trace: None,
            host_environment: host_environment.clone(),
            deferred_execution_grants: BTreeMap::new(),
            cell_bindings: Default::default(),
            workers: service.clone(),
            artifact_store: crate::testing::sqlite_memory_artifact_store().await,
        }),
    };
    let owner = VmOwner::new("parked-cell-witness");
    let start = state
        .vm
        .state()
        .bytes()
        .map(|bytes| {
            StartState::Snapshot(OpaqueVmState::seal(
                VmStateKind::Snapshot,
                owner.clone(),
                lashlang::vm_contract_versions(),
                bytes.to_vec(),
            ))
        })
        .unwrap_or(StartState::Fresh);
    let context = lash_vm_client::RunContext {
        environment: host_environment,
        mode: lashlang::ExecutionMode::Process,
        ..Default::default()
    };
    let run = |start, boundary| lash_lashlang_runtime::WorkerRun {
        service: &service,
        host: &host,
        identities: identities.clone(),
        owner: owner.clone(),
        frame_epoch: FrameEpoch(0),
        program: ProgramSource::Source {
            dialect: language.into(),
            text: code.into(),
        },
        context: context.clone(),
        projected: Default::default(),
        bounds: lashlang::ExecutionBounds::new(
            lashlang::ExecutionBound::Unbounded,
            lashlang::ExecutionBound::Unbounded,
        ),
        state: start,
        boundary,
    };
    let BrokeredEnd::Suspended { checkpoint } = run(start, &|| true)
        .run()
        .await
        .map_err(|e| e.to_string())?
    else {
        return Err("parked cell did not stop at its tool effect".into());
    };
    let (wire, closure_root) = match service
        .request_accounted(lash_vm_client::service::Request::ContinuationProbe {
            bytes: checkpoint.vm.bytes().to_vec(),
            remove_first_reference: break_retention,
        })
        .await
        .map_err(|e| e.to_string())?
    {
        lash_vm_client::service::Response::ContinuationProbe {
            bytes,
            closure_root,
        } => (bytes, closure_root),
        other => return Err(format!("unexpected continuation probe: {other:?}")),
    };
    if !closure_root {
        return Err("parked continuation did not retain a closure root".into());
    }
    let bytes = wire.len();
    let resumed = OpaqueVmState::seal(
        VmStateKind::Continuation,
        owner.clone(),
        lashlang::vm_contract_versions(),
        wire,
    );
    let finish = match run(StartState::Continuation(resumed), &|| false)
        .run()
        .await
        .map_err(|e| e.to_string())?
    {
        BrokeredEnd::Complete { value, .. } => {
            // The process-mode witness owns a copy of the session globals.
            // Its terminal state does not replace the RLM session state.
            match rmp_serde::from_slice::<lashlang::ExecutionOutcome>(&value.0)
                .map_err(|e| e.to_string())?
            {
                lashlang::ExecutionOutcome::Finished(value) => {
                    crate::projection::flow_to_json_value(&value)
                }
                other => return Err(format!("parked cell resumed to {other:?}")),
            }
        }
        BrokeredEnd::GuestError { error, .. } => {
            return Err(rmp_serde::from_slice::<lashlang::RuntimeFailure>(&error.0)
                .map_err(|e| e.to_string())?
                .error
                .to_string());
        }
        other => return Err(format!("parked cell resumed to {other:?}")),
    };
    if break_retention {
        return Err("broken continuation unexpectedly resumed successfully".into());
    }
    Ok(ParkedCellEvidence {
        finish,
        continuation_bytes: bytes,
        closure_root,
    })
}
