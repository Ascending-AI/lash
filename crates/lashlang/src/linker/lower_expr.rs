impl<'module> Linker<'module> {
    fn lower_expr(
        &self,
        expr: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.lower_expr_expected(expr, scope, None)
    }

    fn lower_expr_expected(
        &self,
        expr: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let previous_span = scope.span;
        if let Some(span) = self.expression_spans.get(&(expr as *const Expr as usize)) {
            scope.span = Some(*span);
        }
        let result = self.lower_expr_expected_inner(expr, scope, expected);
        scope.span = previous_span;
        result
    }

    fn lower_expr_expected_inner(
        &self,
        expr: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.reject_trigger_event_special_form(expr, scope.span)?;
        self.validate_expected_literals(expr, expected, scope.span)?;
        if matches!(expr, Expr::Variable(_) | Expr::Field { .. })
            && let Some(resource) = self.resolve_module_expr(expr, scope)
        {
            return Ok((
                Expr::ResourceRef(resource.clone()),
                Some(Binding::Resource {
                    resource_type: resource.resource_type.to_string(),
                }),
            ));
        }
        Ok(match expr {
            Expr::Block(expressions) => {
                let mut lowered = Vec::with_capacity(expressions.len());
                let mut last = None;
                let last_index = expressions.len().saturating_sub(1);
                for (index, expression) in expressions.iter().enumerate() {
                    let (expr, binding) = self.lower_expr_expected(
                        expression,
                        scope,
                        (index == last_index).then_some(expected).flatten(),
                    )?;
                    lowered.push(expr);
                    last = binding;
                }
                (Expr::Block(lowered), last)
            }
            Expr::LabelAnnotated { label, expr } => {
                self.ensure_feature(
                    self.surface.language_features.label_annotations,
                    "label annotations",
                    scope.span,
                )?;
                let (expr, binding) = self.lower_expr_expected(expr, scope, expected)?;
                (
                    Expr::LabelAnnotated {
                        label: label.clone(),
                        expr: Box::new(expr),
                    },
                    binding,
                )
            }
            Expr::Variable(name) => {
                if let Some(binding) = scope.get(name) {
                    (Expr::Variable(name.clone()), Some(binding))
                } else if let Some(process_ty) = self.process_types.get(name.as_str()) {
                    (
                        Expr::ProcessRef {
                            process: name.clone(),
                        },
                        Some(Binding::Value(process_ty.clone())),
                    )
                } else {
                    return Err(LinkError::UnknownName {
                        name: name.to_string(),
                        span: scope.span,
                    });
                }
            }
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Break
            | Expr::Continue => (expr.clone(), Some(Binding::Value(literal_type(expr)))),
            Expr::TypeLiteral(_) => (
                expr.clone(),
                Some(
                    self.closed_schema_witness_binding(expr)
                        .unwrap_or_else(any_binding),
                ),
            ),
            Expr::Tuple(items) => {
                let mut lowered = Vec::with_capacity(items.len());
                let mut item_types = Vec::with_capacity(items.len());
                let expected_item = expected.and_then(|expected| {
                    match self.resolve_type_aliases(expected) {
                        TypeExpr::List(item) => Some(*item),
                        _ => None,
                    }
                });
                for item in items {
                    let (item, binding) =
                        self.lower_expr_expected(item, scope, expected_item.as_ref())?;
                    lowered.push(item);
                    item_types.push(binding_type(binding.as_ref()));
                }
                (
                    Expr::Tuple(lowered),
                    Some(Binding::Value(TypeExpr::List(Box::new(union_type(
                        item_types,
                    ))))),
                )
            }
            Expr::List(items) => {
                let mut lowered = Vec::with_capacity(items.len());
                let mut item_types = Vec::with_capacity(items.len());
                let expected_item = expected.and_then(|expected| {
                    match self.resolve_type_aliases(expected) {
                        TypeExpr::List(item) => Some(*item),
                        _ => None,
                    }
                });
                for item in items {
                    let (item, binding) =
                        self.lower_expr_expected(item, scope, expected_item.as_ref())?;
                    lowered.push(item);
                    item_types.push(binding_type(binding.as_ref()));
                }
                (
                    Expr::List(lowered),
                    Some(Binding::Value(TypeExpr::List(Box::new(union_type(
                        item_types,
                    ))))),
                )
            }
            Expr::ListComprehension { element, clauses } => {
                let mut lowered_clauses = Vec::with_capacity(clauses.len());
                let mut previous_bindings = Vec::new();
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            let (iterable, iterable_binding) = self.lower_expr(iterable, scope)?;
                            let item_ty = self.iterable_item_type(
                                &binding_type(iterable_binding.as_ref()),
                                scope.span,
                            )?;
                            previous_bindings.push((
                                binding.to_string(),
                                scope.bind(binding.as_str(), self.binding_for_type(&item_ty)),
                            ));
                            lowered_clauses.push(ListComprehensionClause::For {
                                binding: binding.clone(),
                                iterable,
                            });
                        }
                        ListComprehensionClause::If { condition } => {
                            let condition = self.lower_expr(condition, scope)?.0;
                            lowered_clauses.push(ListComprehensionClause::If { condition });
                        }
                    }
                }
                let (element, binding) = self.lower_expr(element, scope)?;
                for (name, previous) in previous_bindings.into_iter().rev() {
                    scope.restore(name.as_str(), previous);
                }
                (
                    Expr::ListComprehension {
                        element: Box::new(element),
                        clauses: lowered_clauses,
                    },
                    Some(Binding::Value(TypeExpr::List(Box::new(binding_type(
                        binding.as_ref(),
                    ))))),
                )
            }
            Expr::Record(entries) => {
                let mut lowered = Vec::with_capacity(entries.len());
                let mut fields = Vec::with_capacity(entries.len());
                for (name, value) in entries {
                    let expected_field = expected.and_then(|expected| {
                        match self.resolve_type_aliases(expected) {
                            TypeExpr::Object(fields) => fields
                                .into_iter()
                                .find(|field| field.name == *name)
                                .map(|field| field.ty),
                            _ => None,
                        }
                    });
                    let (value, binding) =
                        self.lower_expr_expected(value, scope, expected_field.as_ref())?;
                    fields.push(TypeField {
                        name: name.clone(),
                        ty: binding_type(binding.as_ref()),
                        optional: false,
                    });
                    lowered.push((name.clone(), value));
                }
                (
                    Expr::Record(lowered),
                    Some(Binding::Value(TypeExpr::Object(fields))),
                )
            }
            Expr::Assign { target, expr } => {
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        self.lower_expr(index, scope)?;
                    }
                }
                let target_expected = self.assignment_target_type(target, scope)?;
                let (lowered, binding) =
                    self.lower_expr_expected(expr, scope, target_expected.as_ref())?;
                if target.steps.is_empty() {
                    scope.bind(
                        target.root.as_str(),
                        binding.clone().unwrap_or(any_binding()),
                    );
                } else {
                    let value_ty = binding_type(binding.as_ref());
                    scope.update_path(target, &value_ty)?;
                }
                (
                    Expr::Assign {
                        target: target.clone(),
                        expr: Box::new(lowered),
                    },
                    binding,
                )
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                let condition = self.lower_expr(condition, scope)?.0;
                let mut then_scope = scope.clone();
                let (then_block, then_binding) =
                    self.lower_expr_expected(then_block, &mut then_scope, expected)?;
                let mut else_scope = scope.clone();
                let (else_block, else_binding) =
                    self.lower_expr_expected(else_block, &mut else_scope, expected)?;
                scope.join_branches(then_scope, else_scope);
                (
                    Expr::If {
                        condition: Box::new(condition),
                        then_block: Box::new(then_block),
                        else_block: Box::new(else_block),
                    },
                    Some(Binding::Value(union_type(vec![
                        binding_type(then_binding.as_ref()),
                        binding_type(else_binding.as_ref()),
                    ]))),
                )
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                let (iterable, iterable_binding) = self.lower_expr(iterable, scope)?;
                let item_ty = self.iterable_item_type(
                    &binding_type(iterable_binding.as_ref()),
                    scope.span,
                )?;
                let before = scope.clone();
                let mut body_scope = scope.clone();
                let previous = body_scope.bind(
                    binding.as_str(),
                    self.binding_for_type(&item_ty),
                );
                let body = self.lower_expr(body, &mut body_scope)?.0;
                body_scope.restore(binding.as_str(), previous);
                scope.widen_loop(before, body_scope);
                (
                    Expr::For {
                        binding: binding.clone(),
                        iterable: Box::new(iterable),
                        body: Box::new(body),
                    },
                    Some(Binding::Value(TypeExpr::Null)),
                )
            }
            Expr::While { condition, body } => {
                let condition = self.lower_expr(condition, scope)?.0;
                let before = scope.clone();
                let mut body_scope = scope.clone();
                let body = self.lower_expr(body, &mut body_scope)?.0;
                scope.widen_loop(before, body_scope);
                (
                    Expr::While {
                        condition: Box::new(condition),
                        body: Box::new(body),
                    },
                    Some(Binding::Value(TypeExpr::Null)),
                )
            }
            Expr::StartProcess(start) => {
                self.ensure_feature(self.surface.abilities.processes, "processes", scope.span)?;
                let Some(process) = self.program.process(start.process.as_str()) else {
                    return Err(LinkError::UnknownProcess {
                        name: start.process.to_string(),
                        span: scope.span,
                    });
                };
                let mut seen = BTreeSet::new();
                let mut lowered_args = Vec::with_capacity(start.args.len());
                for (arg, value) in &start.args {
                    if !seen.insert(arg.to_string()) {
                        return Err(LinkError::DuplicateProcessArgument {
                            arg: arg.to_string(),
                            span: scope.span,
                        });
                    }
                    let Some(param) = process.params.iter().find(|param| param.name == *arg) else {
                        return Err(LinkError::UnexpectedProcessArgument {
                            process: process.name.to_string(),
                            arg: arg.to_string(),
                            span: scope.span,
                        });
                    };
                    let (lowered, binding) =
                        self.lower_expr_expected(value, scope, Some(&param.ty))?;
                    self.validate_process_arg_binding(
                        process.name.as_str(),
                        arg.as_str(),
                        &param.ty,
                        binding.as_ref(),
                        scope.span,
                    )?;
                    lowered_args.push((arg.clone(), lowered));
                }
                for param in &process.params {
                    if !seen.contains(param.name.as_str()) {
                        return Err(LinkError::MissingProcessArgument {
                            process: process.name.to_string(),
                            arg: param.name.to_string(),
                            span: scope.span,
                        });
                    }
                }
                (
                    Expr::StartProcess(crate::ast::ProcessStartExpr {
                        process: start.process.clone(),
                        args: lowered_args,
                    }),
                    Some(Binding::Value(
                        self.process_output_type(start.process.as_str()),
                    )),
                )
            }
            Expr::ProcessRef { process } => {
                let Some(process_ty) = self.process_types.get(process.as_str()) else {
                    return Err(LinkError::UnknownProcess {
                        name: process.to_string(),
                        span: scope.span,
                    });
                };
                (
                    Expr::ProcessRef {
                        process: process.clone(),
                    },
                    Some(Binding::Value(process_ty.clone())),
                )
            }
            Expr::HostDescriptorConstructor { type_name, input } => (
                Expr::HostDescriptorConstructor {
                    type_name: type_name.clone(),
                    input: Box::new(self.lower_expr(input, scope)?.0),
                },
                Some(Binding::Value(TypeExpr::Ref(type_name.clone()))),
            ),
            Expr::ResourceRef(resource) => {
                let resource = self.validate_resource_ref(resource, scope.span)?;
                (
                    Expr::ResourceRef(resource.clone()),
                    Some(Binding::Resource {
                        resource_type: resource.resource_type.to_string(),
                    }),
                )
            }
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                if let Some(mut path) = module_path_for_expr(receiver) {
                    path.push(operation.clone());
                    if let Some(constructor) =
                        self.surface.resources.resolve_value_constructor(&path)
                    {
                        if args.len() != 1 {
                            return Err(LinkError::IncompatibleConstructorInput {
                                path: module_path_key(&path),
                                expected: format_type_expr(&constructor.input_ty),
                                actual: format!("{} arguments", args.len()),
                                span: scope.span,
                            });
                        }
                        let (input, input_binding) = self.lower_expr_expected(
                            &args[0],
                            scope,
                            Some(&constructor.input_ty),
                        )?;
                        let actual_ty = binding_type(input_binding.as_ref());
                        if !self.is_type_assignable(&actual_ty, &constructor.input_ty) {
                            return Err(LinkError::IncompatibleConstructorInput {
                                path: module_path_key(&path),
                                expected: format_type_expr(
                                    &self.resolve_type_aliases(&constructor.input_ty),
                                ),
                                actual: format_type_expr(&self.resolve_type_aliases(&actual_ty)),
                                span: scope.span,
                            });
                        }
                        return Ok((
                            Expr::HostDescriptorConstructor {
                                type_name: constructor.type_name.clone().into(),
                                input: Box::new(input),
                            },
                            Some(Binding::Value(constructor.output_ty.clone())),
                        ));
                    }
                }
                let resolved_receiver = self
                    .resolve_module_operation_expr(receiver, operation)
                    .or_else(|| self.resolve_module_expr(receiver, scope));
                let (lowered_receiver, resource_type, receiver_alias) =
                    if let Some(resource) = resolved_receiver.as_ref() {
                        (
                            Expr::ResourceRef(resource.clone()),
                            Some(resource.resource_type.to_string()),
                            Some(resource.alias.to_string()),
                        )
                    } else {
                        let (lowered_receiver, binding) = self.lower_expr(receiver, scope)?;
                        let resource_type = match binding {
                            Some(Binding::Resource { resource_type }) => Some(resource_type),
                            _ => None,
                        };
                        (lowered_receiver, resource_type, None)
                    };
                let Some(resource_type) = resource_type else {
                    if let Some(path) = module_path_for_expr(receiver) {
                        let suggestions = self
                            .surface
                            .resources
                            .operation_suggestions_for_prefix(&path, operation.as_str());
                        if !suggestions.is_empty() {
                            return Err(LinkError::AmbiguousModuleOperation {
                                module_path: module_path_key(&path),
                                operation: operation.to_string(),
                                suggestions,
                                span: scope.span,
                            });
                        }
                    }
                    return Err(LinkError::UnresolvedReceiver {
                        operation: operation.to_string(),
                        suggestions: self
                            .surface
                            .resources
                            .operation_suggestions_for_operation(operation.as_str()),
                        span: scope.span,
                    });
                };
                if let Some(alias) = receiver_alias.as_deref()
                    && self
                        .surface
                        .resources
                        .resolve_module_operation(&resource_type, alias, operation.as_str())
                        .is_none()
                {
                    return Err(LinkError::UnknownResourceOperation {
                        resource_type: resource_type.clone(),
                        operation: operation.to_string(),
                        suggestions: self
                            .surface
                            .resources
                            .operation_suggestions_for_resource_type(&resource_type),
                        span: scope.span,
                    });
                }
                let Some(operation_binding) = self
                    .surface
                    .resources
                    .resolve_operation(&resource_type, operation)
                    .cloned()
                else {
                    return Err(LinkError::UnknownResourceOperation {
                        resource_type: resource_type.clone(),
                        operation: operation.to_string(),
                        suggestions: self
                            .surface
                            .resources
                            .operation_suggestions_for_resource_type(&resource_type),
                        span: scope.span,
                    });
                };
                let trigger_operation = if crate::is_trigger_resource_type(&resource_type) {
                    crate::TriggerHostOperation::from_receiver_method(operation.as_str())
                } else {
                    None
                };
                if let Some(trigger_operation) = trigger_operation {
                    self.ensure_feature(self.surface.abilities.triggers, "triggers", scope.span)?;
                    validate_trigger_operation_subscription_key(
                        trigger_operation,
                        args,
                        scope.span,
                    )?;
                }
                let trigger_operation = trigger_operation.filter(|operation| {
                    matches!(
                        operation,
                        crate::TriggerHostOperation::Register
                            | crate::TriggerHostOperation::List
                            | crate::TriggerHostOperation::Update
                            | crate::TriggerHostOperation::Revive
                    )
                });
                if let Some(trigger_operation) = trigger_operation {
                    let (lowered_args, output_ty) =
                        self.lower_trigger_operation_args(trigger_operation, args, scope)?;
                    return Ok((
                        Expr::ReceiverCall {
                            receiver: Box::new(lowered_receiver),
                            operation: operation.clone(),
                            args: lowered_args,
                        },
                        Some(Binding::Value(output_ty)),
                    ));
                }
                let mut lowered_args = Vec::with_capacity(args.len());
                let mut arg_types = Vec::with_capacity(args.len());
                for arg in args {
                    let expected_arg =
                        expected_call_arg_type(&operation_binding.input_ty, args.len());
                    let (arg, binding) =
                        self.lower_expr_expected(arg, scope, expected_arg)?;
                    lowered_args.push(arg);
                    arg_types.push(binding_type(binding.as_ref()));
                }
                let actual_input = call_input_type(arg_types);
                if !self.is_type_assignable(&actual_input, &operation_binding.input_ty) {
                    return Err(LinkError::IncompatibleOperationInput {
                        operation: operation.to_string(),
                        expected: format_type_expr(
                            &self.resolve_type_aliases(&operation_binding.input_ty),
                        ),
                        actual: format_type_expr(&self.resolve_type_aliases(&actual_input)),
                        span: scope.span,
                    });
                }
                (
                    Expr::ReceiverCall {
                        receiver: Box::new(lowered_receiver),
                        operation: operation.clone(),
                        args: lowered_args,
                    },
                    Some(Binding::Value(
                        self.operation_call_output_type(&operation_binding, args),
                    )),
                )
            }
            Expr::Await(inner) => {
                let (inner, binding) = self.lower_expr_expected(inner, scope, expected)?;
                (Expr::Await(Box::new(inner)), binding)
            }
            Expr::SleepFor(inner) => {
                self.ensure_feature(self.surface.abilities.sleep, "sleep", scope.span)?;
                (
                    Expr::SleepFor(Box::new(self.lower_expr(inner, scope)?.0)),
                    Some(Binding::Value(TypeExpr::Null)),
                )
            }
            Expr::SleepUntil(inner) => {
                self.ensure_feature(self.surface.abilities.sleep, "sleep", scope.span)?;
                (
                    Expr::SleepUntil(Box::new(self.lower_expr(inner, scope)?.0)),
                    Some(Binding::Value(TypeExpr::Null)),
                )
            }
            Expr::WaitSignal { name } => {
                self.ensure_feature(
                    self.surface.abilities.process_signals,
                    "process signals",
                    scope.span,
                )?;
                if !scope.process_body {
                    return Err(LinkError::ProcessLifecycleOutsideProcess {
                        keyword: "wait_signal",
                        span: scope.span,
                    });
                }
                (
                    Expr::WaitSignal { name: name.clone() },
                    Some(Binding::Value(TypeExpr::Any)),
                )
            }
            Expr::SignalRun { run, name, payload } => {
                self.ensure_feature(
                    self.surface.abilities.process_signals,
                    "process signals",
                    scope.span,
                )?;
                // `signal_run` (sending) is a control-plane op like `await` /
                // `cancel`, valid from the foreground turn as well as inside a
                // process body. Only `wait_signal` (receiving) is process-only.
                (
                    Expr::SignalRun {
                        run: Box::new(self.lower_expr(run, scope)?.0),
                        name: name.clone(),
                        payload: Box::new(self.lower_expr(payload, scope)?.0),
                    },
                    Some(Binding::Value(TypeExpr::Null)),
                )
            }
            Expr::ResultUnwrap(inner) => {
                let (inner, binding) = self.lower_expr_expected(inner, scope, expected)?;
                (Expr::ResultUnwrap(Box::new(inner)), binding)
            }
            Expr::Cancel(inner) => (
                Expr::Cancel(Box::new(self.lower_expr(inner, scope)?.0)),
                Some(Binding::Value(TypeExpr::Any)),
            ),
            Expr::Print(inner) => (
                Expr::Print(Box::new(self.lower_expr(inner, scope)?.0)),
                Some(Binding::Value(TypeExpr::Null)),
            ),
            Expr::Yield(inner) => (
                Expr::Yield(Box::new(self.lower_expr(inner, scope)?.0)),
                Some(Binding::Value(TypeExpr::Null)),
            ),
            Expr::Wake(inner) => (
                Expr::Wake(Box::new(self.lower_expr(inner, scope)?.0)),
                Some(Binding::Value(TypeExpr::Null)),
            ),
            Expr::Finish(inner) => {
                let expected_return = scope.expected_return.clone();
                let (inner, binding) =
                    self.lower_expr_expected(inner, scope, expected_return.as_ref())?;
                let finish_ty = binding_type(binding.as_ref());
                (Expr::Finish(Box::new(inner)), Some(Binding::Value(finish_ty)))
            }
            Expr::Fail(inner) => (
                Expr::Fail(Box::new(self.lower_expr(inner, scope)?.0)),
                Some(Binding::Value(TypeExpr::Null)),
            ),
            Expr::BuiltinCall { name, args } => {
                if !crate::builtins::is_builtin(name.as_str()) {
                    if let Some(suggestion) = self
                        .surface
                        .resources
                        .operation_suggestions_for_host(name.as_str())
                        .into_iter()
                        .next()
                    {
                        return Err(LinkError::BareToolCall {
                            name: name.to_string(),
                            suggestion,
                            span: scope.span,
                        });
                    }
                    return Err(LinkError::UnknownBuiltin {
                        name: name.to_string(),
                        span: scope.span,
                    });
                }
                let lowered_args = args
                    .iter()
                    .map(|arg| self.lower_expr(arg, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                let arg_types = lowered_args
                    .iter()
                    .map(|(_, binding)| binding_type(binding.as_ref()))
                    .collect::<Vec<_>>();
                self.validate_shaping_builtin(name.as_str(), &arg_types, scope.span)?;
                (
                    Expr::BuiltinCall {
                        name: name.clone(),
                        args: lowered_args.into_iter().map(|(expr, _)| expr).collect(),
                    },
                    Some(Binding::Value(shaping_builtin_return_type(
                        name.as_str(),
                        &arg_types,
                    ))),
                )
            }
            Expr::Field { target, field } => {
                let (target, binding) = self.lower_expr(target, scope)?;
                let ty =
                    self.field_type(&binding_type(binding.as_ref()), field.as_str(), scope.span)?;
                (
                    Expr::Field {
                        target: Box::new(target),
                        field: field.clone(),
                    },
                    Some(Binding::Value(ty)),
                )
            }
            Expr::Index { target, index } => {
                let (target, target_binding) = self.lower_expr(target, scope)?;
                let index = self.lower_expr(index, scope)?.0;
                (
                    Expr::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    Some(Binding::Value(self.index_type(
                        &binding_type(target_binding.as_ref()),
                        scope.span,
                    )?)),
                )
            }
            Expr::Unary { op, expr } => (
                Expr::Unary {
                    op: *op,
                    expr: Box::new(self.lower_expr(expr, scope)?.0),
                },
                Some(Binding::Value(match op {
                    crate::ast::UnaryOp::Not => TypeExpr::Bool,
                    crate::ast::UnaryOp::Negate => TypeExpr::Float,
                })),
            ),
            Expr::Binary { left, op, right } => {
                let (left, left_binding) = self.lower_expr(left, scope)?;
                let (right, right_binding) = self.lower_expr(right, scope)?;
                self.validate_binary_operands(
                    *op,
                    &binding_type(left_binding.as_ref()),
                    &binding_type(right_binding.as_ref()),
                    scope.span,
                )?;
                (
                    Expr::Binary {
                        left: Box::new(left),
                        op: *op,
                        right: Box::new(right),
                    },
                    Some(Binding::Value(binary_return_type(*op))),
                )
            }
        })
    }

    fn resolve_module_expr(&self, expr: &Expr, scope: &Scope) -> Option<ResourceRefExpr> {
        let path = module_path_for_expr(expr)?;
        if path
            .first()
            .and_then(|root| scope.get_str(root.as_str()))
            .is_some()
        {
            return None;
        }
        self.surface.resources.resolve_module_path(&path)
    }

    fn resolve_module_operation_expr(
        &self,
        receiver: &Expr,
        operation: &AstString,
    ) -> Option<ResourceRefExpr> {
        // Exact host operation paths occupy the module namespace even when a
        // live value shares their root. Other expressions retain lexical
        // shadowing through `resolve_module_expr`.
        let path = module_path_for_expr(receiver)?;
        let resource = self.surface.resources.resolve_module_path(&path)?;
        self.surface.resources.resolve_module_operation(
            resource.resource_type.as_str(),
            resource.alias.as_str(),
            operation.as_str(),
        )?;
        Some(resource)
    }

    fn reject_trigger_event_special_form(
        &self,
        expr: &Expr,
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        if is_trigger_event_projection_expr(expr) {
            return Err(LinkError::TriggerEventProjection { span });
        }
        if is_trigger_event_expr(expr) {
            return Err(LinkError::TriggerEventOutsideInputs { span });
        }
        Ok(())
    }

}
