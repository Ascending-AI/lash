//! `processes.create` — the declaring leaf tool that makes a process
//! definition from source (FIG-3116).
//!
//! The attempt compiles: it lowers the source through the caller's dialect,
//! links it against the tools the caller can call, and answers the definition
//! value of the one process the module exports. That is pure (ADR 0093), so a
//! redriven attempt answers the same value. It publishes nothing (ADR 0116):
//! it declares a [`ToolIntent::RegisterProcessDefinition`] with no name that
//! carries the compiled module, and realization publishes the module under the
//! realizing execution's journal referrer behind the attempt's commit. The
//! definition is then an RLM value like any other: the frame whose global
//! binds it holds its module, a `continue_as` seed carries it, and the frame's
//! end or its session's deletion releases it (ADR 0113 §3.1, §6).

use serde_json::Value;

use lash_core::{
    AttemptContext, SessionId, ToolAttemptOutcome, ToolCall, ToolDefinition, ToolIntent,
    ToolIntents, ToolOutcome, ToolOutcomeDone,
};
use lash_tool_support::{StaticToolExecute, StaticToolProvider, ToolDefinitionBindingExt};

use crate::{LASHLANG_ENGINE_KIND, LashlangSurface};

/// Lowers source text in one dialect to a lashlang program, answering the
/// rendered refusal on failure. The dialect front end owns the parse (ADR
/// 0096); this tool owns the link and the declaration.
pub type ProcessSourceParser = fn(&str) -> Result<lashlang::Program, String>;

/// The `processes.create` tool definition.
pub fn process_create_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
        "tool:create_process",
        "create_process",
        "Create a process definition from source text and return it. The source defines exactly one process; the returned definition can be passed to `processes.start`, `processes.register` or a trigger target, and it lasts as long as the variable that holds it.",
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
    )
    .with_tool_binding(lash_core::ToolBinding::new(["processes"], "create"))
}

/// The provider a dialect plugin registers to put `processes.create` on the
/// tool surface: `dialect` is the language id the tool accepts, `parse` its
/// front end, and `surface` the lashlang host surface a cell of that dialect
/// links against.
pub fn process_create_tool_provider(
    dialect: &'static str,
    parse: ProcessSourceParser,
    surface: LashlangSurface,
) -> StaticToolProvider<ProcessCreateTools> {
    StaticToolProvider::new(
        vec![process_create_tool_definition()],
        ProcessCreateTools {
            dialect,
            parse,
            surface,
        },
    )
}

pub struct ProcessCreateTools {
    dialect: &'static str,
    parse: ProcessSourceParser,
    surface: LashlangSurface,
}

#[async_trait::async_trait]
impl StaticToolExecute for ProcessCreateTools {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        execute_process_create_tool_call(call.context, call.args, self)
    }
}

/// A created definition: the value the attempt answers and the declaration
/// realization runs.
struct CreatedDefinition {
    definition: Value,
    process_name: String,
    module: lash_core::DeclaredModuleArtifact,
}

fn execute_process_create_tool_call(
    context: &AttemptContext<'_>,
    args: &Value,
    tools: &ProcessCreateTools,
) -> ToolAttemptOutcome {
    let created = match create_definition(context.tool_catalog().map(AsRef::as_ref), args, tools) {
        Ok(created) => created,
        Err(message) => return refuse(message),
    };
    ToolAttemptOutcome::done(
        ToolOutcomeDone::ok(created.definition.clone()),
        ToolIntents::v3(vec![ToolIntent::RegisterProcessDefinition(Box::new(
            lash_core::RegisterProcessDefinitionIntent {
                session_id: SessionId::from(context.session_id()),
                engine_kind: LASHLANG_ENGINE_KIND.to_string(),
                definition: created.definition,
                env_spec: None,
                label: Some(created.process_name),
                name: None,
                expected_revision: None,
                module: Some(created.module),
            },
        ))]),
    )
}

fn create_definition(
    catalog: Option<&lash_core::ToolCatalog>,
    args: &Value,
    tools: &ProcessCreateTools,
) -> Result<CreatedDefinition, String> {
    let source = required_string(args, "source")?;
    let dialect = required_string(args, "dialect")?;
    if dialect != tools.dialect {
        return Err(format!(
            "create_process cannot compile `{dialect}` source: this session's dialect is `{}`",
            tools.dialect
        ));
    }
    let program = (tools.parse)(source)?;
    let empty = lash_core::ToolCatalog::default();
    let environment = tools
        .surface
        .host_environment(catalog.unwrap_or(&empty))
        .map_err(|error| format!("invalid lashlang host tool surface: {error}"))?;
    let compiled = lashlang::compile_module(lashlang::ModuleCompileRequest {
        source,
        program,
        environment: &environment,
    })
    .map_err(|error| error.diagnostic().to_string())?;
    let artifact = compiled.artifact;
    let mut processes = artifact.exports().processes.keys();
    let (Some(process_name), None) = (processes.next(), processes.next()) else {
        return Err(format!(
            "create_process needs source that defines exactly one process; it defines {}",
            artifact.exports().processes.len()
        ));
    };
    let identity =
        lashlang::ProcessDefinitionIdentity::from_artifact_export(&artifact, process_name)
            .ok_or_else(|| format!("process `{process_name}` is not exported by its module"))?;
    let bytes = artifact
        .to_store_bytes()
        .map_err(|error| format!("failed to encode the process module: {error}"))?;
    let bytes = String::from_utf8(bytes)
        .map_err(|error| format!("the process module did not encode as text: {error}"))?;
    Ok(CreatedDefinition {
        definition: identity.to_process_value(),
        process_name: process_name.clone(),
        module: lash_core::DeclaredModuleArtifact {
            module_ref: artifact.module_ref().as_str().to_owned(),
            bytes,
        },
    })
}

fn required_string<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
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
    use lashlang::testing::ast_builders as b;

    const DIALECT: &str = "test-dialect";

    fn answer_program(_source: &str) -> Result<lashlang::Program, String> {
        Ok(crate::lib_tests::process_module(
            "answer",
            Vec::new(),
            lashlang::TypeExpr::Float,
            b::num(42.0),
        ))
    }

    /// A process whose body calls a catalog tool: it links only against a
    /// catalog that has the tool.
    fn echo_program(_source: &str) -> Result<lashlang::Program, String> {
        Ok(crate::lib_tests::process_module(
            "relay",
            Vec::new(),
            lashlang::TypeExpr::Any,
            b::module_call(
                &["demo"],
                "echo",
                vec![b::record(vec![("text", b::string("hi"))])],
            ),
        ))
    }

    fn no_process(_source: &str) -> Result<lashlang::Program, String> {
        Ok(b::program(Vec::new()))
    }

    fn refused(_source: &str) -> Result<lashlang::Program, String> {
        Err("the dialect refused this source".to_string())
    }

    fn tools(parse: ProcessSourceParser) -> ProcessCreateTools {
        ProcessCreateTools {
            dialect: DIALECT,
            parse,
            surface: LashlangSurface::default(),
        }
    }

    fn args(dialect: &str) -> Value {
        serde_json::json!({ "source": "one process", "dialect": dialect })
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

    /// The attempt answers the definition value of the module's one process
    /// and declares an unnamed registration carrying exactly that module's
    /// store bytes: realization publishes what the value names, and nothing
    /// else (FIG-3116).
    #[test]
    fn create_declares_an_unnamed_registration_carrying_the_compiled_module() {
        let context = lash_core::testing::mock_attempt_context();
        let outcome =
            execute_process_create_tool_call(&context, &args(DIALECT), &tools(answer_program));
        let ToolAttemptOutcome::Done { result, intents } = outcome else {
            panic!("expected a done attempt");
        };
        let answered = match result.into_output().outcome {
            lash_core::ToolCallOutcome::Success(value) => value.to_json_value(),
            other => panic!("expected the created definition, got {other:?}"),
        };
        let identity = lashlang::ProcessDefinitionIdentity::from_process_value(&answered)
            .expect("the answer is a process definition value");
        assert_eq!(identity.process_name, "answer");
        let [ToolIntent::RegisterProcessDefinition(intent)] = intents.intents.as_slice() else {
            panic!(
                "expected one registration intent, got {:?}",
                intents.intents
            );
        };
        assert_eq!(intent.name, None, "create registers no name");
        assert_eq!(intent.expected_revision, None);
        assert_eq!(intent.engine_kind, LASHLANG_ENGINE_KIND);
        assert_eq!(intent.definition, answered);
        let module = intent.module.as_ref().expect("the module travels");
        assert_eq!(module.module_ref, identity.module_ref.as_str());
        let artifact = lashlang::ModuleArtifact::from_store_bytes(module.bytes.as_bytes())
            .expect("the declared bytes decode as the module");
        assert_eq!(artifact.module_ref(), &identity.module_ref);
        assert!(identity.matches_artifact_export(&artifact));
    }

    /// A process that calls a tool links against the catalog the attempt was
    /// dispatched with, and is refused without it.
    #[test]
    fn create_links_against_the_dispatch_catalog() {
        let created =
            create_definition(Some(&echo_catalog()), &args(DIALECT), &tools(echo_program))
                .unwrap_or_else(|error| panic!("links against the catalog: {error}"));
        assert_eq!(created.process_name, "relay");
        let error = create_definition(None, &args(DIALECT), &tools(echo_program))
            .err()
            .expect("an empty catalog cannot link the tool call");
        assert!(error.contains("unknown module `demo`"), "{error}");
    }

    #[test]
    fn create_refuses_another_dialect_and_declares_nothing() {
        let context = lash_core::testing::mock_attempt_context();
        let rendered = refusal(execute_process_create_tool_call(
            &context,
            &args("lua"),
            &tools(answer_program),
        ));
        assert!(
            rendered.contains("cannot compile `lua` source"),
            "{rendered}"
        );
    }

    #[test]
    fn create_refuses_source_without_exactly_one_process() {
        let context = lash_core::testing::mock_attempt_context();
        let rendered = refusal(execute_process_create_tool_call(
            &context,
            &args(DIALECT),
            &tools(no_process),
        ));
        assert!(rendered.contains("exactly one process"), "{rendered}");
        let rendered = refusal(execute_process_create_tool_call(
            &context,
            &args(DIALECT),
            &tools(refused),
        ));
        assert!(rendered.contains("the dialect refused"), "{rendered}");
        let rendered = refusal(execute_process_create_tool_call(
            &context,
            &serde_json::json!({ "dialect": DIALECT }),
            &tools(answer_program),
        ));
        assert!(rendered.contains("non-empty `source`"), "{rendered}");
    }

    #[test]
    fn create_tool_definition_binds_processes_create() {
        let definition = process_create_tool_definition();
        assert_eq!(definition.name(), "create_process");
        assert_eq!(
            definition
                .manifest
                .bindings
                .get(lash_tool_support::TYPESCRIPT_TOOL_BINDING_KEY),
            Some(
                &serde_json::to_value(lash_core::ToolBinding::new(["processes"], "create"))
                    .expect("binding serializes")
            ),
        );
        let input = &definition.contract.input_schema.canonical;
        assert_eq!(input["required"], serde_json::json!(["source", "dialect"]));
        assert_eq!(
            definition.contract.output_schema.canonical["x-lash"],
            serde_json::json!({ "kind": "process_unknown" })
        );
    }
}
