//! Native operation fixture for Deferred ownership.
use super::*;
use lash_core::facade_support::{PluginOperation, PluginSpec, PluginTask, SessionParam};
use lash_core::plugin::{PluginOperationOutcome, PluginTaskContext};
use lash_core::tool_dispatch::{
    RunCoordinator, SingletonBodyOutcome, SingletonToolCall, SingletonToolHandlers,
};
use lash_core::tool_run::{SegmentOrdinal, ToolDeclaration};
use std::sync::Arc;

pub(super) const DEFERRED: &str = "e2e.h3.deferred";

pub(super) fn register(
    spec: PluginSpec,
    materials: Arc<dyn lash_core::store::ToolMaterialStore>,
) -> PluginSpec {
    spec.with_plugin_task_typed::<Deferred, _, _>(move |ctx, label| {
        let materials = materials.clone();
        async move {
            let call = call(&ctx, &label, ToolDeclaration::deferring())?;
            let token = ctx.cancellation_token.clone();
            let handlers = Pending(
                Echo {
                    output: label,
                    cancelled: Arc::new(move || token.is_cancelled()),
                },
                materials,
            );
            let mut run = RunCoordinator::open(
                &ctx.scoped_effect_controller,
                call.owner.clone(),
                call.segment,
                call.available.clone(),
            );
            let decided = run
                .start_round(
                    std::slice::from_ref(&call),
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::new(handlers),
                    Default::default(),
                )
                .await
                .map_err(|error| error.to_string())?;
            if decided.is_empty() {
                while run
                    .progress()
                    .await
                    .map_err(|error| error.to_string())?
                    .is_none()
                {}
            }
            run.await_deferred()
                .await
                .map_err(|error| error.to_string())?;
            let terminals = run.drain().await.map_err(|error| error.to_string())?;
            run.close().await.map_err(|error| error.to_string())?;
            let output = terminals
                .into_iter()
                .find_map(|(_, terminal)| match terminal {
                    lash_core::tool_dispatch::SingletonTerminal::Final { capture, .. } => {
                        capture.output().map(str::to_owned)
                    }
                    _ => None,
                })
                .ok_or_else(|| "H3 Deferred completed without its retained result".to_owned())?;
            let output: String =
                serde_json::from_str(&output).map_err(|error| error.to_string())?;
            Ok(PluginOperationOutcome::new(output))
        }
    })
}

fn call(
    ctx: &PluginTaskContext,
    label: &str,
    declaration: ToolDeclaration,
) -> Result<SingletonToolCall, String> {
    let lash_core::ExecutionScope::SessionOperation {
        session_id,
        operation_id,
    } = ctx.scoped_effect_controller.execution_scope()
    else {
        return Err("H3 fixture was not admitted as an operation Run".into());
    };
    let revision = PluginRevision::new(PLUGIN, BehaviorRevision::ONE);
    let callback = PluginCallbackIdentity {
        owner: revision.clone(),
        key: format!("tool:{label}"),
    };
    Ok(SingletonToolCall {
        owner: lash_core::EffectOpener::session_operation(session_id.clone(), operation_id.clone()),
        segment: SegmentOrdinal(0),
        call_id: lash_core::ToolCallId::derive(
            "",
            lash_core::ToolCallRoot::host_submission(operation_id)
                .map_err(|error| error.to_string())?,
            &[],
        ),
        tool_name: format!("h3.{label}"),
        arguments: serde_json::Value::Null,
        declaration,
        binding: AdmittedBinding {
            executable: callback.clone(),
            preparation: callback,
            presentation: PresentationBinding {
                presenter: None,
                steps: Vec::new(),
            },
        },
        available: vec![revision],
        cancel: ExternalCancelPolicy::Ignore,
        environment: None,
    })
}

struct Deferred;
macro_rules! task {
    ($ty:ty, $name:expr, $args:ty) => {
        impl PluginOperation for $ty {
            const NAME: &'static str = $name;
            const DESCRIPTION: &'static str = "H3 native Run fixture";
            const SESSION_PARAM: SessionParam = SessionParam::Required;
            type Args = $args;
            type Output = String;
            type Error = String;
            const ERROR_TYPE: &'static str = $name;
            /// The fixture operation returns a Serde string as its typed error.
            /// version_surface = "coexist"
            /// version_guard(items(Error))
            const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
            fn error_class(_: &String) -> lash_core::plugin::PluginFailureClass {
                lash_core::plugin::PluginFailureClass::Terminal
            }
        }
        impl PluginTask for $ty {}
    };
}
task!(Deferred, DEFERRED, String);

/// The source's retained result is read from the serving store set.
struct Pending(Echo, Arc<dyn lash_core::store::ToolMaterialStore>);
#[lash_core::async_trait]
impl SingletonToolHandlers for Pending {
    fn tool_material_store(&self) -> Option<&dyn lash_core::store::ToolMaterialStore> {
        Some(self.1.as_ref())
    }
    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        self.0.prepare(call).await
    }
    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        self.0.before_checks(call, request).await
    }
    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        Ok(SingletonBodyOutcome::Deferred {
            source: attempt
                .completion_key
                .cloned()
                .ok_or_else(|| "Run did not arm H3's completion source".to_owned())?,
        })
    }
    async fn after_checks(
        &self,
        call: &lash_core::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        self.0.after_checks(call, capture).await
    }
    async fn run_cancel_requested(&self) -> Result<bool, String> {
        self.0.run_cancel_requested().await
    }

    async fn realize_declarations(
        &self,
        call: &lash_core::ToolCallId,
        intents: &[lash_core::ToolIntentKind],
    ) -> Result<(), String> {
        self.0.realize_declarations(call, intents).await
    }
    async fn present(
        &self,
        call: &lash_core::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        self.0.present(call, capture).await
    }
    fn emit_stream(
        &self,
        call: &lash_core::ToolCallId,
        stream: &lash_core::runtime::AttemptStream,
    ) {
        self.0.emit_stream(call, stream);
    }
    async fn launch_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<lash_core::ProcessId, String> {
        self.0.launch_start(obligation).await
    }
    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process: &lash_core::ProcessId,
        cancel: bool,
    ) -> Result<(), String> {
        self.0.discharge_start(obligation, process, cancel).await
    }
}

pub(super) mod isolated;
