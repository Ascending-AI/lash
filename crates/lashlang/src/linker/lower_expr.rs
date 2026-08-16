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

    /// Dispatches one expression node to its variant's lowering.
    ///
    /// Every variant lowers in its own method rather than in an arm of this
    /// match, and that split is load-bearing rather than cosmetic: this is the
    /// recursive step, so its frame is paid once per level of source nesting,
    /// and an unoptimized build gives a stack slot to every local of every arm
    /// whichever arm actually runs. Collapsed into one body the arms summed to
    /// a ~68 KiB frame, so the deepest program the parser admits
    /// (`MAX_NESTING_DEPTH`) needed ~3 MiB to link — past the 2 MiB a host
    /// thread gets, which turned a legal program into an abort. Split, each
    /// level carries this dispatcher plus the one variant's frame. Keep new
    /// variants in their own method.
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
            Expr::Block(expressions) => self.lower_block(expressions, scope, expected)?,
            Expr::LabelAnnotated { label, expr } => {
                self.lower_label_annotated(label, expr, scope, expected)?
            }
            Expr::Variable(name) => self.lower_variable(name, scope)?,
            Expr::Null
            | Expr::Undefined
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
            Expr::Tuple(items) => self.lower_tuple(items, scope, expected)?,
            Expr::List(items) => self.lower_list(items, scope, expected)?,
            Expr::ListComprehension { element, clauses } => {
                self.lower_list_comprehension(element, clauses, scope)?
            }
            Expr::Record(entries) => self.lower_record(entries, scope, expected)?,
            Expr::Assign { target, expr } => self.lower_assign(target, expr, scope)?,
            Expr::If {
                condition,
                then_block,
                else_block,
            } => self.lower_if(condition, then_block, else_block, scope, expected)?,
            Expr::For {
                binding,
                iterable,
                body,
            } => self.lower_for(binding, iterable, body, scope)?,
            Expr::While { condition, body } => self.lower_while(condition, body, scope)?,
            Expr::StartProcess(start) => self.lower_start_process(start, scope)?,
            Expr::ProcessRef { process } => self.lower_process_ref(process, scope)?,
            Expr::HostDescriptorConstructor { type_name, input } => {
                self.lower_host_descriptor_constructor(type_name, input, scope)?
            }
            Expr::ResourceRef(resource) => self.lower_resource_ref(resource, scope)?,
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => self.lower_receiver_call(receiver, operation, args, scope)?,
            Expr::Await(inner) => self.lower_await(inner, scope, expected)?,
            Expr::SleepFor(inner) => self.lower_sleep_for(inner, scope)?,
            Expr::SleepUntil(inner) => self.lower_sleep_until(inner, scope)?,
            Expr::WaitSignal { name } => self.lower_wait_signal(name, scope)?,
            Expr::SignalRun { run, name, payload } => {
                self.lower_signal_run(run, name, payload, scope)?
            }
            Expr::ResultUnwrap(inner) => self.lower_result_unwrap(inner, scope, expected)?,
            Expr::Cancel(inner) => self.lower_cancel(inner, scope)?,
            Expr::Print(inner) => self.lower_print(inner, scope)?,
            Expr::Yield(inner) => self.lower_yield(inner, scope)?,
            Expr::Wake(inner) => self.lower_wake(inner, scope)?,
            Expr::Finish(inner) => self.lower_finish(inner, scope)?,
            Expr::Fail(inner) => self.lower_fail(inner, scope)?,
            Expr::BuiltinCall { name, args } => self.lower_builtin_call(name, args, scope)?,
            Expr::Function(function) => self.lower_function(function, scope)?,
            Expr::Call { function, args } => self.lower_call(function, args, scope)?,
            Expr::Map { items, function } => self.lower_map(items, function, scope)?,
            Expr::Try(exception) => self.lower_try_expr(exception, scope)?,
            Expr::Throw(value) => self.lower_throw_expr(value, scope)?,
            Expr::Return(value) => self.lower_return_expr(value, scope)?,
            Expr::Field { target, field } => self.lower_field(target, field, scope)?,
            Expr::Index { target, index } => self.lower_index(target, index, scope)?,
            Expr::Unary { op, expr } => self.lower_unary(op, expr, scope)?,
            Expr::Binary { left, op, right } => self.lower_binary(left, op, right, scope)?,
            Expr::JavaScriptUnary { op, expr } => self.lower_javascript_unary(op, expr, scope)?,
            Expr::JavaScriptBinary { left, op, right } => {
                self.lower_javascript_binary(left, op, right, scope)?
            }
            Expr::JavaScriptLogical { left, op, right } => {
                self.lower_javascript_logical(left, op, right, scope)?
            }
        })
    }

    fn lower_block(
        &self,
        expressions: &[Expr],
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
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
        Ok((Expr::Block(lowered), last))
    }

    fn lower_label_annotated(
        &self,
        label: &crate::ast::LabelMetadata,
        expr: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.ensure_feature(
            self.surface.language_features.label_annotations,
            "label annotations",
            scope.span,
        )?;
        let (expr, binding) = self.lower_expr_expected(expr, scope, expected)?;
        Ok((
            Expr::LabelAnnotated {
                label: label.clone(),
                expr: Box::new(expr),
            },
            binding,
        ))
    }

    fn lower_variable(
        &self,
        name: &AstString,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok(if let Some(binding) = scope.get(name) {
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
        })
    }

    fn lower_tuple(
        &self,
        items: &[Expr],
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let mut lowered = Vec::with_capacity(items.len());
        let mut item_types = Vec::with_capacity(items.len());
        let expected_item =
            expected.and_then(|expected| match self.resolve_type_aliases(expected) {
                TypeExpr::List(item) => Some(*item),
                _ => None,
            });
        for item in items {
            let (item, binding) = self.lower_expr_expected(item, scope, expected_item.as_ref())?;
            lowered.push(item);
            item_types.push(binding_type(binding.as_ref()));
        }
        Ok((
            Expr::Tuple(lowered),
            Some(Binding::Value(TypeExpr::List(Box::new(union_type(
                item_types,
            ))))),
        ))
    }

    fn lower_list(
        &self,
        items: &[Expr],
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let mut lowered = Vec::with_capacity(items.len());
        let mut item_types = Vec::with_capacity(items.len());
        let expected_item =
            expected.and_then(|expected| match self.resolve_type_aliases(expected) {
                TypeExpr::List(item) => Some(*item),
                _ => None,
            });
        for item in items {
            let (item, binding) = self.lower_expr_expected(item, scope, expected_item.as_ref())?;
            lowered.push(item);
            item_types.push(binding_type(binding.as_ref()));
        }
        Ok((
            Expr::List(lowered),
            Some(Binding::Value(TypeExpr::List(Box::new(union_type(
                item_types,
            ))))),
        ))
    }

    fn lower_list_comprehension(
        &self,
        element: &Expr,
        clauses: &[ListComprehensionClause],
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let mut lowered_clauses = Vec::with_capacity(clauses.len());
        let mut previous_bindings = Vec::new();
        for clause in clauses {
            match clause {
                ListComprehensionClause::For { binding, iterable } => {
                    let (iterable, iterable_binding) = self.lower_expr(iterable, scope)?;
                    let item_ty = self
                        .iterable_item_type(&binding_type(iterable_binding.as_ref()), scope.span)?;
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
        Ok((
            Expr::ListComprehension {
                element: Box::new(element),
                clauses: lowered_clauses,
            },
            Some(Binding::Value(TypeExpr::List(Box::new(binding_type(
                binding.as_ref(),
            ))))),
        ))
    }

    fn lower_record(
        &self,
        entries: &[(AstString, Expr)],
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let mut lowered = Vec::with_capacity(entries.len());
        let mut fields = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            let expected_field =
                expected.and_then(|expected| match self.resolve_type_aliases(expected) {
                    TypeExpr::Object(fields) => fields
                        .into_iter()
                        .find(|field| field.name == *name)
                        .map(|field| field.ty),
                    _ => None,
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
        Ok((
            Expr::Record(lowered),
            Some(Binding::Value(TypeExpr::Object(fields))),
        ))
    }

    fn lower_assign(
        &self,
        target: &crate::ast::AssignTarget,
        expr: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        for step in &target.steps {
            if let AssignPathStep::Index(index) = step {
                self.lower_expr(index, scope)?;
            }
        }
        let target_expected = self.assignment_target_type(target, scope)?;
        let (lowered, binding) = self.lower_expr_expected(expr, scope, target_expected.as_ref())?;
        if target.steps.is_empty() {
            scope.bind(
                target.root.as_str(),
                binding.clone().unwrap_or(any_binding()),
            );
        } else {
            let value_ty = binding_type(binding.as_ref());
            scope.update_path(target, &value_ty)?;
        }
        Ok((
            Expr::Assign {
                target: target.clone(),
                expr: Box::new(lowered),
            },
            binding,
        ))
    }

    fn lower_if(
        &self,
        condition: &Expr,
        then_block: &Expr,
        else_block: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let condition = self.lower_expr(condition, scope)?.0;
        let mut then_scope = scope.clone();
        let (then_block, then_binding) =
            self.lower_expr_expected(then_block, &mut then_scope, expected)?;
        let mut else_scope = scope.clone();
        let (else_block, else_binding) =
            self.lower_expr_expected(else_block, &mut else_scope, expected)?;
        scope.join_branches(then_scope, else_scope);
        Ok((
            Expr::If {
                condition: Box::new(condition),
                then_block: Box::new(then_block),
                else_block: Box::new(else_block),
            },
            Some(Binding::Value(union_type(vec![
                binding_type(then_binding.as_ref()),
                binding_type(else_binding.as_ref()),
            ]))),
        ))
    }

    fn lower_for(
        &self,
        binding: &AstString,
        iterable: &Expr,
        body: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let (iterable, iterable_binding) = self.lower_expr(iterable, scope)?;
        let item_ty =
            self.iterable_item_type(&binding_type(iterable_binding.as_ref()), scope.span)?;
        let before = scope.clone();
        let mut body_scope = scope.clone();
        let previous = body_scope.bind(binding.as_str(), self.binding_for_type(&item_ty));
        let body = self.lower_expr(body, &mut body_scope)?.0;
        body_scope.restore(binding.as_str(), previous);
        scope.widen_loop(before, body_scope);
        Ok((
            Expr::For {
                binding: binding.clone(),
                iterable: Box::new(iterable),
                body: Box::new(body),
            },
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_while(
        &self,
        condition: &Expr,
        body: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let condition = self.lower_expr(condition, scope)?.0;
        let before = scope.clone();
        let mut body_scope = scope.clone();
        let body = self.lower_expr(body, &mut body_scope)?.0;
        scope.widen_loop(before, body_scope);
        Ok((
            Expr::While {
                condition: Box::new(condition),
                body: Box::new(body),
            },
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_start_process(
        &self,
        start: &crate::ast::ProcessStartExpr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
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
            let (lowered, binding) = self.lower_expr_expected(value, scope, Some(&param.ty))?;
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
        Ok((
            Expr::StartProcess(crate::ast::ProcessStartExpr {
                process: start.process.clone(),
                args: lowered_args,
            }),
            Some(Binding::Value(
                self.process_output_type(start.process.as_str()),
            )),
        ))
    }

    fn lower_process_ref(
        &self,
        process: &AstString,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let Some(process_ty) = self.process_types.get(process.as_str()) else {
            return Err(LinkError::UnknownProcess {
                name: process.to_string(),
                span: scope.span,
            });
        };
        Ok((
            Expr::ProcessRef {
                process: process.clone(),
            },
            Some(Binding::Value(process_ty.clone())),
        ))
    }

    fn lower_host_descriptor_constructor(
        &self,
        type_name: &AstString,
        input: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::HostDescriptorConstructor {
                type_name: type_name.clone(),
                input: Box::new(self.lower_expr(input, scope)?.0),
            },
            Some(Binding::Value(TypeExpr::Ref(type_name.clone()))),
        ))
    }

    fn lower_resource_ref(
        &self,
        resource: &ResourceRefExpr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let resource = self.validate_resource_ref(resource, scope.span)?;
        Ok((
            Expr::ResourceRef(resource.clone()),
            Some(Binding::Resource {
                resource_type: resource.resource_type.to_string(),
            }),
        ))
    }

    fn lower_receiver_call(
        &self,
        receiver: &Expr,
        operation: &AstString,
        args: &[Expr],
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        if let Some(mut path) = module_path_for_expr(receiver) {
            path.push(operation.clone());
            if let Some(constructor) = self.surface.resources.resolve_value_constructor(&path) {
                if args.len() != 1 {
                    return Err(LinkError::IncompatibleConstructorInput {
                        path: module_path_key(&path),
                        expected: format_type_expr(&constructor.input_ty),
                        actual: format!("{} arguments", args.len()),
                        span: scope.span,
                    });
                }
                let (input, input_binding) =
                    self.lower_expr_expected(&args[0], scope, Some(&constructor.input_ty))?;
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
            validate_trigger_operation_subscription_key(trigger_operation, args, scope.span)?;
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
            let expected_arg = expected_call_arg_type(&operation_binding.input_ty, args.len());
            let (arg, binding) = self.lower_expr_expected(arg, scope, expected_arg)?;
            lowered_args.push(arg);
            arg_types.push(binding_type(binding.as_ref()));
        }
        let actual_input = call_input_type(arg_types);
        if !self.is_type_assignable(&actual_input, &operation_binding.input_ty) {
            return Err(LinkError::IncompatibleOperationInput {
                operation: operation.to_string(),
                expected: format_type_expr(&self.resolve_type_aliases(&operation_binding.input_ty)),
                actual: format_type_expr(&self.resolve_type_aliases(&actual_input)),
                span: scope.span,
            });
        }
        Ok((
            Expr::ReceiverCall {
                receiver: Box::new(lowered_receiver),
                operation: operation.clone(),
                args: lowered_args,
            },
            Some(Binding::Value(
                self.operation_call_output_type(&operation_binding, args),
            )),
        ))
    }

    fn lower_await(
        &self,
        inner: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let (inner, binding) = self.lower_expr_expected(inner, scope, expected)?;
        Ok((Expr::Await(Box::new(inner)), binding))
    }

    fn lower_sleep_for(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.ensure_feature(self.surface.abilities.sleep, "sleep", scope.span)?;
        Ok((
            Expr::SleepFor(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_sleep_until(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.ensure_feature(self.surface.abilities.sleep, "sleep", scope.span)?;
        Ok((
            Expr::SleepUntil(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_wait_signal(
        &self,
        name: &AstString,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.ensure_feature(
            self.surface.abilities.process_signals,
            "process signals",
            scope.span,
        )?;
        if !scope.process_body {
            return Err(LinkError::ProcessLifecycleOutsideProcess {
                keyword: self.wait_signal_keyword(),
                span: scope.span,
            });
        }
        Ok((
            Expr::WaitSignal { name: name.clone() },
            Some(Binding::Value(TypeExpr::Any)),
        ))
    }

    fn lower_signal_run(
        &self,
        run: &Expr,
        name: &AstString,
        payload: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        self.ensure_feature(
            self.surface.abilities.process_signals,
            "process signals",
            scope.span,
        )?;
        Ok(
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
            ),
        )
    }

    fn lower_result_unwrap(
        &self,
        inner: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let (inner, binding) = self.lower_expr_expected(inner, scope, expected)?;
        Ok((Expr::ResultUnwrap(Box::new(inner)), binding))
    }

    fn lower_cancel(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Cancel(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Any)),
        ))
    }

    fn lower_print(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Print(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_yield(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Yield(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_wake(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Wake(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_finish(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let expected_return = scope.expected_return.clone();
        let (inner, binding) = self.lower_expr_expected(inner, scope, expected_return.as_ref())?;
        let finish_ty = binding_type(binding.as_ref());
        Ok((
            Expr::Finish(Box::new(inner)),
            Some(Binding::Value(finish_ty)),
        ))
    }

    fn lower_fail(
        &self,
        inner: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Fail(Box::new(self.lower_expr(inner, scope)?.0)),
            Some(Binding::Value(TypeExpr::Null)),
        ))
    }

    fn lower_builtin_call(
        &self,
        name: &AstString,
        args: &[Expr],
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
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
        Ok((
            Expr::BuiltinCall {
                name: name.clone(),
                args: lowered_args.into_iter().map(|(expr, _)| expr).collect(),
            },
            Some(Binding::Value(shaping_builtin_return_type(
                name.as_str(),
                &arg_types,
            ))),
        ))
    }

    fn lower_function(
        &self,
        function: &crate::ast::FunctionExpr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        for capture in &function.captures {
            if scope.get(capture).is_none() {
                return Err(LinkError::UnknownName {
                    name: capture.to_string(),
                    span: scope.span,
                });
            }
        }
        let mut function_scope = Scope::new(scope.process_body, scope.span);
        for capture in &function.captures {
            function_scope.bind(capture, any_binding());
        }
        for param in &function.params {
            function_scope.bind(param, any_binding());
        }
        if let Some(name) = &function.name {
            function_scope.bind(name, any_binding());
        }
        let body = self.lower_expr(&function.body, &mut function_scope)?.0;
        Ok((
            Expr::Function(Box::new(crate::ast::FunctionExpr {
                name: function.name.clone(),
                params: function.params.clone(),
                captures: function.captures.clone(),
                body: Box::new(body),
            })),
            Some(any_binding()),
        ))
    }

    fn lower_call(
        &self,
        function: &Expr,
        args: &[Expr],
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Call {
                function: Box::new(self.lower_expr(function, scope)?.0),
                args: args
                    .iter()
                    .map(|arg| self.lower_expr(arg, scope).map(|value| value.0))
                    .collect::<Result<_, _>>()?,
            },
            Some(any_binding()),
        ))
    }

    fn lower_map(
        &self,
        items: &Expr,
        function: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Map {
                items: Box::new(self.lower_expr(items, scope)?.0),
                function: Box::new(self.lower_expr(function, scope)?.0),
            },
            Some(any_binding()),
        ))
    }

    fn lower_field(
        &self,
        target: &Expr,
        field: &AstString,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let (target, binding) = self.lower_expr(target, scope)?;
        let ty = self.field_type(&binding_type(binding.as_ref()), field.as_str(), scope.span)?;
        Ok((
            Expr::Field {
                target: Box::new(target),
                field: field.clone(),
            },
            Some(Binding::Value(ty)),
        ))
    }

    fn lower_index(
        &self,
        target: &Expr,
        index: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let (target, target_binding) = self.lower_expr(target, scope)?;
        let index = self.lower_expr(index, scope)?.0;
        Ok((
            Expr::Index {
                target: Box::new(target),
                index: Box::new(index),
            },
            Some(Binding::Value(self.index_type(
                &binding_type(target_binding.as_ref()),
                scope.span,
            )?)),
        ))
    }

    fn lower_unary(
        &self,
        op: &crate::ast::UnaryOp,
        expr: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Unary {
                op: *op,
                expr: Box::new(self.lower_expr(expr, scope)?.0),
            },
            Some(Binding::Value(match op {
                crate::ast::UnaryOp::Not => TypeExpr::Bool,
                crate::ast::UnaryOp::Negate => TypeExpr::Float,
            })),
        ))
    }

    fn lower_binary(
        &self,
        left: &Expr,
        op: &crate::ast::BinaryOp,
        right: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let (left, left_binding) = self.lower_expr(left, scope)?;
        let (right, right_binding) = self.lower_expr(right, scope)?;
        self.validate_binary_operands(
            *op,
            &binding_type(left_binding.as_ref()),
            &binding_type(right_binding.as_ref()),
            scope.span,
        )?;
        Ok((
            Expr::Binary {
                left: Box::new(left),
                op: *op,
                right: Box::new(right),
            },
            Some(Binding::Value(binary_return_type(*op))),
        ))
    }

    fn lower_try_expr(
        &self,
        exception: &crate::ast::TryExpr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        let before = scope.clone();
        let mut try_scope = before.clone();
        let body = self.lower_expr(&exception.body, &mut try_scope)?.0;
        let catch = if let Some(catch) = &exception.catch {
            let mut catch_scope = before.clone();
            let previous = catch_scope.bind(&catch.binding, any_binding());
            let body = self.lower_expr(&catch.body, &mut catch_scope)?.0;
            catch_scope.restore(&catch.binding, previous);
            scope.join_branches(try_scope, catch_scope);
            Some(crate::ast::CatchClause {
                binding: catch.binding.clone(),
                body: Box::new(body),
            })
        } else {
            *scope = try_scope;
            None
        };
        let finally = exception
            .finally
            .as_ref()
            .map(|finally| {
                self.lower_expr(finally, scope)
                    .map(|value| Box::new(value.0))
            })
            .transpose()?;
        Ok((
            Expr::Try(Box::new(crate::ast::TryExpr {
                body: Box::new(body),
                catch,
                finally,
            })),
            Some(any_binding()),
        ))
    }

    fn lower_throw_expr(
        &self,
        value: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Throw(Box::new(self.lower_expr(value, scope)?.0)),
            Some(any_binding()),
        ))
    }

    fn lower_return_expr(
        &self,
        value: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::Return(Box::new(self.lower_expr(value, scope)?.0)),
            Some(any_binding()),
        ))
    }

    fn lower_javascript_unary(
        &self,
        op: &crate::ast::JavaScriptUnaryOp,
        expr: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::JavaScriptUnary {
                op: *op,
                expr: Box::new(self.lower_expr(expr, scope)?.0),
            },
            Some(any_binding()),
        ))
    }

    fn lower_javascript_binary(
        &self,
        left: &Expr,
        op: &crate::ast::JavaScriptBinaryOp,
        right: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::JavaScriptBinary {
                left: Box::new(self.lower_expr(left, scope)?.0),
                op: *op,
                right: Box::new(self.lower_expr(right, scope)?.0),
            },
            Some(any_binding()),
        ))
    }

    fn lower_javascript_logical(
        &self,
        left: &Expr,
        op: &crate::ast::JavaScriptLogicalOp,
        right: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Option<Binding>), LinkError> {
        Ok((
            Expr::JavaScriptLogical {
                left: Box::new(self.lower_expr(left, scope)?.0),
                op: *op,
                right: Box::new(self.lower_expr(right, scope)?.0),
            },
            Some(any_binding()),
        ))
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
