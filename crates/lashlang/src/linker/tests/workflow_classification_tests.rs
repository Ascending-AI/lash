use super::*;
use crate::{WorkflowDiagnosticKind, WorkflowNodeId, projected_node_type_facets};

fn assert_classified_producer(recovered: bool, with_owner: bool) {
    let fixtures = errors();
    assert_eq!(fixtures.len(), WorkflowDiagnosticKind::ALL.len());
    let expected_kinds = WorkflowDiagnosticKind::ALL
        .into_iter()
        .map(WorkflowDiagnosticKind::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        fixtures
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<BTreeSet<_>>(),
        expected_kinds
    );
    let mut program = Program::block(vec![Expr::Null]);
    program
        .spans
        .insert(AstPath::main(vec![0]), Span { start: 1, end: 3 });
    let environment = LashlangHostEnvironment::new(LashlangHostCatalog::new());
    let path = AstPath::main(Vec::new());
    let owner = AstPath::main(vec![0]);
    let id: WorkflowNodeId =
        serde_json::from_value(serde_json::json!("node-fixture")).expect("node id");
    for (kind, error) in fixtures {
        let mut linker = Linker::new(&program, &environment).with_workflow_analysis();
        let expected_span = if with_owner {
            *linker.workflow_diagnostic_owner.borrow_mut() = Some(owner.clone());
            program.spans.get(&owner).copied().or_else(|| error.span())
        } else {
            error.span()
        };
        let message = error.to_string();
        if recovered {
            linker.record_recovered_workflow_error(&program.main, &path, error);
        } else {
            linker.record_workflow_error(&program.main, &path, error);
        }
        let analysis = linker.take_workflow_analysis();
        let facets = projected_node_type_facets(
            Some(&analysis),
            if with_owner { &owner } else { &path },
            &[],
            &id,
        )
        .expect("producer records node facts");
        assert_eq!(facets.diagnostics.len(), 1, "{kind}");
        let diagnostic = &facets.diagnostics[0];
        assert_eq!(diagnostic.kind.as_str(), kind);
        assert_eq!(diagnostic.node_id, id);
        assert_eq!(diagnostic.message, message);
        assert_eq!(diagnostic.span, expected_span);
        let wire = serde_json::to_value(diagnostic).expect("diagnostic encodes");
        assert_eq!(wire["classification"], "definite", "{kind}");
    }
}

#[test]
fn recorded_workflow_diagnostics_are_definite_for_every_kind() {
    assert_classified_producer(false, false);
}

#[test]
fn recovered_workflow_diagnostics_are_definite_for_every_kind() {
    assert_classified_producer(true, true);
}

fn errors() -> Vec<(&'static str, LinkError)> {
    vec![
        (
            "invalid_ast",
            LinkError::InvalidAst {
                source: crate::ast::InvalidAst::ReturnOutsideFunction,
            },
        ),
        (
            "duplicate_declaration",
            LinkError::DuplicateDeclaration {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "duplicate_process_param",
            LinkError::DuplicateProcessParam {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_process",
            LinkError::UnknownProcess {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_name",
            LinkError::UnknownName {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_builtin",
            LinkError::UnknownBuiltin {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_resource",
            LinkError::UnknownResource {
                path: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_type",
            LinkError::UnknownType {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_constructor_input",
            LinkError::IncompatibleConstructorInput {
                path: "fixture".to_string(),
                expected: "str".to_string(),
                actual: "int".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_operation_input",
            LinkError::IncompatibleOperationInput {
                operation: "fixture".to_string(),
                expected: "str".to_string(),
                actual: "int".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "awaited_settled_expression",
            LinkError::AwaitedSettledExpression {
                actual: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_expected_literal",
            LinkError::IncompatibleExpectedLiteral {
                expected: "str".to_string(),
                actual: "int".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_process_return",
            LinkError::IncompatibleProcessReturn {
                process: "fixture".to_string(),
                expected: "str".to_string(),
                actual: "int".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_function_return",
            LinkError::IncompatibleFunctionReturn {
                function: "fixture".to_string(),
                expected: "str".to_string(),
                actual: "int".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "duplicate_function_param",
            LinkError::DuplicateFunctionParam {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "function_argument_count",
            LinkError::FunctionArgumentCount {
                function: "fixture".to_string(),
                expected: 1,
                actual: 2,
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_function_argument",
            LinkError::IncompatibleFunctionArgument {
                function: "fixture".to_string(),
                param: "fixture".to_string(),
                expected: "str".to_string(),
                actual: "int".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "forbidden_in_function",
            LinkError::ForbiddenInFunction {
                function: "fixture".to_string(),
                construct: "fixture",
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "function_name_is_not_a_value",
            LinkError::FunctionNameIsNotAValue {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "function_shadows_builtin",
            LinkError::FunctionShadowsBuiltin {
                name: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "process_literal_outside_process_slot",
            LinkError::ProcessLiteralOutsideProcessSlot {
                expected: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unresolved_receiver",
            LinkError::UnresolvedReceiver {
                operation: "fixture".to_string(),
                suggestions: Vec::new(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_resource_operation",
            LinkError::UnknownResourceOperation {
                resource_type: "fixture".to_string(),
                operation: "fixture".to_string(),
                suggestions: Vec::new(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "ambiguous_module_operation",
            LinkError::AmbiguousModuleOperation {
                module_path: "fixture".to_string(),
                operation: "fixture".to_string(),
                suggestions: Vec::new(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "bare_tool_call",
            LinkError::BareToolCall {
                name: "fixture".to_string(),
                suggestion: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_process_argument",
            LinkError::IncompatibleProcessArgument {
                process: "fixture".into(),
                arg: "fixture".into(),
                expected: "str".into(),
                actual: "int".into(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "feature_disabled",
            LinkError::FeatureDisabled {
                feature: "fixture",
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "process_lifecycle_outside_process",
            LinkError::ProcessLifecycleOutsideProcess {
                keyword: "fixture",
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "opaque_host_descriptor_access",
            LinkError::OpaqueHostDescriptorAccess {
                type_name: "fixture".to_string(),
                access: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "unknown_object_field",
            LinkError::UnknownObjectField {
                field: "fixture".to_string(),
                known: Vec::new(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_builtin_operands",
            LinkError::IncompatibleBuiltinOperands {
                builtin: "fixture".to_string(),
                expected: "fixture",
                actual: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "incompatible_iteration_target",
            LinkError::IncompatibleIterationTarget {
                actual: "fixture".to_string(),
                span: Some(Span { start: 4, end: 9 }),
            },
        ),
        (
            "module_hash",
            LinkError::ModuleHash {
                message: "fixture".to_string(),
            },
        ),
    ]
}
