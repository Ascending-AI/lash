mod namespace;

mod artifact;
mod ast;
mod builtins;
mod compile;
mod identifier;
mod identity;
mod introspection;
mod json_schema;
mod linker;
mod runtime;
mod span;
mod tracking;
mod trigger;
mod typed_output;
mod workflow_graph;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use artifact::{
    ArtifactPublicationPause, ArtifactStoreError, ContentHash, DurabilityTier, HostRequirements,
    HostRequirementsRef, InMemoryLashlangArtifactStore, LASHLANG_COMPILER_VERSION,
    LASHLANG_SEMANTIC_HASH_VERSION, LASHLANG_VM_ABI_VERSION, LashlangArtifactStore, ModuleArtifact,
    ModuleArtifactError, ModuleExports, ModuleRef, ProcessRef, canonical_program_ir,
    global_in_memory_lashlang_artifact_store, host_requirements_for_program,
};
pub use ast::{
    AssignPathStep, AssignTarget, BinaryOp, CatchClause, Declaration, Expr, ExprFolder,
    ExprVisitor, ExpressionSourceSpan, FunctionDecl, FunctionExpr, FunctionParam, InvalidAst,
    JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, LIFTED_PROCESS_NAME_PREFIX,
    LabelMetadata, ListComprehensionClause, MAX_AST_NESTING_DEPTH, NestingTooDeep, ProcessDecl,
    ProcessLiteralExpr, ProcessParam, ProcessSignalDecl, ProcessSignature, ProcessSignatureError,
    ProcessType, Program, ResourceRefExpr, TryExpr, TypeDecl, TypeExpr, TypeField, UnaryOp,
    UnionMembers, check_ast_nesting_depth, fold_expr_children, format_type_expr,
    lifted_process_identity, validate_ast, walk_expr,
};

/// Names of every source Lashlang builtin, in registry order.
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
    referenced_module_call_paths, referenced_receiver_call_paths,
};
pub use json_schema::{
    JsonSchemaError, X_LASH_KEYWORD, XLashParam, XLashSignature, XLashType,
    json_schema_to_type_expr, type_expr_to_json_schema,
};
pub use lash_sansio::MediaType;
pub use linker::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostCatalogError, LashlangHostEnvironment,
    LashlangLanguageFeatures, LinkError, LinkedModule, NamedDataType, NamedDataTypeError,
    OperationContract, OutputFromInputBinding, ResolvedOperation, ResourceOperationBinding,
    ResourceTypeCatalog, TriggerSourceBinding, ValueConstructorBinding,
};
pub use runtime::{
    AbilityOp, AbilityResult, BudgetedJsonProjectionConfig, BudgetedJsonProjector, CompileStats,
    CompiledLinkedProgram, CompiledProcessCache, CompiledProcessCacheKey, CompiledProgram,
    CompiledProgramCache, CompiledProgramCacheStats, ContinuationError, ErrorTaxonomy,
    ExecutableProgram, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionMode, ExecutionOutcome, ExecutionScratch, FormatError,
    GlobalPatch, GlobalPatchOutcome, HeapId, ImageValue, LASH_HOST_DESCRIPTOR_TYPE_KEY,
    LASH_HOST_DESCRIPTOR_VALUE_KEY, LASH_HOST_REQUIREMENTS_REF_KEY, LASH_MODULE_REF_KEY,
    LASH_PROCESS_NAME_KEY, LASH_PROCESS_REF_KEY, LASH_PROCESS_VALUE_KEY, LASH_TYPE_KEY,
    LASHLANG_SNAPSHOT_VERSION, LinkedProgramCache, LinkedProgramCacheError, ListValue,
    ProcessEvent, ProcessEventKind, ProcessSignal, ProcessStart, ProfileReport, ProfileStat,
    ProjectedBindingError, ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor,
    ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, Record, ResourceHandle,
    ResourceOperation, ResourceOperationBatch, ResourceOperationBatchResult,
    ResourceOperationResult, RuntimeError, RuntimeFailure, Sleep, SleepKind, Snapshot,
    SnapshotDecodeError, State, VM_CONTINUATION_FORMAT_VERSION, Value, ValueProjectionContext,
    ValueProjector, Vm, VmContinuation, VmFinallyCompletionContinuation, VmFinallyContinuation,
    VmHandlerContinuation, VmHeapContinuation, VmIteratorContinuation, VmIteratorCursor,
    VmPendingErrorOriginContinuation, VmProfileContinuation, VmRunOutcome, compile_ast,
    compile_linked, compile_linked_process, compile_module_artifact_process, compile_process,
    execute, from_json, is_process_handle, prewarm, unwrap_type_value,
};
pub use runtime::{
    CANONICAL_MESSAGEPACK_DEPTH_LIMIT, CanonicalMapOrder, CanonicalPathSegment,
    TYPESCRIPT_REGEXP_EXECUTION_FUEL, TYPESCRIPT_REGEXP_FUEL_PER_INSTRUCTION,
    TYPESCRIPT_REGEXP_MAX_NESTING, TYPESCRIPT_REGEXP_MAX_PATTERN_CODE_UNITS,
    TypeScriptRegExpValidationError, validate_canonical_messagepack_structure,
    validate_typescript_regexp, validate_typescript_regexp_shape,
};
pub use runtime::{
    DEFAULT_HEAP_LOGICAL_BYTE_LIMIT, HEAP_GC_ALLOCATION_INTERVAL, HEAP_SIZE_SCHEDULE_VERSION,
};
pub use runtime::{DEFAULT_HOST_MEMORY_LIMIT_BYTES, DEFAULT_MAX_VM_FRAME_DEPTH};
pub use span::Span;

/// Version of the compiled bytecode contract used for durable continuations.
/// Increment whenever identical source/artifact identities may compile to a
/// continuation-incompatible instruction stream.
pub const BYTECODE_FORMAT_VERSION: u32 = 17;
pub use tracking::{
    LashlangBranchSite, LashlangExecutionCallSite, LashlangExecutionChild,
    LashlangExecutionObservation, LashlangExecutionSite, ProcessBranchSelection,
    WorkflowExecutionSite, process_ref_key,
};
pub use trigger::{
    HostDescriptor, HostDescriptorError, LASH_TRIGGER_EVENT_KEY, TRIGGER_MODULE_ALIAS,
    TRIGGER_REGISTRATION_TYPE_NAME, TriggerCompatibility, TriggerCompatibilityError,
    TriggerCompatibilityRequest, TriggerHostOperation, TriggerInputBinding, TriggerInputTemplate,
    TriggerListRequest, TriggerPruneRequest, TriggerRegistrationRequest,
    add_trigger_resource_operations, check_trigger_compatibility, event_type_for_source,
    is_resolved_type_assignable, is_trigger_resource_type, list_call_args, register_call_args,
    trigger_event_placeholder_expr,
};
pub use typed_output::{OutputSchemaError, parse_output_schema};
pub use workflow_graph::{
    VariableVersion, WORKFLOW_GRAPH_SCHEMA_VERSION, WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
    WorkflowContainer, WorkflowDeclaration, WorkflowEdge, WorkflowEdgeKind, WorkflowEffectKind,
    WorkflowExpectedArgument, WorkflowGraph, WorkflowListComprehensionClause, WorkflowNode,
    WorkflowNodeId, WorkflowNodeKind, WorkflowNodeNameSource, WorkflowNodeTypeFacets,
    WorkflowProcess, WorkflowSubgraph, WorkflowTerminalKind, WorkflowTypeDiagnostic,
    WorkflowTypedVariable, child_path, execution_sites, node_id_for_execution_site,
    projected_node_type_facets, runtime_execution_site_for_workflow_site,
};

/// Internals the workflow-graph projector needs.
///
/// The projector lives in `lash-typescript` because every text surface it owns
/// is TypeScript, but the classification it reads — purity, execution-site
/// descriptors, link facts — is language-independent and stays here.
pub use linker::{WorkflowLinkAnalysis, analyze_workflow_program};
pub use runtime::{
    RESOURCE_OPERATION_EXECUTION_SITE_KIND, execution_site_descriptor, is_pure_expr,
};

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

fn runtime_hint(error: &RuntimeError) -> Option<&'static str> {
    match error {
        RuntimeError::UnwrappedToolResultFailed { .. } => {
            Some("remove `?` and inspect `.ok` or `.error` when you need to handle failures")
        }
        RuntimeError::AwaitExpectsHandle { .. } => {
            Some("value is already resolved; remove `await`, or await the call directly")
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
    use crate::testing::ast_builders as b;

    /// `finish <value>`
    fn finish_number(value: f64) -> Program {
        b::program(vec![b::finish(b::num(value))])
    }

    /// `finish (await tools.read_file({ path: "." }))?`
    fn read_file_program() -> Program {
        b::program(vec![b::finish(b::module_call(
            &["tools"],
            "read_file",
            vec![b::record(vec![("path", b::string("."))])],
        ))])
    }

    /// `finish persisted` — links only where `persisted` is a live global.
    fn finish_persisted() -> Program {
        b::program(vec![b::finish(b::var("persisted"))])
    }

    /// A program no host environment links, used where a cache hit must not
    /// reach the linker at all.
    fn unlinkable_program() -> Program {
        b::program(vec![b::finish(b::var("never_bound_anywhere"))])
    }

    /// Two statements, with the second one's span stated as the whole of the
    /// second line of `source`.
    ///
    /// The runtime blames a failure on the span table the front-end supplied,
    /// so a test that pins `--> line 2, column 1` has to state the offsets its
    /// rendering is read against (ADR 0096: nothing in this crate parses).
    fn second_line_program(first: Expr, second: Expr, source: &str) -> Program {
        let first_line = source
            .split('\n')
            .next()
            .expect("the witness has two lines")
            .len();
        let program = b::with_source_spans(
            b::program(vec![first, second]),
            &[(&[1], first_line + 1, source.len())],
        );
        // The VM reads the per-statement table, so the statement spans are
        // stated alongside the expression one the renderer reads.
        b::with_expression_spans(program, &[(0, first_line), (first_line + 1, source.len())])
    }

    struct Host;

    impl ExecutionHost for Host {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) if operation.operation == "anything" => {
                    Ok(AbilityResult::Value(Value::Record(std::sync::Arc::new(
                        Record::from_iter([("ok".to_string(), Value::Bool(true))]),
                    ))))
                }
                AbilityOp::ResourceOperationBatch(batch) => {
                    Ok(AbilityResult::ResourceOperationBatch(
                        ResourceOperationBatchResult::settled_in_input_order(
                            batch
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
                        ),
                    ))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Ok(AbilityResult::Value(Value::Null)),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_reports_runtime_errors() {
        // finish missing
        let compiled = compile_ast(&b::program(vec![b::finish(b::var("missing"))]))
            .expect("the program should compile");
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
    fn scalar_type_keyword_in_value_position_has_type_literal_hint() {
        // `finish { value: str }`: in value position `str` is an ordinary name,
        // and the linker has no binding for it.
        let source = "finish { value: str }";
        let program =
            crate::testing::ast_builders::program(vec![crate::testing::ast_builders::finish(
                crate::testing::ast_builders::record(vec![(
                    "value",
                    crate::testing::ast_builders::var("str"),
                )]),
            )]);
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
        let compiled = compile_ast(&second_line_program(
            b::assign("x", b::num(1.0)),
            b::finish(b::var("missing")),
            source,
        ))
        .expect("the program should compile");
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
        // finish 7
        let compiled = compile_ast(&b::program(vec![b::finish(b::num(7.0))]))
            .expect("the program should compile");
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
        let first = cache.get_or_compile_ast("finish 1", finish_number(1.0));
        let second = cache.get_or_compile_ast("finish 1", finish_number(1.0));
        let same_ast = cache.get_or_compile_ast("finish 1\n", finish_number(1.0));
        let other = cache.get_or_compile_ast("finish 2", finish_number(2.0));
        let third = cache.get_or_compile_ast("finish 3", finish_number(3.0));

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
            .get_or_compile_ast(source, read_file_program(), &base_environment)
            .expect("link first program");
        let second = cache
            .get_or_compile_ast(source, read_file_program(), &base_environment)
            .expect("reuse same surface");
        let extra = cache
            .get_or_compile_ast(source, read_file_program(), &extra_environment)
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
    fn linked_program_cache_hit_does_not_relink_the_program() {
        let source = r#"finish (await tools.read_file({ path: "." }))?"#;
        let environment = LashlangHostEnvironment::new(
            LashlangHostCatalog::tool_default(["read_file"]),
            LashlangAbilities::default(),
        );
        let mut cache = LinkedProgramCache::with_capacity(2);

        let first = cache
            .get_or_compile_ast(source, read_file_program(), &environment)
            .expect("link first program");

        // A hit is served without touching the program it is handed, which is
        // the work this cache exists to skip. Handing it a program that cannot
        // link proves the hit never reaches the linker.
        let second = cache
            .get_or_compile_ast(source, unlinkable_program(), &environment)
            .expect("reuse the cached linked program");

        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn linked_program_cache_hit_serves_an_already_parsed_program_without_the_ast() {
        let source = "finish 1";
        let environment =
            LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::default());
        let mut cache = LinkedProgramCache::with_capacity(2);

        // The cache is keyed by the source text; what it stores is the linked
        // AST, so the program is built directly.
        let first = cache
            .get_or_compile_ast(
                source,
                crate::testing::ast_builders::program(vec![crate::testing::ast_builders::finish(
                    crate::testing::ast_builders::num(1.0),
                )]),
                &environment,
            )
            .expect("link first program");

        let cached = cache
            .cached_linked_program(source, &environment)
            .expect("the linked program is cached");

        assert!(std::sync::Arc::ptr_eq(&first, &cached));
        assert_eq!(cache.stats().hits, 1);
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
            .get_or_compile_ast(source, read_file_program(), &base_environment)
            .expect("link first program");
        let newline = cache
            .get_or_compile_ast(
                &format!("{source}\n"),
                read_file_program(),
                &base_environment,
            )
            .expect("link source-distinct program");
        let changed = cache
            .get_or_compile_ast(source, read_file_program(), &changed_environment)
            .expect("link changed surface requirement");
        let missing = cache
            .get_or_compile_ast(source, read_file_program(), &missing_environment)
            .expect_err("missing resource operation should not reuse cached program");

        assert!(!std::sync::Arc::ptr_eq(&first, &newline));
        assert!(!std::sync::Arc::ptr_eq(&first, &changed));
        assert_ne!(
            first.linked_module().host_requirements_ref,
            changed.linked_module().host_requirements_ref
        );
        assert!(matches!(
            missing,
            LinkError::UnknownResourceOperation { operation, .. } if operation == "read_file"
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
            .get_or_compile_ast(source, finish_persisted(), &available)
            .expect("live global should link");
        assert_eq!(
            linked.linked_module().artifact.host_requirements.globals,
            ["persisted".to_string()].into_iter().collect()
        );
        let error = cache
            .get_or_compile_ast(source, finish_persisted(), &missing)
            .expect_err("cache hit must not bypass current globals");
        assert!(matches!(
            error,
            LinkError::UnknownName { name, .. } if name == "persisted"
        ));
        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().misses, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn execute_with_diagnostics_covers_representative_runtime_failures() {
        let cases: [(&str, Expr, Expr, &str, &str); 4] = [
            (
                "x = 1\nfinish ({ ok: false, error: \"boom\" })?",
                b::assign("x", b::num(1.0)),
                b::finish(b::unwrap(b::record(vec![
                    ("ok", b::bool_lit(false)),
                    ("error", b::string("boom")),
                ]))),
                "`?` unwrapped failed tool result: boom",
                "finish ({ ok: false, error: \"boom\" })?",
            ),
            (
                "x = 1\nfinish len(true)",
                b::assign("x", b::num(1.0)),
                b::finish(b::builtin("len", vec![b::bool_lit(true)])),
                "`len` requires a string, tuple, list, record, or null",
                "finish len(true)",
            ),
            // Reading an absent property off a string or a number is
            // `undefined`, not a failure (ADR 0096); null and undefined are
            // the values that still refuse to be read through.
            (
                "x = null\nfinish x.field",
                b::assign("x", b::null()),
                b::finish(b::field(b::var("x"), "field")),
                "can't read `.field` from null",
                "finish x.field",
            ),
            (
                "x = null\nfinish x[0]",
                b::assign("x", b::null()),
                b::finish(b::index(b::var("x"), b::num(0.0))),
                "can't index null",
                "finish x[0]",
            ),
        ];

        for (source, first, second, expected_error, expected_snippet) in cases {
            let compiled = compile_ast(&second_line_program(first, second, source))
                .expect("the program should compile");
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
            // v = await tools.anything({})?
            // finish v
            b::program(vec![
                b::assign("v", b::module_call(&["tools"], "anything", vec![])),
                b::finish(b::var("v")),
            ]),
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
        // finish null
        let compiled = compile_ast(&b::program(vec![b::finish(b::null())]))
            .expect("the program should compile");
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
