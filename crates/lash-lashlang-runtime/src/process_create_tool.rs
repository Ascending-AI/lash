//! `processes.create` — the declaring leaf tool that makes a process
//! definition from source (FIG-3116).
//!
//! The attempt compiles: it lowers the source through the caller's dialect,
//! links it against the tools the caller can call, and answers the definition
//! value of the one process the module exports. That is pure (ADR 0093), so a
//! redriven attempt answers the same value. It publishes nothing (ADR 0116):
//! it declares a [`ToolIntent::PublishDefinition`] with no name that
//! carries the compiled module, and realization publishes the module under the
//! realizing execution's journal referrer behind the attempt's commit. The
//! definition is then an RLM value like any other: the frame whose global
//! binds it holds its module, a `continue_as` seed carries it, and the frame's
//! end or its session's deletion releases it (ADR 0113 §3.1, §6).

use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use serde_json::Value;

use lash_core::{
    AttemptContext, ToolAttemptOutcome, ToolCall, ToolDefinition, ToolIntent, ToolIntents,
    ToolOutcome, ToolOutcomeDone,
};
use lash_tool_support::{StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt};

use crate::LashlangSurface;

/// The `processes.create` tool definition.
#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
pub fn process_create_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:create_process",
        "create_process",
        "Create a process definition from source text and return it. The source defines exactly one process; the returned definition can be passed to `processes.start` or a trigger target, and it lasts as long as the variable that holds it.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Source text that defines exactly one process.",
                },
                "dialect": {
                    "type": "string",
                    "description": "Language the source is written in: the language this session's code is written in.",
                },
            },
            "required": ["source", "dialect"],
            "additionalProperties": false
        }),
        serde_json::json!({
            "x-lash": { "kind": "process_unknown" },
            "description": "The created process definition.",
        }),
    ).expect("valid declared tool schemas")
    .with_declaration(
        lash_core::ToolDeclaration::default()
            .with_intents([lash_core::ToolIntentKind::PublishDefinition]),
    )
    .with_tool_binding(lash_core::ToolBinding::new(["processes"], "create"))
}

/// The provider a dialect plugin registers to put `processes.create` on the
/// tool surface: `dialect` is the language id the tool accepts, and `surface`
/// is the host surface the worker links against.
pub fn process_create_tool_provider(
    dialect: &'static str,
    surface: LashlangSurface,
    workers: lash_vm_client::service::Service,
) -> StaticToolProvider<ProcessCreateTools> {
    StaticToolProvider::new(
        vec![process_create_tool_definition()],
        ProcessCreateTools {
            dialect,
            workers,
            surface,
        },
    )
}

pub struct ProcessCreateTools {
    dialect: &'static str,
    surface: LashlangSurface,
    workers: lash_vm_client::service::Service,
}

#[async_trait::async_trait]
impl StaticToolExecute for ProcessCreateTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let recovery = match self
            .workers
            .begin_execution(call.context.execution_scope_id())
            .await
        {
            Ok(recovery) => recovery,
            Err(error) => return worker_failure(error),
        };
        let scoped = ProcessCreateTools {
            workers: recovery.service().clone(),
            surface: self.surface.clone(),
            dialect: self.dialect,
        };
        let outcome = execute_process_create_tool_call(call.context, call.args, &scoped).await;
        match recovery.settle().await {
            Ok(()) => outcome,
            Err(error) => worker_failure(error),
        }
    }
}

/// A created definition: the value the attempt answers and the declaration
/// realization runs.
struct CreatedDefinition {
    draft: lash_core::ProcessDefinitionDraft,
    module: lash_core::DeclaredModuleArtifact,
}

async fn execute_process_create_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
    tools: &ProcessCreateTools,
) -> ToolAttemptOutcome {
    let created =
        match create_definition(context.tool_catalog().map(AsRef::as_ref), args, tools).await {
            Ok(created) => created,
            Err(CreateDefinitionError::Worker(error)) => return worker_failure(error),
            Err(refusal) => return refuse(refusal),
        };
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(lash_sansio::handle::definition_slot_json(0)),
        ToolIntents::v3(vec![ToolIntent::PublishDefinition(Box::new(
            lash_core::PublishDefinitionIntent {
                owner: context.owner().runtime_owner(),
                draft: created.draft,
                module: Some(created.module),
            },
        ))]),
    )
}

#[derive(Debug, thiserror::Error)]
enum CreateDefinitionError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Compile(#[from] lashlang::ModuleCompileError),
    #[error(transparent)]
    Worker(#[from] lash_vm_client::PoolError),
}

impl From<String> for CreateDefinitionError {
    fn from(message: String) -> Self {
        Self::Refused(message)
    }
}

async fn create_definition(
    catalog: Option<&lash_core::ToolCatalog>,
    args: &Value,
    tools: &ProcessCreateTools,
) -> Result<CreatedDefinition, CreateDefinitionError> {
    let fields = args
        .as_object()
        .ok_or_else(|| "create requires an object".to_string())?;
    if let Some(key) = fields
        .keys()
        .find(|key| !["source", "dialect"].contains(&key.as_str()))
    {
        return Err(format!("create unknown field `{key}`").into());
    }
    let source = required_string(args, "source")?;
    let dialect = required_string(args, "dialect")?;
    if dialect != tools.dialect {
        return Err(format!(
            "create_process cannot compile `{dialect}` source: this session's dialect is `{}`",
            tools.dialect
        )
        .into());
    }
    let empty = lash_core::ToolCatalog::default();
    let environment = tools
        .surface
        .host_environment(catalog.unwrap_or(&empty))
        .map_err(|error| format!("invalid lashlang host tool surface: {error}"))?;
    match tools
        .workers
        .request_accounted(lash_vm_client::service::Request::CreateDefinition {
            source: source.into(),
            environment,
        })
        .await
        .map_err(CreateDefinitionError::Worker)?
    {
        lash_vm_client::service::Response::Definition(created) => Ok(CreatedDefinition {
            draft: created.draft,
            module: created.module,
        }),
        lash_vm_client::service::Response::CompileRefused { error, .. } => Err(error.into()),
        lash_vm_client::service::Response::Refused { message, .. } => Err(message.into()),
        _ => Err(lash_vm_client::PoolError::breach(
            lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
        )
        .into()),
    }
}

fn worker_failure(error: lash_vm_client::PoolError) -> ToolAttemptOutcome {
    let error = error.into_runtime_error();
    if error.is_terminal() {
        return refuse(error.message);
    }
    ToolAttemptOutcome::host_failed(
        lash_core::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation(),
    )
}

fn required_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("create_process requires a non-empty `{field}`"))
}

fn refuse(message: impl std::fmt::Display) -> ToolAttemptOutcome {
    match ToolOutcome::err_fmt(format_args!("{message}")) {
        ToolOutcome::Done(output) => {
            ToolAttemptOutcome::done_without_intents(ToolOutcomeDone::from_output(*output))
        }
        ToolOutcome::Pending(_) => unreachable!("err_fmt always produces a done outcome"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const DIALECT: &str = "typescript";
    const ANSWER: &str = "const answer = async (): Promise<number> => { return 42; };";
    const ECHO: &str = "const relay = async () => { return await demo.echo({text: 'hi'}); };";
    fn tools() -> ProcessCreateTools {
        ProcessCreateTools {
            dialect: DIALECT,
            surface: LashlangSurface::default(),
            workers: lash_vm_client::service::Service::default(),
        }
    }
    fn args(dialect: &str) -> Value {
        serde_json::json!({ "source": ANSWER, "dialect": dialect })
    }
    fn echo_catalog() -> lash_core::ToolCatalog {
        lash_core::ToolCatalog::from_tool_definitions(vec![
            ToolDefinition::raw(
                "tool:demo_echo",
                "demo_echo",
                "Echo text.",
                serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                serde_json::json!({ "type": "string" }),
            )
            .expect("valid declared tool schemas")
            .with_tool_binding(lash_core::ToolBinding::new(["demo"], "echo")),
        ])
    }

    fn refusal(outcome: ToolAttemptOutcome) -> String {
        let ToolAttemptOutcome::Done { result, intents } = outcome else {
            panic!("expected a done attempt");
        };
        assert!(intents.is_empty(), "a refused create declares nothing");
        format!("{result:?}")
    }

    #[tokio::test]
    async fn create_declares_publication_after_its_attempt_commit() {
        let context = lash_core::testing::mock_attempt_context();
        let ToolAttemptOutcome::Done { result, intents } =
            execute_process_create_tool_call(&context, &args(DIALECT), &tools()).await
        else {
            panic!("done attempt")
        };
        let lash_core::ToolCallOutcome::Success(value) = result.into_output().outcome else {
            panic!("publication slot")
        };
        assert_eq!(
            value.to_json_value(),
            lash_sansio::handle::definition_slot_json(0)
        );
        let [ToolIntent::PublishDefinition(intent)] = intents.intents.as_slice() else {
            panic!("one publication")
        };
        let module = intent.module.as_ref().expect("compiled module travels");
        let artifact =
            lashlang::ModuleArtifact::from_store_bytes(module.bytes.as_bytes()).expect("module");
        let descriptor =
            lashlang::ProcessDefinitionIdentity::from_process_value(intent.draft.value().as_json())
                .expect("normalized descriptor");
        assert!(descriptor.matches_artifact_export(&artifact));
        assert_eq!(module.module_ref, artifact.module_ref().as_str());
        assert_eq!(
            intent.draft.artifacts(),
            &[lash_core::ArtifactName {
                store: lash_core::ArtifactStoreId::LashlangModule,
                artifact_ref: module.module_ref.clone()
            }]
        );
        let encoded = serde_json::to_value(intent).expect("intent");
        for key in ["name", "label", "expected_revision", "process_name"] {
            assert!(encoded.get(key).is_none());
        }
        for key in [
            "name",
            "label",
            "replace",
            "expected_revision",
            "engine",
            "args",
        ] {
            let mut extra = args(DIALECT);
            extra[key] = serde_json::json!("forbidden");
            assert!(
                create_definition(None, &extra, &tools()).await.is_err(),
                "{key}"
            );
        }
    }

    /// A process that calls a tool links against the catalog the attempt was
    /// dispatched with, and is refused without it.
    #[tokio::test]
    async fn create_links_against_the_dispatch_catalog() {
        let created = create_definition(
            Some(&echo_catalog()),
            &serde_json::json!({"source":ECHO,"dialect":DIALECT}),
            &tools(),
        )
        .await
        .unwrap_or_else(|error| panic!("links against the catalog: {error}"));
        assert_eq!(
            lashlang::ModuleArtifact::from_store_bytes(created.module.bytes.as_bytes())
                .expect("module")
                .exports()
                .processes
                .len(),
            1
        );
        let error = create_definition(
            None,
            &serde_json::json!({"source":ECHO,"dialect":DIALECT}),
            &tools(),
        )
        .await
        .err()
        .expect("an empty catalog cannot link the tool call");
        assert!(
            error.to_string().contains("unknown module `demo`"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn create_refuses_another_dialect_and_declares_nothing() {
        let context = lash_core::testing::mock_attempt_context();
        let rendered =
            refusal(execute_process_create_tool_call(&context, &args("lua"), &tools()).await);
        assert!(
            rendered.contains("cannot compile `lua` source"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn create_refuses_source_without_exactly_one_process() {
        let context = lash_core::testing::mock_attempt_context();
        let rendered = refusal(
            execute_process_create_tool_call(
                &context,
                &serde_json::json!({"source":"let x = 1;","dialect":DIALECT}),
                &tools(),
            )
            .await,
        );
        assert!(rendered.contains("exactly one process"), "{rendered}");
        let rendered = refusal(
            execute_process_create_tool_call(
                &context,
                &serde_json::json!({"source":"(","dialect":DIALECT}),
                &tools(),
            )
            .await,
        );
        assert!(
            rendered.contains("source")
                || rendered.contains("expected")
                || rendered.contains("Expected"),
            "{rendered}"
        );
        let rendered = refusal(
            execute_process_create_tool_call(
                &context,
                &serde_json::json!({ "dialect": DIALECT }),
                &tools(),
            )
            .await,
        );
        assert!(rendered.contains("non-empty `source`"), "{rendered}");
    }
}
