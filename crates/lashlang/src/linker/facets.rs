pub(crate) fn analyze_workflow_program(
    program: &Program,
    surface: &LashlangHostEnvironment,
) -> WorkflowLinkAnalysis {
    let mut linker = Linker::new(program, surface).with_expected_type_facts();
    linker.prepare_for_workflow_analysis();
    let spans = expression_spans_by_pointer(program);
    let mut analysis = WorkflowLinkAnalysis::default();

    for (index, declaration) in program.declarations.iter().enumerate() {
        let Declaration::Process(process) = declaration else {
            continue;
        };
        let mut scope = Scope::new(true, program.declaration_spans.get(index).copied());
        scope.expected_return = process.return_ty.clone();
        for param in &process.params {
            scope.bind(param.name.as_str(), linker.binding_for_type(&param.ty));
        }
        scope.bind(
            "input",
            Binding::Value(process_input_type(process)),
        );
        scope.bind(
            "inputs",
            Binding::Value(process_input_record_type(process)),
        );
        linker.analyze_workflow_block(&process.body, &mut scope, &spans, &mut analysis);
    }

    let mut main_scope = Scope::new(false, None);
    for name in &surface.globals {
        main_scope.bind(name, any_binding());
    }
    linker.analyze_workflow_block(
        &program.main,
        &mut main_scope,
        &spans,
        &mut analysis,
    );
    analysis
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
            Self::IncompatibleExpectedLiteral { .. } => "incompatible_expected_literal",
            Self::IncompatibleProcessReturn { .. } => "incompatible_process_return",
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
        }
    }
}

impl<'module> Linker<'module> {
    fn prepare_for_workflow_analysis(&mut self) {
        for declaration in &self.program.declarations {
            match declaration {
                Declaration::Type(declaration) => {
                    self.type_names.insert(declaration.name.to_string());
                    self.type_defs
                        .insert(declaration.name.to_string(), declaration.ty.clone());
                }
                Declaration::Process(process) => {
                    self.process_names.insert(process.name.to_string());
                }
            }
        }
        for declaration in &self.program.declarations {
            let Declaration::Process(process) = declaration else {
                continue;
            };
            self.process_types.insert(
                process.name.to_string(),
                process_type_for_decl(
                    process,
                    process.return_ty.clone().unwrap_or(TypeExpr::Any),
                ),
            );
        }
        for (index, declaration) in self.program.declarations.iter().enumerate() {
            let Declaration::Process(process) = declaration else {
                continue;
            };
            if let Ok(output) = self.infer_process_output(
                process,
                self.program.declaration_spans.get(index).copied(),
            ) {
                self.process_types.insert(
                    process.name.to_string(),
                    process_type_for_decl(process, output),
                );
            }
        }
    }

    fn analyze_workflow_block(
        &self,
        expr: &Expr,
        scope: &mut Scope,
        spans: &BTreeMap<usize, Span>,
        analysis: &mut WorkflowLinkAnalysis,
    ) {
        match expr {
            Expr::Block(expressions) => {
                for expression in expressions {
                    self.analyze_workflow_node(expression, scope, spans, analysis);
                }
            }
            expression => self.analyze_workflow_node(expression, scope, spans, analysis),
        }
    }

    fn analyze_workflow_node(
        &self,
        expr: &Expr,
        scope: &mut Scope,
        spans: &BTreeMap<usize, Span>,
        analysis: &mut WorkflowLinkAnalysis,
    ) {
        let expression_key = expr as *const Expr as usize;
        let mut facts = WorkflowLinkNodeFacts {
            available_variables: scope
                .bindings
                .iter()
                .map(|(name, binding)| {
                    (
                        name.clone(),
                        self.resolve_type_aliases(&binding_type(Some(binding))),
                    )
                })
                .collect(),
            ..WorkflowLinkNodeFacts::default()
        };
        let before = scope.clone();
        let parent_span = scope.span;
        scope.span = spans.get(&expression_key).copied().or(parent_span);
        if let Err(error) = self.infer_expr_type(expr, scope) {
            facts.diagnostics.push(error);
            *scope = before.clone();
            recover_workflow_binding(expr, scope);
        }
        scope.span = parent_span;
        facts.expected_arguments = self.expected_arguments_for_node(expr);
        analysis.nodes.insert(expression_key, facts);
        self.analyze_nested_workflow_nodes(expr, &before, spans, analysis);
    }

    fn analyze_nested_workflow_nodes(
        &self,
        expr: &Expr,
        scope: &Scope,
        spans: &BTreeMap<usize, Span>,
        analysis: &mut WorkflowLinkAnalysis,
    ) {
        let value = workflow_node_value(expr);
        match value {
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut branch_scope = scope.clone();
                let _ = self.infer_expr_type(condition, &mut branch_scope);
                let mut then_scope = branch_scope.clone();
                self.analyze_workflow_block(then_block, &mut then_scope, spans, analysis);
                let mut else_scope = branch_scope;
                self.analyze_workflow_block(else_block, &mut else_scope, spans, analysis);
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                let mut body_scope = scope.clone();
                let item_ty = self
                    .infer_expr_type(iterable, &mut body_scope)
                    .and_then(|ty| self.iterable_item_type(&ty, body_scope.span))
                    .unwrap_or(TypeExpr::Any);
                body_scope.bind(binding.as_str(), self.binding_for_type(&item_ty));
                self.analyze_workflow_block(body, &mut body_scope, spans, analysis);
            }
            Expr::While { condition, body } => {
                let mut body_scope = scope.clone();
                let _ = self.infer_expr_type(condition, &mut body_scope);
                self.analyze_workflow_block(body, &mut body_scope, spans, analysis);
            }
            Expr::ListComprehension { element, clauses } => {
                let mut element_scope = scope.clone();
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            let item_ty = self
                                .infer_expr_type(iterable, &mut element_scope)
                                .and_then(|ty| {
                                    self.iterable_item_type(&ty, element_scope.span)
                                })
                                .unwrap_or(TypeExpr::Any);
                            element_scope
                                .bind(binding.as_str(), self.binding_for_type(&item_ty));
                        }
                        ListComprehensionClause::If { condition } => {
                            let _ = self.infer_expr_type(condition, &mut element_scope);
                        }
                    }
                }
                self.analyze_workflow_block(element, &mut element_scope, spans, analysis);
            }
            _ => {}
        }
    }

    fn expected_arguments_for_node(&self, expr: &Expr) -> Vec<WorkflowLinkExpectedArgument> {
        let Some(expected_type_facts) = &self.expected_type_facts else {
            return Vec::new();
        };
        let expected_type_facts = expected_type_facts.borrow();
        let mut calls = Vec::new();
        collect_receiver_calls(workflow_node_value(expr), &mut calls);
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

fn recover_workflow_binding(expr: &Expr, scope: &mut Scope) {
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

fn collect_receiver_calls<'a>(expr: &'a Expr, calls: &mut Vec<&'a Expr>) {
    if matches!(expr, Expr::ReceiverCall { .. }) {
        calls.push(expr);
    }
    for child in expr.children() {
        collect_receiver_calls(child, calls);
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
        Expr::LabelAnnotated { expr, .. }
        | Expr::Await(expr)
        | Expr::ResultUnwrap(expr) => collect_expected_slots(expr, slot, expected, arguments),
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
                collect_expected_slots(
                    item,
                    format!("{slot}[{index}]"),
                    expected,
                    arguments,
                );
            }
        }
        _ => {}
    }
}

fn expression_spans_by_pointer(program: &Program) -> BTreeMap<usize, Span> {
    let spans_by_path = program
        .expression_source_spans
        .iter()
        .map(|source_span| (source_span.path.clone(), source_span.span))
        .collect::<BTreeMap<_, _>>();
    let mut spans = BTreeMap::new();
    collect_expression_spans_by_pointer(
        &program.main,
        &mut Vec::new(),
        &spans_by_path,
        &mut spans,
    );
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
