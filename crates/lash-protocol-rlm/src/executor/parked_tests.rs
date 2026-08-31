use std::collections::BTreeMap;
use std::sync::Arc;

use lash_lashlang_runtime::LashlangSurface;

use crate::dialect::SourceDialect;

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
        Output = Result<lashlang::AbilityResult, lashlang::ExecutionHostError>,
    > + Send {
        self.bridge.perform(op)
    }

    fn execution_mode(&self) -> lashlang::ExecutionMode {
        lashlang::ExecutionMode::Process
    }

    async fn yield_now(&self) {
        self.bridge.yield_now().await;
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

pub(crate) fn parked_cell_context_for_tests() -> lash_core::RuntimeExecutionContext<'static> {
    lash_core::testing::code_execution_context_with_tool_provider_and_catalog(
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

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolOutcome {
        lash_core::ToolOutcome::ok(call.args["value"].clone())
    }
}

pub(crate) async fn execute_parked_cell_for_tests(
    state: &mut RlmExecutionState,
    ctx: lash_core::RuntimeExecutionContext<'static>,
    language: &str,
    code: &str,
    break_retention: bool,
) -> Result<ParkedCellEvidence, String> {
    use lashlang::{CompilationDialect, GlobalPatch, Vm, VmRunOutcome};

    let mut host_environment = LashlangSurface::default()
        .host_environment(ctx.tool_catalog().as_ref())
        .map_err(|error| error.to_string())?;
    let live_global_names = state
        .rlm
        .globals()
        .keys()
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    host_environment = host_environment.with_globals(live_global_names);

    let cached_program = match language {
        "lashlang" => state
            .linked_programs
            .get_or_compile(code, &host_environment)
            .map_err(|error| error.to_string())?,
        "typescript" => match state.linked_programs.cached_linked_program(
            code,
            &host_environment,
            CompilationDialect::Typescript,
        ) {
            Some(program) => program,
            None => {
                let program = lash_typescript::parse_with_globals(code, &host_environment.globals)
                    .map_err(|error| error.to_string())?;
                state
                    .linked_programs
                    .get_or_compile_ast(
                        code,
                        program,
                        &host_environment,
                        CompilationDialect::Typescript,
                    )
                    .map_err(|error| error.to_string())?
            }
        },
        other => return Err(format!("unsupported parked-cell dialect {other}")),
    };
    let linked_module = cached_program.linked_module();
    let bridge = HostBridge::new(HostBridgeConfig {
        ctx,
        language_id: SourceDialect::Typescript.language_id(),
        print_projector: Arc::new(crate::rlm_support::print_history_projector()),
        tool_result_projectors: Vec::new(),
        lashlang_execution_trace: None,
        host_environment,
        deferred_execution_grants: BTreeMap::new(),
        artifact_store: lashlang::global_in_memory_lashlang_artifact_store(),
        trigger_key_manifest: linked_module.artifact.trigger_key_manifest.clone(),
        initial_observations: Vec::new(),
    });
    let host = ParkedCellHost { bridge };
    let mut vm = Vm::from_state(cached_program.compiled_program(), &mut state.rlm, &host)
        .map_err(|error| error.to_string())?;
    let parked = vm
        .run_process_until_effect()
        .await
        .map_err(|error| error.to_string())?;
    if !matches!(parked, VmRunOutcome::EffectCompleted) {
        return Err(format!(
            "parked cell did not stop at its tool effect: {parked:?}"
        ));
    }
    let continuation = vm.suspend().map_err(|error| error.to_string())?;
    let mut wire = serde_json::to_vec(&continuation).map_err(|error| error.to_string())?;
    let closure_root = continuation
        .operand_stack
        .iter()
        .chain(continuation.slots.iter().flatten())
        .any(|value| matches!(value, lashlang::Value::Ref(_)));
    if !closure_root {
        return Err("parked continuation did not retain a closure root".to_string());
    }
    if break_retention {
        let mut broken = continuation.clone();
        let root = broken
            .operand_stack
            .iter_mut()
            .chain(broken.slots.iter_mut().flatten())
            .find(|value| matches!(value, lashlang::Value::Ref(_)))
            .expect("closure root checked above");
        *root = lashlang::Value::Null;
        wire = serde_json::to_vec(&broken).map_err(|error| error.to_string())?;
    }
    drop(vm);

    let restored: lashlang::VmContinuation =
        serde_json::from_slice(&wire).map_err(|error| error.to_string())?;
    let mut vm = Vm::resume_from(restored, cached_program.compiled_program(), &host)
        .map_err(|error| error.to_string())?;
    let finish = loop {
        match vm
            .run_process_until_effect()
            .await
            .map_err(|error| error.to_string())?
        {
            VmRunOutcome::EffectCompleted => continue,
            VmRunOutcome::Complete(lashlang::ExecutionOutcome::Finished(value)) => {
                break crate::projection::flow_to_json_value(&value).await;
            }
            VmRunOutcome::Complete(other) => {
                return Err(format!("parked cell resumed to {other:?}"));
            }
        }
    };
    let globals = vm.into_globals().map_err(|error| error.to_string())?;
    let existing = state
        .rlm
        .globals()
        .keys()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let retained = globals
        .keys()
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    let mut patches = existing
        .into_iter()
        .filter(|name| !retained.contains(name))
        .map(|name| GlobalPatch::Remove { name })
        .collect::<Vec<_>>();
    patches.extend(globals.iter().map(|(name, value)| GlobalPatch::Insert {
        name: name.to_string(),
        value: value.clone(),
    }));
    state
        .rlm
        .patch_globals(patches)
        .map_err(|error| error.to_string())?;
    if break_retention {
        return Err("broken continuation unexpectedly resumed successfully".to_string());
    }
    Ok(ParkedCellEvidence {
        finish,
        continuation_bytes: wire.len(),
        closure_root,
    })
}
