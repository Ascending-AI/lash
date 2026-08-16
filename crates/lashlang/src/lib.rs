mod artifact;
mod ast;
mod builtins;
mod compile;
mod identity;
mod introspection;
mod json_schema;
mod lexer;
mod linker;
mod parser;
mod runtime;
mod source;
mod tracking;
mod trigger;
mod trigger_manifest;
mod typed_output;
mod workflow_graph;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use artifact::{
    ArtifactStoreError, ContentHash, DurabilityTier, HostRequirements, HostRequirementsRef,
    InMemoryLashlangArtifactStore, LASHLANG_COMPILER_VERSION, LASHLANG_SEMANTIC_HASH_VERSION,
    LASHLANG_VM_ABI_VERSION, LashlangArtifactStore, ModuleArtifact, ModuleArtifactError,
    ModuleExports, ModuleRef, ProcessRef, canonical_program_ir,
    global_in_memory_lashlang_artifact_store, host_requirements_for_program,
};
pub use ast::{
    AssignPathStep, AssignTarget, BinaryOp, Declaration, Expr, ExprFolder, ExprVisitor,
    LabelMetadata, ListComprehensionClause, ProcessDecl, ProcessParam, ProcessSignalDecl,
    ProcessStartExpr, Program, ResourceRefExpr, TypeDecl, TypeExpr, TypeField, UnaryOp,
    fold_expr_children, format_type_expr, walk_expr,
};

/// Names of every builtin accepted by the linker and runtime, in registry order.
pub fn builtin_names() -> impl ExactSizeIterator<Item = &'static str> + Clone {
    builtins::names()
}
pub use compile::{
    ModuleCompileDiagnostic, ModuleCompileError, ModuleCompileOutput, ModuleCompileRequest,
    ModuleCompileStage, compile_module,
};
pub use identity::{ProcessDefinitionIdentity, ProcessDefinitionIdentityError};
pub use introspection::{
    ModuleInstanceIntrospection, ModuleIntrospection, ModuleIntrospectionError,
    ModuleOperationIntrospection, NamedDataTypeIntrospection, ProcessInputIntrospection,
    ProcessIntrospection, ProcessSignalIntrospection, ResourceOperationIntrospection,
    ResourceTypeIntrospection, TriggerSourceIntrospection, TypeView, ValueConstructorIntrospection,
    referenced_module_call_paths,
};
pub use json_schema::json_schema_to_type_expr;
pub use lash_sansio::MediaType;
pub use lexer::{LexError, Span, Token, TokenKind, lex};
pub use linker::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostCatalogError, LashlangHostEnvironment,
    LashlangLanguageFeatures, LinkError, LinkedModule, NamedDataType, NamedDataTypeError,
    OutputFromInputBinding, ResourceOperationBinding, ResourceTypeCatalog, TriggerSourceBinding,
    ValueConstructorBinding,
};
pub use parser::{ParseError, parse, parse_expression, parse_type_expression};
pub use runtime::{
    AbilityOp, AbilityResult, BudgetedJsonProjectionConfig, BudgetedJsonProjector, CompileStats,
    CompiledLinkedProgram, CompiledProcessCache, CompiledProcessCacheKey, CompiledProgram,
    CompiledProgramCache, CompiledProgramCacheStats, ContinuationError, ExecutableProgram,
    ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost, ExecutionHostError,
    ExecutionMode, ExecutionOutcome, ExecutionScratch, FormatError, GlobalPatch,
    GlobalPatchOutcome, HeapId, ImageValue, LASH_HOST_DESCRIPTOR_TYPE_KEY,
    LASH_HOST_DESCRIPTOR_VALUE_KEY, LASH_HOST_REQUIREMENTS_REF_KEY, LASH_MODULE_REF_KEY,
    LASH_PROCESS_NAME_KEY, LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY, LASH_TYPE_KEY,
    LASHLANG_SNAPSHOT_VERSION, LinkedProgramCache, LinkedProgramCacheError, ListValue,
    ProcessEvent, ProcessEventKind, ProcessSignal, ProcessStart, ProfileReport, ProfileStat,
    ProjectedBindingError, ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor,
    ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record, ResourceHandle,
    ResourceOperation, ResourceOperationBatch, ResourceOperationBatchResult,
    ResourceOperationResult, RuntimeError, RuntimeFailure, Sleep, SleepKind, Snapshot,
    SnapshotDecodeError, State, Value, ValueProjectionContext, ValueProjector, Vm, VmContinuation,
    VmHeapContinuation, VmIteratorContinuation, VmIteratorCursor, VmProfileContinuation,
    VmRunOutcome, compile, compile_linked, compile_linked_process, compile_module_artifact_process,
    compile_process, execute, from_json, prewarm, unwrap_type_value,
};
#[doc(hidden)]
pub use runtime::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, validate_canonical_messagepack_structure,
};
pub use runtime::{
    DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, HEAP_GC_ALLOCATION_INTERVAL, HEAP_SIZE_SCHEDULE_VERSION,
};

/// Version of the compiled bytecode contract used for durable continuations.
/// Increment whenever identical source/artifact identities may compile to a
/// continuation-incompatible instruction stream.
pub const BYTECODE_FORMAT_VERSION: u32 = 3;
pub use source::{
    CanonicalSourceError, canonical_assign_target_source, canonical_expression_source,
    canonical_process_source, canonical_process_source_with_requirements, canonical_program_source,
    canonical_program_source_with_requirements,
};
pub use tracking::{
    LashlangBranchSite, LashlangExecutionCallSite, LashlangExecutionChild,
    LashlangExecutionObservation, LashlangExecutionSite, ProcessBranchSelection,
    WorkflowExecutionSite, process_ref_key,
};
pub use trigger::{
    HostDescriptor, HostDescriptorError, LASH_TRIGGER_EVENT_KEY, TriggerCompatibility,
    TriggerCompatibilityError, TriggerCompatibilityRequest, TriggerHostOperation,
    TriggerInputBinding, TriggerInputTemplate, TriggerListRequest, TriggerPruneRequest,
    TriggerRegistrationRequest, add_trigger_resource_operations, check_trigger_compatibility,
    event_type_for_source, is_trigger_resource_type, list_call_args, register_call_args,
    trigger_event_placeholder_expr,
};
pub use trigger_manifest::{
    CurrentTriggerKeyManifest, TriggerKeyManifest, TriggerKeyManifestDiff,
    TriggerManifestReplacement,
};
pub use typed_output::{OutputSchemaError, parse_output_schema};
pub use workflow_graph::{
    GraphRenderError, VariableVersion, WORKFLOW_GRAPH_SCHEMA_VERSION,
    WORKFLOW_TYPE_FACET_SCHEMA_VERSION, WorkflowContainer, WorkflowDeclaration, WorkflowEdge,
    WorkflowEdgeKind, WorkflowEffectKind, WorkflowExpectedArgument, WorkflowGraph,
    WorkflowGraphBuildError, WorkflowListComprehensionClause, WorkflowNode, WorkflowNodeId,
    WorkflowNodeKind, WorkflowNodeNameSource, WorkflowNodeTypeFacets, WorkflowProcess,
    WorkflowSubgraph, WorkflowTerminalKind, WorkflowTypeDiagnostic, WorkflowTypedVariable,
    node_id_for_execution_site, runtime_execution_site_for_workflow_site,
    workflow_graph_from_source, workflow_graph_from_source_with_facets, workflow_graph_to_source,
};

pub fn format_parse_diagnostic(source: &str, error: &ParseError) -> String {
    let span = error.span().unwrap_or(Span {
        start: error.offset(),
        end: error.offset(),
    });
    format_source_diagnostic(source, span, &error.to_string(), parse_hint(error))
}

pub fn format_runtime_diagnostic(source: &str, error: &RuntimeError, span: Option<Span>) -> String {
    let Some(span) = span else {
        return format_message_with_hint(&error.to_string(), runtime_hint(error));
    };
    format_source_diagnostic(source, span, &error.to_string(), runtime_hint(error))
}

pub fn format_link_diagnostic(source: &str, error: &LinkError) -> String {
    let hint = link_hint(error);
    match error.span() {
        Some(span) => format_source_diagnostic(source, span, &error.to_string(), hint.as_deref()),
        None => format_message_with_hint(&error.to_string(), hint.as_deref()),
    }
}

fn format_source_diagnostic(source: &str, span: Span, message: &str, hint: Option<&str>) -> String {
    let start = span.start.min(source.len());
    let (line, column, _line_start, line_end, source_line) = line_column_snippet(source, start);
    let caret_pad = " ".repeat(column.saturating_sub(1));
    let underline_len = if start < line_end {
        let underline_end = span.end.max(start.saturating_add(1)).min(line_end);
        source[start..underline_end].chars().count().max(1)
    } else {
        1
    };
    let underline = format!("^{}", "~".repeat(underline_len.saturating_sub(1)));
    let mut diagnostic = format!(
        "{message}\n--> line {line}, column {column}\n{source_line}\n{caret_pad}{underline}"
    );
    if let Some(hint) = hint {
        diagnostic.push_str("\nhint: ");
        diagnostic.push_str(hint);
    }
    diagnostic
}

fn format_message_with_hint(message: &str, hint: Option<&str>) -> String {
    let mut diagnostic = message.to_string();
    if let Some(hint) = hint {
        diagnostic.push_str("\nhint: ");
        diagnostic.push_str(hint);
    }
    diagnostic
}

fn link_hint(error: &LinkError) -> Option<String> {
    if let LinkError::UnknownName { name, .. } = error
        && matches!(name.as_str(), "str" | "int" | "float" | "bool" | "any")
    {
        return Some("types belong in `Type { ... }` literals".to_string());
    }
    let (prefix, suggestions) = match error {
        LinkError::UnknownResourceOperation { suggestions, .. } => {
            ("available operations: ", suggestions)
        }
        LinkError::UnresolvedReceiver { suggestions, .. } => {
            ("use a module authority, e.g. ", suggestions)
        }
        _ => return None,
    };
    let message = error.to_string();
    let suggestions = suggestions
        .iter()
        .filter(|suggestion| !message.contains(suggestion.as_str()))
        .map(|suggestion| format!("`{suggestion}`"))
        .collect::<Vec<_>>();
    (!suggestions.is_empty()).then(|| format!("{prefix}{}", suggestions.join(", ")))
}

fn parse_hint(error: &ParseError) -> Option<&'static str> {
    match error {
        ParseError::Unexpected { found, .. } if found == "`if`" => {
            Some("use `cond ? yes : no` for inline conditionals")
        }
        ParseError::Unexpected { found, .. } if found == "`for`" => Some(
            "`for` is a statement. Put it on its own line, not inside an expression or record literal.",
        ),
        ParseError::DeclarativeTriggerRemoved { .. } => Some(
            "construct a host-provided trigger source value and call the trigger registry register operation",
        ),
        _ => None,
    }
}

fn runtime_hint(error: &RuntimeError) -> Option<&'static str> {
    match error {
        RuntimeError::UnwrappedToolResultFailed { .. } => {
            Some("remove `?` and inspect `.ok` or `.error` when you need to handle failures")
        }
        RuntimeError::ReadOnlyProjectedBinding { .. } => {
            Some("copy the projected value into a new variable before changing it")
        }
        RuntimeError::ValidateTypeLiteralRequired => {
            Some("pass `Type { ... }` or a variable that holds a Type literal")
        }
        _ => None,
    }
}

fn line_column_snippet(source: &str, offset: usize) -> (usize, usize, usize, usize, String) {
    let offset = offset.min(source.len());
    let mut line = 1usize;
    let mut line_start = 0usize;
    for (idx, ch) in source.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = idx + ch.len_utf8();
        }
    }
    let column = source[line_start..offset].chars().count() + 1;
    let line_end = source[offset..]
        .find('\n')
        .map(|rel| offset + rel)
        .unwrap_or(source.len());
    (
        line,
        column,
        line_start,
        line_end,
        source[line_start..line_end]
            .trim_end_matches('\r')
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Host;

    impl ExecutionHost for Host {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) if operation.operation == "anything" => {
                    Ok(AbilityResult::Value(Value::Record(std::sync::Arc::new(
                        Record::from_iter([("ok".to_string(), Value::Bool(true))]),
                    ))))
                }
                AbilityOp::ResourceOperationBatch(batch) => Ok(
                    AbilityResult::ResourceOperationBatch(ResourceOperationBatchResult {
                        results: batch
                            .operations
                            .into_iter()
                            .map(|operation| {
                                if operation.operation == "anything" {
                                    ResourceOperationResult::Value(Value::Record(
                                        std::sync::Arc::new(Record::from_iter([(
                                            "ok".to_string(),
                                            Value::Bool(true),
                                        )])),
                                    ))
                                } else {
                                    ResourceOperationResult::Error(ExecutionHostError::new(
                                        "unsupported host ability",
                                    ))
                                }
                            })
                            .collect(),
                    }),
                ),
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Ok(AbilityResult::Value(Value::Null)),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compile_reports_parse_errors() {
        let err = compile("if true").expect_err("parse should fail");
        assert!(matches!(err, ParseError::Expected { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_reports_runtime_errors() {
        let compiled = compile("finish missing").expect("source should compile");
        let mut state = State::new();
        let err = execute(&compiled, &mut state, &Host)
            .await
            .expect_err("runtime should fail");
        assert!(matches!(err, RuntimeError::UndefinedVariable { .. }));
    }

    #[test]
    fn message_hint_format_preserves_message_and_appends_hint() {
        assert_eq!(
            format_message_with_hint("plain failure", None),
            "plain failure"
        );
        assert_eq!(
            format_message_with_hint("tool failed", Some("inspect `.error`")),
            "tool failed\nhint: inspect `.error`"
        );
    }

    #[test]
    fn removed_declarative_trigger_parse_hint_is_stable() {
        let error = ParseError::DeclarativeTriggerRemoved {
            span: Span { start: 0, end: 8 },
        };
        assert_eq!(
            parse_hint(&error),
            Some(
                "construct a host-provided trigger source value and call the trigger registry register operation"
            )
        );
    }

    #[test]
    fn scalar_type_keyword_in_value_position_has_type_literal_hint() {
        let source = "finish { value: str }";
        let program = crate::parse(source).expect("source should parse");
        let error = crate::LinkedModule::link(program, crate::LashlangHostEnvironment::default())
            .expect_err("scalar type keyword is not a value");
        let diagnostic = format_link_diagnostic(source, &error);
        assert!(diagnostic.contains("unknown name `str`"), "{diagnostic}");
        assert!(
            diagnostic.contains("hint: types belong in `Type { ... }` literals"),
            "{diagnostic}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn traced_environment_records_source_location() {
        let source = "x = 1\nfinish missing";
        let compiled = compile(source).expect("source should compile");
        let mut state = State::new();
        let env = ExecutionEnvironment::new(&Host).traced();
        execute(&compiled, &mut state, &env)
            .await
            .expect_err("runtime should fail");
        let failure = env
            .take_runtime_failure()
            .expect("traced host should receive runtime failure");
        let message = format_runtime_diagnostic(source, &failure.error, failure.span);
        assert!(message.contains("unknown name `missing`"), "{message}");
        assert!(message.contains("--> line 2, column 1"), "{message}");
        assert!(message.contains("finish missing"), "{message}");
        assert!(message.contains("^"), "{message}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn compile_prewarm_and_environment_scratch_execution_work_together() {
        prewarm();
        let compiled = compile("finish 7").expect("source should compile");
        let mut state = State::new();
        let env = ExecutionEnvironment::new(&Host)
            .traced()
            .with_scratch(ExecutionScratch::new());
        let outcome = execute(&compiled, &mut state, &env)
            .await
            .expect("execution should succeed");
        assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(7.0)));
        assert!(env.take_recycled_scratch().is_some());
    }

    #[test]
    fn compiled_program_cache_reuses_source_and_tracks_lru_stats() {
        let mut cache = CompiledProgramCache::with_capacity(2);
        let first = cache.get_or_compile("finish 1").expect("compile first");
        let second = cache.get_or_compile("finish 1").expect("compile cache hit");
        let same_ast = cache
            .get_or_compile("finish 1\n")
            .expect("compile source-distinct program");
        let other = cache.get_or_compile("finish 2").expect("compile second");
        let third = cache.get_or_compile("finish 3").expect("compile third");

        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(!std::sync::Arc::ptr_eq(&first, &same_ast));
        assert!(!std::sync::Arc::ptr_eq(&first, &other));
        assert!(!std::sync::Arc::ptr_eq(&other, &third));

        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 4);
        assert_eq!(stats.evictions, 2);
        assert_eq!(stats.entries, 2);
        assert_eq!(stats.capacity, 2);
    }

    #[test]
    fn linked_program_cache_reuses_source_when_host_environment_satisfies_requirements() {
        let source = r#"finish (await tools.read_file({ path: "." }))?"#;
        let base_environment = LashlangHostEnvironment::new(
            LashlangHostCatalog::tool_default(["read_file"]),
            LashlangAbilities::default(),
        );
        let extra_environment = LashlangHostEnvironment::new(
            LashlangHostCatalog::tool_default(["read_file", "unrelated"]),
            LashlangAbilities::default(),
        );
        let mut cache = LinkedProgramCache::with_capacity(2);

        let first = cache
            .get_or_compile(source, &base_environment)
            .expect("compile first linked program");
        let second = cache
            .get_or_compile(source, &base_environment)
            .expect("reuse same surface");
        let extra = cache
            .get_or_compile(source, &extra_environment)
            .expect("reuse when unrelated tools are added");

        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(std::sync::Arc::ptr_eq(&first, &extra));
        assert_eq!(
            first.linked_module().host_requirements_ref,
            extra.linked_module().host_requirements_ref
        );

        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.entries, 1);
    }

    #[test]
    fn linked_program_cache_keeps_source_and_host_requirements_distinct() {
        let source = r#"finish (await tools.read_file({ path: "." }))?"#;
        let base_environment = LashlangHostEnvironment::new(
            LashlangHostCatalog::tool_default(["read_file"]),
            LashlangAbilities::default(),
        );
        let mut changed_resources = LashlangHostCatalog::new();
        changed_resources
            .add_module_operation(
                ["tools"],
                "Tools",
                "read_file",
                "read_file_v2",
                TypeExpr::Any,
                TypeExpr::Any,
            )
            .expect("host catalog operation must not conflict");
        let changed_environment =
            LashlangHostEnvironment::new(changed_resources, LashlangAbilities::default());
        let missing_environment = LashlangHostEnvironment::new(
            LashlangHostCatalog::tool_default(["echo"]),
            LashlangAbilities::default(),
        );
        let mut cache = LinkedProgramCache::with_capacity(4);

        let first = cache
            .get_or_compile(source, &base_environment)
            .expect("compile first linked program");
        let newline = cache
            .get_or_compile(&format!("{source}\n"), &base_environment)
            .expect("compile source-distinct linked program");
        let changed = cache
            .get_or_compile(source, &changed_environment)
            .expect("compile changed surface requirement");
        let missing = cache
            .get_or_compile(source, &missing_environment)
            .expect_err("missing resource operation should not reuse cached program");

        assert!(!std::sync::Arc::ptr_eq(&first, &newline));
        assert!(!std::sync::Arc::ptr_eq(&first, &changed));
        assert_ne!(
            first.linked_module().host_requirements_ref,
            changed.linked_module().host_requirements_ref
        );
        assert!(matches!(
            missing,
            LinkedProgramCacheError::Link(LinkError::UnknownResourceOperation {
                operation,
                ..
            }) if operation == "read_file"
        ));

        let stats = cache.stats();
        assert_eq!(stats.hits, 0);
        assert_eq!(stats.misses, 4);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.entries, 3);
    }

    #[test]
    fn linked_program_cache_rechecks_required_live_globals() {
        let source = "finish persisted";
        let available = LashlangHostEnvironment::default().with_globals(["persisted"]);
        let missing = LashlangHostEnvironment::default();
        let mut cache = LinkedProgramCache::with_capacity(2);

        let linked = cache
            .get_or_compile(source, &available)
            .expect("live global should link");
        assert_eq!(
            linked.linked_module().artifact.host_requirements.globals,
            ["persisted".to_string()].into_iter().collect()
        );
        let error = cache
            .get_or_compile(source, &missing)
            .expect_err("cache hit must not bypass current globals");
        assert!(matches!(
            error,
            LinkedProgramCacheError::Link(LinkError::UnknownName { name, .. })
                if name == "persisted"
        ));
        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().misses, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_with_diagnostics_covers_representative_runtime_failures() {
        let cases = [
            (
                "x = 1\nfinish ({ ok: false, error: \"boom\" })?",
                "`?` unwrapped failed tool result: boom",
                "finish ({ ok: false, error: \"boom\" })?",
            ),
            (
                "x = 1\nfinish len(true)",
                "`len` requires a string, tuple, list, record, or null",
                "finish len(true)",
            ),
            (
                "x = 1\nfinish \"text\".field",
                "can't read `.field` from string",
                "finish \"text\".field",
            ),
            ("x = 1\nfinish 7[0]", "can't index number", "finish 7[0]"),
        ];

        for (source, expected_error, expected_snippet) in cases {
            let compiled = compile(source).expect("source should compile");
            let mut state = State::new();
            let env = ExecutionEnvironment::new(&Host).traced();
            execute(&compiled, &mut state, &env)
                .await
                .expect_err("runtime should fail");
            let failure = env
                .take_runtime_failure()
                .expect("traced host should receive runtime failure");
            let message = format_runtime_diagnostic(source, &failure.error, failure.span);
            assert!(message.contains(expected_error), "{message}");
            assert!(message.contains("--> line 2, column 1"), "{message}");
            assert!(message.contains(expected_snippet), "{message}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_success_path_uses_host() {
        let linked = LinkedModule::link(
            parse("v = await tools.anything({})? finish v").expect("source should parse"),
            LashlangHostEnvironment::new(
                LashlangHostCatalog::tool_default(["anything"]),
                LashlangAbilities::default(),
            ),
        )
        .expect("source should link");
        let compiled = compile_linked(&linked);
        let mut state = State::new();
        let outcome = execute(&compiled, &mut state, &Host)
            .await
            .expect("should succeed");
        let ExecutionOutcome::Finished(value) = outcome else {
            panic!("expected finish");
        };
        assert_eq!(
            value.as_record().expect("tool result should be record")["ok"],
            Value::Bool(true)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_allows_finish_null() {
        let compiled = compile("finish null").expect("source should compile");
        let mut state = State::new();
        let outcome = execute(&compiled, &mut state, &Host)
            .await
            .expect("should succeed");
        let ExecutionOutcome::Finished(value) = outcome else {
            panic!("expected finish");
        };
        assert_eq!(value, Value::Null);
    }
}
