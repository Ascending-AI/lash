use super::*;

#[expect(
    clippy::expect_used,
    reason = "declaration indexes fit u32, per the program construction checks"
)]
pub fn analyze_workflow_program(
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
        let body_path = AstPath::declaration(
            index.try_into().expect("declaration index fits u32"),
            Vec::new(),
        );
        let mut scope = Scope::new(true, declaration_span(program, index));
        scope.expected_return = process.return_ty.clone();
        for param in &process.params {
            scope.declare(param.name.as_str(), linker.binding_for_type(&param.ty));
        }
        scope.declare("input", Binding::Value(process_input_type(process)));
        scope.declare("inputs", Binding::Value(process_input_record_type(process)));
        if let Err(error) = linker.lower_expr(&process.body, &body_path, &mut scope) {
            linker.record_workflow_error(&process.body, &body_path, error);
        }
    }

    let mut main_scope = Scope::new(false, None);
    for name in &surface.globals {
        main_scope.bind(name, any_binding());
    }
    let main_path = AstPath::main(Vec::new());
    if let Err(error) = linker.lower_expr(&program.main, &main_path, &mut main_scope) {
        linker.record_workflow_error(&program.main, &main_path, error);
    }
    linker.take_workflow_analysis()
}

impl WorkflowLinkAnalysis {
    /// The facts the canonical walk recorded for the node at `path` in the
    /// analyzed program.
    pub(crate) fn facts_for(&self, path: &AstPath) -> Option<&WorkflowLinkNodeFacts> {
        self.nodes.get(path)
    }
}

impl LinkError {
    pub fn kind(&self) -> &'static str {
        crate::WorkflowDiagnosticKind::from_link_error(self).as_str()
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
            if let Ok(output) = self.infer_process_output(
                process,
                &AstPath::declaration(index as u32, Vec::new()),
                declaration_span(self.program, index),
            ) {
                self.process_types.insert(
                    process.name.to_string(),
                    process_type_for_decl(process, output),
                );
            }
        }
    }

    pub(super) fn begin_workflow_node(&self, path: &AstPath, scope: &Scope) {
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        analysis.borrow_mut().nodes.insert(
            path.clone(),
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

    pub(super) fn finish_workflow_node(&self, expr: &Expr, path: &AstPath) {
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        let expected_arguments = self.expected_arguments_for_node(expr, path);
        analysis
            .borrow_mut()
            .nodes
            .entry(path.clone())
            .or_default()
            .expected_arguments = expected_arguments;
    }

    pub(super) fn record_workflow_error(&self, expr: &Expr, path: &AstPath, error: LinkError) {
        let span = error.span();
        let error_path = self
            .workflow_error_path
            .borrow_mut()
            .take()
            .unwrap_or_else(|| path.clone());
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        let expected_arguments = self.expected_arguments_for_node(expr, path);
        let mut analysis = analysis.borrow_mut();
        let facts = analysis.nodes.entry(path.clone()).or_default();
        facts.diagnostics.push(WorkflowLinkDiagnostic {
            error,
            span,
            path: error_path,
        });
        facts.expected_arguments = expected_arguments;
    }

    pub(super) fn record_recovered_workflow_error(
        &self,
        expr: &Expr,
        path: &AstPath,
        error: LinkError,
    ) {
        let owner = self.workflow_diagnostic_owner.borrow().clone();
        let Some(owner) = owner else {
            self.record_workflow_error(expr, path, error);
            return;
        };
        let Some(analysis) = &self.workflow_analysis else {
            return;
        };
        let span = self
            .program
            .spans
            .get(&owner)
            .copied()
            .or_else(|| error.span());
        let error_path = self
            .workflow_error_path
            .borrow_mut()
            .take()
            .unwrap_or_else(|| path.clone());
        let mut analysis = analysis.borrow_mut();
        analysis
            .nodes
            .entry(owner)
            .or_default()
            .diagnostics
            .push(WorkflowLinkDiagnostic {
                error,
                span,
                path: error_path,
            });
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
        path: &AstPath,
    ) -> Vec<WorkflowLinkExpectedArgument> {
        let Some(expected_type_facts) = &self.expected_type_facts else {
            return Vec::new();
        };
        let expected_type_facts = expected_type_facts.borrow();
        let (value, value_path) = workflow_node_value(expr, path.clone());
        let mut calls = Vec::new();
        receiver_calls_at(value, &value_path, &mut calls);
        let multiple_calls = calls.len() > 1;
        let mut arguments = Vec::new();
        for (call_index, (call, call_path)) in calls.into_iter().enumerate() {
            let Expr::ReceiverCall { args, .. } = call else {
                unreachable!("receiver-call collector only returns receiver calls")
            };
            for (argument_index, argument) in args.iter().enumerate() {
                let slot = if multiple_calls {
                    crate::WorkflowSlotPath::call_argument(call_index as u32, argument_index as u32)
                } else {
                    crate::WorkflowSlotPath::argument(argument_index as u32)
                };
                collect_expected_slots(
                    argument,
                    &call_path.child(argument_index as u32 + 1),
                    slot,
                    &expected_type_facts.by_expression,
                    &mut arguments,
                );
            }
        }
        arguments
    }
}

/// The [`AstPath`] of `child`, which must be a direct `children()` element of
/// `parent` under `parent_path`.
///
/// Child indexes are a property of the variant, so callers that already know
/// the index can use [`AstPath::child`] directly; this helper is for the cases
/// where the child was selected by shape (a record entry named `source`, the
/// value an `Assign` writes) rather than by position.
#[expect(
    clippy::expect_used,
    reason = "the caller selects `child` from `parent`'s own fields, so it is always a children() element, per the message"
)]
pub(super) fn child_ast_path(parent: &Expr, parent_path: &AstPath, child: &Expr) -> AstPath {
    let index = parent
        .children()
        .position(|candidate| std::ptr::eq(candidate, child))
        .expect("child is a direct children() element of parent");
    parent_path.child(index as u32)
}

/// The [`AstPath`] the workflow projector reads facts from for `expr` at
/// `path`, or `None` when the node keeps its own facts.
///
/// The projector peels a node label before reading facts, but keeps an
/// assignment as the facts owner even when its kind is derived from the
/// assigned value. Other wrappers, including Print, keep their own facts.
pub(super) fn workflow_diagnostic_owner_key(expr: &Expr, path: &AstPath) -> Option<AstPath> {
    let (projected, projected_path) = match expr {
        Expr::LabelAnnotated { expr, .. } => (expr.as_ref(), path.child(0)),
        _ => (expr, path.clone()),
    };
    matches!(projected, Expr::Assign { .. }).then_some(projected_path)
}

/// The expression a workflow node's value facets describe, with its own path:
/// labels are transparent and an `Assign` is read through its value.
fn workflow_node_value(mut expr: &Expr, mut path: AstPath) -> (&Expr, AstPath) {
    while let Expr::LabelAnnotated { expr: inner, .. } = expr {
        expr = inner;
        path = path.child(0);
    }
    if let Expr::Assign { expr: value, .. } = expr {
        // `Assign` children are the index steps followed by the value.
        let value_index = expr.children().len() - 1;
        (value, path.child(value_index as u32))
    } else {
        (expr, path)
    }
}

/// Every `ReceiverCall` inside `expr`, with its [`AstPath`], in walk order.
fn receiver_calls_at<'a>(expr: &'a Expr, path: &AstPath, calls: &mut Vec<(&'a Expr, AstPath)>) {
    if matches!(expr, Expr::ReceiverCall { .. }) {
        calls.push((expr, path.clone()));
    }
    for (index, child) in (0u32..).zip(expr.children()) {
        receiver_calls_at(child, &path.child(index), calls);
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
    path: &AstPath,
    slot: crate::WorkflowSlotPath,
    expected: &BTreeMap<AstPath, TypeExpr>,
    arguments: &mut Vec<WorkflowLinkExpectedArgument>,
) {
    if let Some(ty) = expected.get(path) {
        arguments.push(WorkflowLinkExpectedArgument {
            slot: slot.clone(),
            ty: ty.clone(),
            path: path.clone(),
        });
    }
    match expr {
        Expr::LabelAnnotated { expr, .. } | Expr::Await(expr) | Expr::ResultUnwrap(expr) => {
            collect_expected_slots(expr, &path.child(0), slot, expected, arguments)
        }
        Expr::Record(entries) => {
            for (index, (name, value)) in entries.iter().enumerate() {
                let mut field_slot = slot.clone();
                field_slot.push(crate::WorkflowSlotPathSegment::Field(name.clone()));
                collect_expected_slots(
                    value,
                    &path.child(index as u32),
                    field_slot,
                    expected,
                    arguments,
                );
            }
        }
        Expr::List(items) | Expr::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                let mut item_slot = slot.clone();
                item_slot.push(crate::WorkflowSlotPathSegment::Index(index as u32));
                collect_expected_slots(
                    item,
                    &path.child(index as u32),
                    item_slot,
                    expected,
                    arguments,
                );
            }
        }
        _ => {}
    }
}

/// The span recorded for `program.declarations[index]` itself.
pub(super) fn declaration_span(program: &Program, index: usize) -> Option<Span> {
    let index = u32::try_from(index).ok()?;
    program
        .spans
        .get(&AstPath::declaration(index, Vec::new()))
        .copied()
}
