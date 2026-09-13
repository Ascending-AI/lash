use super::*;

pub(crate) fn analyze_workflow_program(
    program: &Program,
    surface: &LashlangHostEnvironment,
) -> WorkflowLinkAnalysis {
    let mut linker = Linker::new(program, surface)
        .with_expected_type_facts()
        .with_workflow_analysis();
    linker.prepare_for_workflow_analysis();
    linker.clear_workflow_analysis();
    linker.recover_workflow_errors.set(true);

    for (index, declaration) in program.declarations.iter().enumerate() {
        let Declaration::Process(process) = declaration else {
            continue;
        };
        let mut scope = Scope::new(true, program.declaration_spans.get(index).copied());
        scope.expected_return = process.return_ty.clone();
        for param in &process.params {
            scope.bind(param.name.as_str(), linker.binding_for_type(&param.ty));
        }
        scope.bind("input", Binding::Value(process_input_type(process)));
        scope.bind("inputs", Binding::Value(process_input_record_type(process)));
        if let Err(error) = linker.lower_expr(&process.body, &mut scope) {
            linker.record_workflow_error(&process.body, error);
        }
    }

    let mut main_scope = Scope::new(false, None);
    for name in &surface.globals {
        main_scope.bind(name, any_binding());
    }
    if let Err(error) = linker.lower_expr(&program.main, &mut main_scope) {
        linker.record_workflow_error(&program.main, error);
    }
    linker.take_workflow_analysis()
}

impl WorkflowLinkAnalysis {
    pub(crate) fn facts_for(&self, expr: &Expr) -> Option<&WorkflowLinkNodeFacts> {
        self.nodes.get(&(expr as *const Expr as usize))
    }
}

impl LinkError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DuplicateDeclaration { .. } => "duplicate_declaration",
            Self::DuplicateProcessParam { .. } => "duplicate_process_param",
            Self::DuplicateProcessSignal { .. } => "duplicate_process_signal",
            Self::UnknownProcess { .. } => "unknown_process",
            Self::MissingProcessArgument { .. } => "missing_process_argument",
            Self::UnexpectedProcessArgument { .. } => "unexpected_process_argument",
            Self::DuplicateProcessArgument { .. } => "duplicate_process_argument",
            Self::UnknownName { .. } => "unknown_name",
            Self::UnknownBuiltin { .. } => "unknown_builtin",
            Self::UnknownResource { .. } => "unknown_resource",
            Self::UnknownType { .. } => "unknown_type",
            Self::IncompatibleConstructorInput { .. } => "incompatible_constructor_input",
            Self::IncompatibleOperationInput { .. } => "incompatible_operation_input",
            Self::AwaitedSettledExpression { .. } => "awaited_settled_expression",
            Self::IncompatibleExpectedLiteral { .. } => "incompatible_expected_literal",
            Self::IncompatibleProcessReturn { .. } => "incompatible_process_return",
            Self::IncompatibleFunctionReturn { .. } => "incompatible_function_return",
            Self::DuplicateFunctionParam { .. } => "duplicate_function_param",
            Self::FunctionArgumentCount { .. } => "function_argument_count",
            Self::IncompatibleFunctionArgument { .. } => "incompatible_function_argument",
            Self::ForbiddenInFunction { .. } => "forbidden_in_function",
            Self::FunctionNameIsNotAValue { .. } => "function_name_is_not_a_value",
            Self::FunctionShadowsBuiltin { .. } => "function_shadows_builtin",
            Self::InvalidTriggerRegistration { .. } => "invalid_trigger_registration",
            Self::InvalidTriggerSubscriptionKey { .. } => "invalid_trigger_subscription_key",
            Self::DuplicateDerivedTriggerSubscriptionKey { .. } => {
                "duplicate_derived_trigger_subscription_key"
            }
            Self::UnresolvedDerivedTriggerSubscriptionKey { .. } => {
                "unresolved_derived_trigger_subscription_key"
            }
            Self::InvalidTriggerInputs { .. } => "invalid_trigger_inputs",
            Self::DuplicateTriggerInput { .. } => "duplicate_trigger_input",
            Self::MissingTriggerInput { .. } => "missing_trigger_input",
            Self::UnknownTriggerInput { .. } => "unknown_trigger_input",
            Self::MissingTriggerEventInput { .. } => "missing_trigger_event_input",
            Self::TriggerEventOutsideInputs { .. } => "trigger_event_outside_inputs",
            Self::TriggerEventProjection { .. } => "trigger_event_projection",
            Self::InvalidTriggerList { .. } => "invalid_trigger_list",
            Self::UnknownTriggerEventType { .. } => "unknown_trigger_event_type",
            Self::InvalidTriggerTarget { .. } => "invalid_trigger_target",
            Self::TriggerEventMismatch { .. } => "trigger_event_mismatch",
            Self::UnresolvedReceiver { .. } => "unresolved_receiver",
            Self::UnknownResourceOperation { .. } => "unknown_resource_operation",
            Self::AmbiguousModuleOperation { .. } => "ambiguous_module_operation",
            Self::BareToolCall { .. } => "bare_tool_call",
            Self::IncompatibleProcessArgument { .. } => "incompatible_process_argument",
            Self::FeatureDisabled { .. } => "feature_disabled",
            Self::ProcessLifecycleOutsideProcess { .. } => "process_lifecycle_outside_process",
            Self::OpaqueHostDescriptorAccess { .. } => "opaque_host_descriptor_access",
            Self::UnknownObjectField { .. } => "unknown_object_field",
            Self::IncompatibleBinaryOperands { .. } => "incompatible_binary_operands",
            Self::IncompatibleBuiltinOperands { .. } => "incompatible_builtin_operands",
            Self::IncompatibleIterationTarget { .. } => "incompatible_iteration_target",
            Self::ModuleHash { .. } => "module_hash",
            Self::InvalidAst { .. } => "invalid_ast",
        }
    }
}

impl<'module> Linker<'module> {
    pub(super) fn prepare_for_workflow_analysis(&mut self) {
        for declaration in &self.program.declarations {
            match declaration {
                Declaration::Type(declaration) => {
                    self.type_defs
                        .insert(declaration.name.to_string(), declaration.ty.clone());
                }
                Declaration::Process(_) => {}
                Declaration::Function(function) => {
                    self.function_signatures
                        .insert(function.name.to_string(), function_signature(function));
                }
            }
        }
        for declaration in &self.program.declarations {
            let Declaration::Process(process) = declaration else {
                continue;
            };
            self.process_types.insert(
                process.name.to_string(),
                process_type_for_decl(process, process.return_ty.clone().unwrap_or(TypeExpr::Any)),
            );
        }
        for (index, declaration) in self.program.declarations.iter().enumerate() {
            let Declaration::Process(process) = declaration else {
                continue;
            };
            if let Ok(output) = self
                .infer_process_output(process, self.program.declaration_spans.get(index).copied())
            {
                self.process_types.insert(
                    process.name.to_string(),
                    process_type_for_decl(process, output),
                );
            }
        }
    }

    pub(super) fn begin_workflow_node(&self, expr: &Expr, scope: &Scope) {
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        analysis.borrow_mut().nodes.insert(
            expr as *const Expr as usize,
            WorkflowLinkNodeFacts {
                available_variables: scope
                    .bindings
                    .iter()
                    .map(|(name, binding)| {
                        (
                            name.clone(),
                            self.resolve_type_aliases(&binding_type(binding)),
                        )
                    })
                    .collect(),
                ..WorkflowLinkNodeFacts::default()
            },
        );
    }

    pub(super) fn finish_workflow_node(&self, expr: &Expr) {
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        let expected_arguments = self.expected_arguments_for_node(expr);
        analysis
            .borrow_mut()
            .nodes
            .entry(expr as *const Expr as usize)
            .or_default()
            .expected_arguments = expected_arguments;
    }

    pub(super) fn record_workflow_error(&self, expr: &Expr, error: LinkError) {
        let span = error.span();
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        let expected_arguments = self.expected_arguments_for_node(expr);
        let mut analysis = analysis.borrow_mut();
        let facts = analysis
            .nodes
            .entry(expr as *const Expr as usize)
            .or_default();
        facts
            .diagnostics
            .push(WorkflowLinkDiagnostic { error, span });
        facts.expected_arguments = expected_arguments;
    }

    pub(super) fn record_recovered_workflow_error(&self, expr: &Expr, error: LinkError) {
        let Some(owner) = self.workflow_diagnostic_owner.get() else {
            self.record_workflow_error(expr, error);
            return;
        };
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        let span = self
            .expression_spans
            .get(&owner)
            .copied()
            .or_else(|| error.span());
        let mut analysis = analysis.borrow_mut();
        analysis
            .nodes
            .entry(owner)
            .or_default()
            .diagnostics
            .push(WorkflowLinkDiagnostic { error, span });
    }

    pub(super) fn clear_workflow_analysis(&self) {
        if let Some(analysis) = &self.workflow_analysis {
            *analysis.borrow_mut() = WorkflowLinkAnalysis::default();
        }
    }

    pub(super) fn take_workflow_analysis(&mut self) -> WorkflowLinkAnalysis {
        self.workflow_analysis
            .take()
            .map(RefCell::into_inner)
            .unwrap_or_default()
    }

    pub(super) fn expected_arguments_for_node(
        &self,
        expr: &Expr,
    ) -> Vec<WorkflowLinkExpectedArgument> {
        let Some(expected_type_facts) = &self.expected_type_facts else {
            return Vec::new();
        };
        let expected_type_facts = expected_type_facts.borrow();
        let mut calls = Vec::new();
        crate::introspection::receiver_calls_in_expr(workflow_node_value(expr), &mut calls);
        let multiple_calls = calls.len() > 1;
        let mut arguments = Vec::new();
        for (call_index, call) in calls.into_iter().enumerate() {
            let Expr::ReceiverCall { args, .. } = call else {
                unreachable!("receiver-call collector only returns receiver calls")
            };
            for (argument_index, argument) in args.iter().enumerate() {
                let slot = if multiple_calls {
                    format!("call[{call_index}].arg[{argument_index}]")
                } else {
                    format!("arg[{argument_index}]")
                };
                collect_expected_slots(
                    argument,
                    slot,
                    &expected_type_facts.by_expression,
                    &mut arguments,
                );
            }
        }
        arguments
    }
}

pub(super) fn workflow_diagnostic_owner_key(expr: &Expr) -> Option<usize> {
    // The projector peels a node label before reading facts, but keeps an
    // assignment as the facts owner even when its kind is derived from the
    // assigned value. Other wrappers, including Print, keep their own facts.
    let projected = match expr {
        Expr::LabelAnnotated { expr, .. } => expr.as_ref(),
        _ => expr,
    };
    matches!(projected, Expr::Assign { .. }).then(|| projected as *const Expr as usize)
}

fn workflow_node_value(mut expr: &Expr) -> &Expr {
    while let Expr::LabelAnnotated { expr: inner, .. } = expr {
        expr = inner;
    }
    if let Expr::Assign { expr: value, .. } = expr {
        value
    } else {
        expr
    }
}

pub(super) fn recover_workflow_binding(expr: &Expr, scope: &mut Scope) {
    let mut expr = expr;
    while let Expr::LabelAnnotated { expr: inner, .. } = expr {
        expr = inner;
    }
    if let Expr::Assign { target, .. } = expr
        && target.steps.is_empty()
    {
        scope.bind(target.root.as_str(), any_binding());
    }
}

fn collect_expected_slots(
    expr: &Expr,
    slot: String,
    expected: &BTreeMap<usize, TypeExpr>,
    arguments: &mut Vec<WorkflowLinkExpectedArgument>,
) {
    if let Some(ty) = expected.get(&(expr as *const Expr as usize)) {
        arguments.push(WorkflowLinkExpectedArgument {
            slot: slot.clone(),
            ty: ty.clone(),
        });
    }
    match expr {
        Expr::LabelAnnotated { expr, .. } | Expr::Await(expr) | Expr::ResultUnwrap(expr) => {
            collect_expected_slots(expr, slot, expected, arguments)
        }
        Expr::Record(entries) => {
            for (name, value) in entries {
                collect_expected_slots(
                    value,
                    format!("{slot}.{}", name.as_str()),
                    expected,
                    arguments,
                );
            }
        }
        Expr::List(items) | Expr::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_expected_slots(item, format!("{slot}[{index}]"), expected, arguments);
            }
        }
        _ => {}
    }
}

pub(super) fn expression_spans_by_pointer(program: &Program) -> BTreeMap<usize, Span> {
    let spans_by_path = program
        .expression_source_spans
        .iter()
        .map(|source_span| (source_span.path.clone(), source_span.span))
        .collect::<BTreeMap<_, _>>();
    let mut spans = BTreeMap::new();
    collect_expression_spans_by_pointer(&program.main, &mut Vec::new(), &spans_by_path, &mut spans);
    spans
}

fn collect_expression_spans_by_pointer(
    expr: &Expr,
    path: &mut Vec<u32>,
    spans_by_path: &BTreeMap<Vec<u32>, Span>,
    spans: &mut BTreeMap<usize, Span>,
) {
    if let Some(span) = spans_by_path.get(path.as_slice()).copied() {
        spans.insert(expr as *const Expr as usize, span);
    }
    for (index, child) in expr.children().enumerate() {
        path.push(index.try_into().expect("AST child index fits u32"));
        collect_expression_spans_by_pointer(child, path, spans_by_path, spans);
        path.pop();
    }
}
