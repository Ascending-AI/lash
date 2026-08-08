impl<'module> Linker<'module> {
    fn validate_process_arg_binding(
        &self,
        process: &str,
        arg: &str,
        expected_ty: &TypeExpr,
        actual: Option<&Binding>,
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        if let Some(expected_resource) = self.resource_type_for_type(expected_ty) {
            return match actual {
                Some(Binding::Resource { resource_type })
                    if *resource_type == expected_resource =>
                {
                    Ok(())
                }
                Some(Binding::Resource { resource_type }) => {
                    Err(LinkError::IncompatibleProcessArgument {
                        process: process.into(),
                        arg: arg.into(),
                        expected: expected_resource.into(),
                        actual: resource_type.as_str().into(),
                        span,
                    })
                }
                _ => Err(LinkError::IncompatibleProcessArgument {
                    process: process.into(),
                    arg: arg.into(),
                    expected: expected_resource.into(),
                    actual: "value".into(),
                    span,
                }),
            };
        }
        let actual_ty = binding_type(actual);
        if self.is_type_assignable(&actual_ty, expected_ty) {
            Ok(())
        } else {
            Err(LinkError::IncompatibleProcessArgument {
                process: process.into(),
                arg: arg.into(),
                expected: format_type_expr(&self.resolve_type_aliases(expected_ty))
                    .into_boxed_str(),
                actual: format_type_expr(&self.resolve_type_aliases(&actual_ty)).into_boxed_str(),
                span,
            })
        }
    }

    fn validate_trigger_operation_args(
        &self,
        operation: crate::TriggerHostOperation,
        args: &[Expr],
        scope: &Scope,
    ) -> Result<TypeExpr, LinkError> {
        match operation {
            crate::TriggerHostOperation::Register
            | crate::TriggerHostOperation::Update
            | crate::TriggerHostOperation::Revive => {
                let call = crate::register_call_args(args)
                    .map_err(|_| LinkError::InvalidTriggerRegistration { span: scope.span })?;
                if let Some(key) = call.subscription_key {
                    validate_trigger_subscription_key_literal(key, scope.span)?;
                }
                let source_ty = self.infer_expr_type(call.source, &mut scope.clone())?;
                let event_ty = self
                    .surface
                    .resources
                    .trigger_source_event(&source_ty)
                    .ok_or_else(|| LinkError::UnknownTriggerEventType {
                        source_ty: format_type_expr(&source_ty),
                        span: scope.span,
                    })?;
                let target_ty = self.infer_expr_type(call.target, &mut scope.clone())?;
                let params = self.trigger_target_params(call.target, &target_ty, scope.span)?;
                let mut validation_scope = scope.clone();
                self.lower_trigger_input_record(
                    trigger_target_process_label(call.target).as_str(),
                    &params,
                    &event_ty,
                    call.inputs,
                    &mut validation_scope,
                )?;
                if matches!(
                    operation,
                    crate::TriggerHostOperation::Update | crate::TriggerHostOperation::Revive
                ) {
                    let expected_revision = trigger_operation_record_entry(args, "expected_revision")
                        .ok_or(LinkError::InvalidTriggerRegistration { span: scope.span })?;
                    let revision_ty = self
                        .infer_expr_type_expected(
                            expected_revision,
                            &mut scope.clone(),
                            Some(&TypeExpr::Int),
                        )?;
                    if !self.is_type_assignable(&revision_ty, &TypeExpr::Int) {
                        return Err(LinkError::IncompatibleOperationInput {
                            operation: operation.receiver_method().to_string(),
                            expected: format_type_expr(&TypeExpr::Int),
                            actual: format_type_expr(&revision_ty),
                            span: scope.span,
                        });
                    }
                }
                Ok(operation.output_ty())
            }
            crate::TriggerHostOperation::List => {
                let call = crate::list_call_args(args)
                    .map_err(|_| LinkError::InvalidTriggerList { span: scope.span })?;
                for (name, expr) in call.entries {
                    match name.as_str() {
                        "target" => {
                            let target_ty = self.infer_expr_type(expr, &mut scope.clone())?;
                            if !matches!(target_ty, TypeExpr::Process { .. }) {
                                return Err(LinkError::InvalidTriggerTarget {
                                    actual: format_type_expr(&target_ty),
                                    span: scope.span,
                                });
                            }
                        }
                        "name" | "source_type" => {
                            let filter_ty = self.infer_expr_type(expr, &mut scope.clone())?;
                            if !self.is_type_assignable(&filter_ty, &TypeExpr::Str) {
                                return Err(LinkError::IncompatibleOperationInput {
                                    operation: operation.receiver_method().to_string(),
                                    expected: format_type_expr(&TypeExpr::Str),
                                    actual: format_type_expr(&filter_ty),
                                    span: scope.span,
                                });
                            }
                        }
                        "enabled" => {
                            let filter_ty = self.infer_expr_type(expr, &mut scope.clone())?;
                            if !self.is_type_assignable(&filter_ty, &TypeExpr::Bool) {
                                return Err(LinkError::IncompatibleOperationInput {
                                    operation: operation.receiver_method().to_string(),
                                    expected: format_type_expr(&TypeExpr::Bool),
                                    actual: format_type_expr(&filter_ty),
                                    span: scope.span,
                                });
                            }
                        }
                        _ => unreachable!("list_call_args rejects unknown trigger filters"),
                    }
                }
                Ok(operation.output_ty())
            }
            _ => unreachable!("only definition/list operations use specialized trigger validation"),
        }
    }

    fn lower_trigger_operation_args(
        &self,
        operation: crate::TriggerHostOperation,
        args: &[Expr],
        scope: &mut Scope,
    ) -> Result<(Vec<Expr>, TypeExpr), LinkError> {
        match operation {
            crate::TriggerHostOperation::Register
            | crate::TriggerHostOperation::Update
            | crate::TriggerHostOperation::Revive => {
                self.lower_trigger_registration_args(operation, args, scope)
            }
            crate::TriggerHostOperation::List => {
                let call = crate::list_call_args(args)
                    .map_err(|_| LinkError::InvalidTriggerList { span: scope.span })?;
                let mut entries = Vec::with_capacity(call.entries.len());
                for (name, expr) in call.entries {
                    match name.as_str() {
                        "target" => {
                            let target_ty = self.infer_expr_type(expr, &mut scope.clone())?;
                            if !matches!(target_ty, TypeExpr::Process { .. }) {
                                return Err(LinkError::InvalidTriggerTarget {
                                    actual: format_type_expr(&target_ty),
                                    span: scope.span,
                                });
                            }
                        }
                        "name" | "source_type" => {
                            let filter_ty = self.infer_expr_type(expr, &mut scope.clone())?;
                            if !self.is_type_assignable(&filter_ty, &TypeExpr::Str) {
                                return Err(LinkError::IncompatibleOperationInput {
                                    operation: operation.receiver_method().to_string(),
                                    expected: format_type_expr(&TypeExpr::Str),
                                    actual: format_type_expr(&filter_ty),
                                    span: scope.span,
                                });
                            }
                        }
                        "enabled" => {
                            let filter_ty = self.infer_expr_type(expr, &mut scope.clone())?;
                            if !self.is_type_assignable(&filter_ty, &TypeExpr::Bool) {
                                return Err(LinkError::IncompatibleOperationInput {
                                    operation: operation.receiver_method().to_string(),
                                    expected: format_type_expr(&TypeExpr::Bool),
                                    actual: format_type_expr(&filter_ty),
                                    span: scope.span,
                                });
                            }
                        }
                        _ => unreachable!("list_call_args rejects unknown trigger filters"),
                    }
                    entries.push((name.clone(), self.lower_expr(expr, scope)?.0));
                }
                Ok((vec![Expr::Record(entries)], operation.output_ty()))
            }
            _ => unreachable!("only definition/list operations use specialized trigger lowering"),
        }
    }

    fn lower_trigger_registration_args(
        &self,
        operation: crate::TriggerHostOperation,
        args: &[Expr],
        scope: &mut Scope,
    ) -> Result<(Vec<Expr>, TypeExpr), LinkError> {
        let call = crate::register_call_args(args)
            .map_err(|_| LinkError::InvalidTriggerRegistration { span: scope.span })?;
        let source_ty = self.infer_expr_type(call.source, &mut scope.clone())?;
        let event_ty = self
            .surface
            .resources
            .trigger_source_event(&source_ty)
            .ok_or_else(|| LinkError::UnknownTriggerEventType {
                source_ty: format_type_expr(&source_ty),
                span: scope.span,
            })?;
        let target_ty = self.infer_expr_type(call.target, &mut scope.clone())?;
        let params = self.trigger_target_params(call.target, &target_ty, scope.span)?;
        let process = trigger_target_process_label(call.target);

        let source = self.lower_expr(call.source, scope)?.0;
        let target = self.lower_expr(call.target, scope)?.0;
        let inputs = self.lower_trigger_input_record(
            process.as_str(),
            &params,
            &event_ty,
            call.inputs,
            scope,
        )?;
        let mut entries = vec![
            ("source".into(), source),
            ("target".into(), target),
            ("inputs".into(), inputs),
        ];
        if let Some(name) = call.name {
            entries.push(("name".into(), self.lower_expr(name, scope)?.0));
        }
        if let Some(subscription_key) = call.subscription_key {
            entries.push((
                "subscription_key".into(),
                self.lower_expr(subscription_key, scope)?.0,
            ));
        }
        if matches!(
            operation,
            crate::TriggerHostOperation::Update | crate::TriggerHostOperation::Revive
        ) {
            let expected_revision = trigger_operation_record_entry(args, "expected_revision")
                .ok_or(LinkError::InvalidTriggerRegistration { span: scope.span })?;
            entries.push((
                "expected_revision".into(),
                self.lower_expr_expected(expected_revision, scope, Some(&TypeExpr::Int))?
                    .0,
            ));
        }
        Ok((vec![Expr::Record(entries)], operation.output_ty()))
    }

    fn lower_trigger_input_record(
        &self,
        process: &str,
        params: &[ProcessParam],
        event_ty: &TypeExpr,
        inputs: &Expr,
        scope: &mut Scope,
    ) -> Result<Expr, LinkError> {
        let Expr::Record(entries) = inputs else {
            return Err(LinkError::InvalidTriggerInputs { span: scope.span });
        };
        let mut seen = BTreeSet::new();
        let mut saw_event = false;
        let mut lowered = Vec::with_capacity(entries.len());
        for (name, value) in entries {
            if !seen.insert(name.to_string()) {
                return Err(LinkError::DuplicateTriggerInput {
                    input: name.to_string(),
                    span: scope.span,
                });
            }
            let Some(param) = params.iter().find(|param| param.name == *name) else {
                return Err(LinkError::UnknownTriggerInput {
                    process: process.to_string(),
                    input: name.to_string(),
                    span: scope.span,
                });
            };
            if is_trigger_event_projection_expr(value) {
                return Err(LinkError::TriggerEventProjection { span: scope.span });
            }
            if is_trigger_event_expr(value) {
                saw_event = true;
                if !self.is_type_assignable(event_ty, &param.ty) {
                    return Err(LinkError::TriggerEventMismatch {
                        event: format_type_expr(&self.resolve_type_aliases(event_ty)),
                        input_name: name.to_string(),
                        input: format_type_expr(&self.resolve_type_aliases(&param.ty)),
                        span: scope.span,
                    });
                }
                lowered.push((name.clone(), crate::trigger_event_placeholder_expr()));
                continue;
            }
            let (lowered_value, binding) =
                self.lower_expr_expected(value, scope, Some(&param.ty))?;
            self.validate_process_arg_binding(
                process,
                name.as_str(),
                &param.ty,
                binding.as_ref(),
                scope.span,
            )?;
            lowered.push((name.clone(), lowered_value));
        }
        for param in params {
            if !seen.contains(param.name.as_str()) {
                return Err(LinkError::MissingTriggerInput {
                    process: process.to_string(),
                    input: param.name.to_string(),
                    span: scope.span,
                });
            }
        }
        if !saw_event {
            return Err(LinkError::MissingTriggerEventInput { span: scope.span });
        }
        Ok(Expr::Record(lowered))
    }

    fn trigger_target_params(
        &self,
        target: &Expr,
        target_ty: &TypeExpr,
        span: Option<Span>,
    ) -> Result<Vec<ProcessParam>, LinkError> {
        if let Some(process_name) = trigger_target_process_name(target)
            && let Some(process) = self.program.process(process_name.as_str())
        {
            return Ok(process.params.clone());
        }
        let TypeExpr::Process {
            input, input_count, ..
        } = target_ty
        else {
            return Err(LinkError::InvalidTriggerTarget {
                actual: format_type_expr(target_ty),
                span,
            });
        };
        match (input_count, input.as_ref()) {
            (0, _) => Ok(Vec::new()),
            (count, TypeExpr::Object(fields)) if *count > 1 => Ok(fields
                .iter()
                .map(|field| ProcessParam {
                    name: field.name.clone(),
                    ty: field.ty.clone(),
                })
                .collect()),
            _ => Err(LinkError::InvalidTriggerTarget {
                actual: format_type_expr(target_ty),
                span,
            }),
        }
    }

    fn infer_process_output(
        &self,
        process: &ProcessDecl,
        span: Option<Span>,
    ) -> Result<TypeExpr, LinkError> {
        let mut scope = Scope::new(true, span);
        scope.expected_return = process.return_ty.clone();
        for param in &process.params {
            scope.bind(param.name.as_str(), self.binding_for_type(&param.ty));
        }
        scope.bind("input", Binding::Value(process_input_type(process)));
        scope.bind("inputs", Binding::Value(process_input_record_type(process)));
        let completion = self.infer_completion(&process.body, &mut scope)?;
        let mut outputs = completion.finishes;
        if completion.can_fallthrough {
            outputs.push(TypeExpr::Null);
        }
        Ok(union_type(outputs))
    }

    fn infer_completion(&self, expr: &Expr, scope: &mut Scope) -> Result<Completion, LinkError> {
        match expr {
            Expr::LabelAnnotated { expr, .. } => self.infer_completion(expr, scope),
            Expr::Finish(value) => {
                let expected_return = scope.expected_return.clone();
                Ok(Completion {
                    finishes: vec![self.infer_expr_type_expected(
                        value,
                        scope,
                        expected_return.as_ref(),
                    )?],
                    can_fallthrough: false,
                })
            }
            Expr::Fail(_) => Ok(Completion {
                finishes: Vec::new(),
                can_fallthrough: false,
            }),
            Expr::Block(expressions) => {
                let mut finishes = Vec::new();
                let mut can_fallthrough = true;
                for expression in expressions {
                    if !can_fallthrough {
                        break;
                    }
                    let completion = self.infer_completion(expression, scope)?;
                    finishes.extend(completion.finishes);
                    can_fallthrough = completion.can_fallthrough;
                }
                Ok(Completion {
                    finishes,
                    can_fallthrough,
                })
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                self.infer_expr_type(condition, scope)?;
                let mut then_scope = scope.clone();
                let then_completion = self.infer_completion(then_block, &mut then_scope)?;
                let mut else_scope = scope.clone();
                let else_completion = self.infer_completion(else_block, &mut else_scope)?;
                scope.join_branches(then_scope, else_scope);
                let mut finishes = then_completion.finishes;
                finishes.extend(else_completion.finishes);
                Ok(Completion {
                    finishes,
                    can_fallthrough: then_completion.can_fallthrough
                        || else_completion.can_fallthrough,
                })
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr_type(iterable, scope)?;
                let item_ty = self.iterable_item_type(&iterable_ty, scope.span)?;
                let before = scope.clone();
                let mut body_scope = scope.clone();
                let previous = body_scope.bind(
                    binding.as_str(),
                    self.binding_for_type(&item_ty),
                );
                let mut completion = self.infer_completion(body, &mut body_scope)?;
                body_scope.restore(binding.as_str(), previous);
                scope.widen_loop(before, body_scope);
                completion.can_fallthrough = true;
                Ok(completion)
            }
            Expr::While { condition, body } => {
                self.infer_expr_type(condition, scope)?;
                let before = scope.clone();
                let mut body_scope = scope.clone();
                let mut completion = self.infer_completion(body, &mut body_scope)?;
                scope.widen_loop(before, body_scope);
                completion.can_fallthrough = true;
                Ok(completion)
            }
            Expr::Assign { target, expr } => {
                let expected = self.assignment_target_type(target, scope)?;
                let ty = self.infer_expr_type_expected(expr, scope, expected.as_ref())?;
                if target.steps.is_empty() {
                    scope.bind(target.root.as_str(), self.binding_for_type(&ty));
                } else {
                    scope.update_path(target, &ty)?;
                }
                Ok(Completion::fallthrough())
            }
            other => {
                self.infer_expr_type(other, scope)?;
                Ok(Completion::fallthrough())
            }
        }
    }

    fn infer_expr_type(&self, expr: &Expr, scope: &mut Scope) -> Result<TypeExpr, LinkError> {
        self.infer_expr_type_expected(expr, scope, None)
    }

    fn infer_expr_type_expected(
        &self,
        expr: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<TypeExpr, LinkError> {
        if let (Some(facts), Some(expected)) = (&self.expected_type_facts, expected) {
            facts.borrow_mut().by_expression.insert(
                expr as *const Expr as usize,
                self.resolve_type_aliases(expected),
            );
        }
        self.reject_trigger_event_special_form(expr, scope.span)?;
        self.validate_expected_literals(expr, expected, scope.span)?;
        if matches!(expr, Expr::Variable(_) | Expr::Field { .. })
            && let Some(resource) = self.resolve_module_expr(expr, scope)
        {
            return Ok(TypeExpr::Ref(resource.resource_type));
        }
        Ok(match expr {
            Expr::LabelAnnotated { expr, .. } => {
                self.infer_expr_type_expected(expr, scope, expected)?
            }
            Expr::Block(expressions) => {
                let mut last = TypeExpr::Null;
                let last_index = expressions.len().saturating_sub(1);
                for (index, expression) in expressions.iter().enumerate() {
                    last = self.infer_expr_type_expected(
                        expression,
                        scope,
                        (index == last_index).then_some(expected).flatten(),
                    )?;
                }
                last
            }
            Expr::Null
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Break
            | Expr::Continue => literal_type(expr),
            Expr::TypeLiteral(_) => {
                binding_type(self.closed_schema_witness_binding(expr).as_ref())
            }
            Expr::Variable(name) => {
                if let Some(binding) = scope.get(name) {
                    binding_type(Some(&binding))
                } else if let Some(process_ty) = self.process_types.get(name.as_str()) {
                    process_ty.clone()
                } else {
                    return Err(LinkError::UnknownName {
                        name: name.to_string(),
                        span: scope.span,
                    });
                }
            }
            Expr::ProcessRef { process } => self
                .process_types
                .get(process.as_str())
                .cloned()
                .ok_or_else(|| LinkError::UnknownProcess {
                    name: process.to_string(),
                    span: scope.span,
                })?,
            Expr::HostDescriptorConstructor { type_name, .. } => TypeExpr::Ref(type_name.clone()),
            Expr::Tuple(items) => TypeExpr::List(Box::new(union_type(
                items
                    .iter()
                    .map(|item| {
                        let expected_item = expected.and_then(|expected| {
                            match self.resolve_type_aliases(expected) {
                                TypeExpr::List(item) => Some(*item),
                                _ => None,
                            }
                        });
                        self.infer_expr_type_expected(item, scope, expected_item.as_ref())
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ))),
            Expr::List(items) => TypeExpr::List(Box::new(union_type(
                items
                    .iter()
                    .map(|item| {
                        let expected_item = expected.and_then(|expected| {
                            match self.resolve_type_aliases(expected) {
                                TypeExpr::List(item) => Some(*item),
                                _ => None,
                            }
                        });
                        self.infer_expr_type_expected(item, scope, expected_item.as_ref())
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ))),
            Expr::ListComprehension { element, clauses } => {
                let mut previous_bindings = Vec::new();
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            let iterable_ty = self.infer_expr_type(iterable, scope)?;
                            let item_ty = self.iterable_item_type(&iterable_ty, scope.span)?;
                            previous_bindings.push((
                                binding.to_string(),
                                scope.bind(binding.as_str(), self.binding_for_type(&item_ty)),
                            ));
                        }
                        ListComprehensionClause::If { condition } => {
                            self.infer_expr_type(condition, scope)?;
                        }
                    }
                }
                let element_ty = self.infer_expr_type(element, scope)?;
                for (name, previous) in previous_bindings.into_iter().rev() {
                    scope.restore(name.as_str(), previous);
                }
                TypeExpr::List(Box::new(element_ty))
            }
            Expr::Record(entries) => TypeExpr::Object(
                entries
                    .iter()
                    .map(|(name, value)| {
                        let expected_field = expected.and_then(|expected| {
                            match self.resolve_type_aliases(expected) {
                                TypeExpr::Object(fields) => fields
                                    .into_iter()
                                    .find(|field| field.name == *name)
                                    .map(|field| field.ty),
                                _ => None,
                            }
                        });
                        Ok(TypeField {
                            name: name.clone(),
                            ty: self.infer_expr_type_expected(
                                value,
                                scope,
                                expected_field.as_ref(),
                            )?,
                            optional: false,
                        })
                    })
                    .collect::<Result<Vec<_>, LinkError>>()?,
            ),
            Expr::Assign { target, expr } => {
                let target_expected = self.assignment_target_type(target, scope)?;
                let ty = self.infer_expr_type_expected(expr, scope, target_expected.as_ref())?;
                if target.steps.is_empty() {
                    scope.bind(target.root.as_str(), self.binding_for_type(&ty));
                } else {
                    scope.update_path(target, &ty)?;
                }
                ty
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                self.infer_expr_type(condition, scope)?;
                let mut then_scope = scope.clone();
                let then_ty =
                    self.infer_expr_type_expected(then_block, &mut then_scope, expected)?;
                let mut else_scope = scope.clone();
                let else_ty =
                    self.infer_expr_type_expected(else_block, &mut else_scope, expected)?;
                scope.join_branches(then_scope, else_scope);
                union_type(vec![then_ty, else_ty])
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                let iterable_ty = self.infer_expr_type(iterable, scope)?;
                let item_ty = self.iterable_item_type(&iterable_ty, scope.span)?;
                let before = scope.clone();
                let mut body_scope = scope.clone();
                let previous = body_scope.bind(
                    binding.as_str(),
                    self.binding_for_type(&item_ty),
                );
                self.infer_expr_type(body, &mut body_scope)?;
                body_scope.restore(binding.as_str(), previous);
                scope.widen_loop(before, body_scope);
                TypeExpr::Null
            }
            Expr::While { condition, body } => {
                self.infer_expr_type(condition, scope)?;
                let before = scope.clone();
                let mut body_scope = scope.clone();
                self.infer_expr_type(body, &mut body_scope)?;
                scope.widen_loop(before, body_scope);
                TypeExpr::Null
            }
            Expr::StartProcess(start) => self.process_output_type(start.process.as_str()),
            Expr::ResourceRef(resource) => TypeExpr::Ref(resource.resource_type.clone()),
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
                        return Ok(constructor.output_ty.clone());
                    }
                }
                let resolved_receiver = self
                    .resolve_module_operation_expr(receiver, operation)
                    .or_else(|| self.resolve_module_expr(receiver, scope));
                let (resource_type, receiver_alias) =
                    if let Some(resource) = resolved_receiver.as_ref() {
                        (
                            resource.resource_type.to_string(),
                            Some(resource.alias.to_string()),
                        )
                    } else {
                        let receiver_ty = self.infer_expr_type(receiver, scope)?;
                        (
                            self.resource_type_for_type(&receiver_ty).ok_or_else(|| {
                                LinkError::UnresolvedReceiver {
                                    operation: operation.to_string(),
                                    suggestions: self
                                        .surface
                                        .resources
                                        .operation_suggestions_for_operation(operation.as_str()),
                                    span: scope.span,
                                }
                            })?,
                            None,
                        )
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
                let binding = self
                    .surface
                    .resources
                    .resolve_operation(&resource_type, operation)
                    .ok_or_else(|| LinkError::UnknownResourceOperation {
                        resource_type: resource_type.clone(),
                        operation: operation.to_string(),
                        suggestions: self
                            .surface
                            .resources
                            .operation_suggestions_for_resource_type(&resource_type),
                        span: scope.span,
                    })?;
                if crate::is_trigger_resource_type(&resource_type)
                    && let Some(trigger_operation) =
                        crate::TriggerHostOperation::from_receiver_method(operation.as_str())
                {
                    validate_trigger_operation_subscription_key(
                        trigger_operation,
                        args,
                        scope.span,
                    )?;
                    if matches!(
                        trigger_operation,
                        crate::TriggerHostOperation::Register
                            | crate::TriggerHostOperation::List
                            | crate::TriggerHostOperation::Update
                            | crate::TriggerHostOperation::Revive
                    ) {
                        self.validate_trigger_operation_args(trigger_operation, args, scope)?
                    } else {
                        let mut arg_types = Vec::with_capacity(args.len());
                        for arg in args {
                            let expected_arg = expected_call_arg_type(&binding.input_ty, args.len());
                            arg_types.push(self.infer_expr_type_expected(arg, scope, expected_arg)?);
                        }
                        let actual_input = call_input_type(arg_types);
                        if !self.is_type_assignable(&actual_input, &binding.input_ty) {
                            return Err(LinkError::IncompatibleOperationInput {
                                operation: operation.to_string(),
                                expected: format_type_expr(
                                    &self.resolve_type_aliases(&binding.input_ty),
                                ),
                                actual: format_type_expr(&self.resolve_type_aliases(&actual_input)),
                                span: scope.span,
                            });
                        }
                        self.operation_call_output_type(binding, args)
                    }
                } else {
                    let mut arg_types = Vec::with_capacity(args.len());
                    for arg in args {
                        let expected_arg =
                            expected_call_arg_type(&binding.input_ty, args.len());
                        arg_types.push(self.infer_expr_type_expected(
                            arg,
                            scope,
                            expected_arg,
                        )?);
                    }
                    let actual_input = call_input_type(arg_types);
                    if !self.is_type_assignable(&actual_input, &binding.input_ty) {
                        return Err(LinkError::IncompatibleOperationInput {
                            operation: operation.to_string(),
                            expected: format_type_expr(
                                &self.resolve_type_aliases(&binding.input_ty),
                            ),
                            actual: format_type_expr(&self.resolve_type_aliases(&actual_input)),
                            span: scope.span,
                        });
                    }
                    self.operation_call_output_type(binding, args)
                }
            }
            Expr::Await(inner) => self.infer_expr_type_expected(inner, scope, expected)?,
            Expr::ResultUnwrap(inner) => self.infer_expr_type_expected(inner, scope, expected)?,
            Expr::SleepFor(_) | Expr::SleepUntil(_) => TypeExpr::Null,
            Expr::WaitSignal { .. } => TypeExpr::Any,
            Expr::SignalRun { .. }
            | Expr::Cancel(_)
            | Expr::Print(_)
            | Expr::Yield(_)
            | Expr::Wake(_)
            | Expr::Fail(_) => TypeExpr::Null,
            Expr::Finish(inner) => {
                let return_expected = scope.expected_return.clone();
                self.infer_expr_type_expected(inner, scope, return_expected.as_ref())?
            }
            Expr::BuiltinCall { name, .. } => builtin_return_type(name.as_str()),
            Expr::Field { target, field } => {
                self.field_type(&self.infer_expr_type(target, scope)?, field, scope.span)?
            }
            Expr::Index { target, .. } => {
                self.index_type(&self.infer_expr_type(target, scope)?, scope.span)?
            }
            Expr::Unary { op, .. } => match op {
                crate::ast::UnaryOp::Not => TypeExpr::Bool,
                crate::ast::UnaryOp::Negate => TypeExpr::Float,
            },
            Expr::Binary { left, op, right } => {
                let left = self.infer_expr_type(left, scope)?;
                let right = self.infer_expr_type(right, scope)?;
                self.validate_binary_operands(*op, &left, &right, scope.span)?;
                binary_return_type(*op)
            }
        })
    }
}

fn trigger_operation_record_entry<'expr>(args: &'expr [Expr], field: &str) -> Option<&'expr Expr> {
    let [Expr::Record(entries)] = args else {
        return None;
    };
    entries
        .iter()
        .find_map(|(name, value)| (name.as_str() == field).then_some(value))
}

fn validate_trigger_operation_subscription_key(
    operation: crate::TriggerHostOperation,
    args: &[Expr],
    span: Option<Span>,
) -> Result<(), LinkError> {
    if matches!(
        operation,
        crate::TriggerHostOperation::List | crate::TriggerHostOperation::Prune
    ) {
        return Ok(());
    }
    let [Expr::Record(entries)] = args else {
        return Ok(());
    };
    if let Some((_, key)) = entries
        .iter()
        .find(|(name, _)| name.as_str() == "subscription_key")
    {
        validate_trigger_subscription_key_literal(key, span)?;
    }
    Ok(())
}

fn validate_trigger_subscription_key_literal(
    key: &Expr,
    span: Option<Span>,
) -> Result<(), LinkError> {
    let Expr::String(key) = key else {
        return Err(LinkError::InvalidTriggerSubscriptionKey { span });
    };
    if key.as_str().is_empty() || key.as_str().starts_with("lash.internal/") {
        return Err(LinkError::InvalidTriggerSubscriptionKey { span });
    }
    Ok(())
}

#[derive(Clone)]
enum StaticTriggerBinding {
    Source {
        source_type: String,
        source_key: String,
    },
    Target(String),
    Json(serde_json::Value),
}

fn materialize_default_trigger_keys(mut program: Program) -> Result<Program, LinkError> {
    let mut seen = BTreeSet::new();
    let mut derived_keys = VecDeque::new();
    for declaration in &program.declarations {
        if let Declaration::Process(process) = declaration {
            let mut bindings = process
                .params
                .iter()
                .filter_map(|param| {
                    let TypeExpr::Ref(source_type) = &param.ty else {
                        return None;
                    };
                    Some((
                        param.name.to_string(),
                        StaticTriggerBinding::Source {
                            source_type: source_type.to_string(),
                            source_key: semantic_trigger_source_key(
                                source_type.as_str(),
                                &serde_json::json!({
                                    "process_param": param.name.as_str(),
                                }),
                            ),
                        },
                    ))
                })
                .collect();
            collect_default_trigger_keys(
                &process.body,
                &mut bindings,
                &mut seen,
                &mut derived_keys,
            )?;
        }
    }
    collect_default_trigger_keys(
        &program.main,
        &mut BTreeMap::new(),
        &mut seen,
        &mut derived_keys,
    )?;

    struct Materializer<'keys> {
        derived_keys: &'keys mut VecDeque<String>,
    }

    impl crate::ExprFolder for Materializer<'_> {
        fn fold_expr(&mut self, expr: Expr) -> Expr {
            let expr = match expr {
                Expr::ReceiverCall {
                    receiver,
                    operation,
                    mut args,
                } if operation.as_str()
                    == crate::TriggerHostOperation::Register.receiver_method()
                    && matches!(
                        receiver.as_ref(),
                        Expr::ResourceRef(resource)
                            if crate::is_trigger_resource_type(resource.resource_type.as_str())
                    )
                    && crate::register_call_args(&args)
                        .is_ok_and(|call| call.subscription_key.is_none()) =>
                {
                    let key = self
                        .derived_keys
                        .pop_front()
                        .expect("every keyless registration was collected");
                    if let [Expr::Record(entries)] = args.as_mut_slice() {
                        entries.push(("subscription_key".into(), Expr::String(key.into())));
                    }
                    Expr::ReceiverCall {
                        receiver,
                        operation,
                        args,
                    }
                }
                expr => expr,
            };
            crate::fold_expr_children(self, expr)
        }
    }

    let mut materializer = Materializer {
        derived_keys: &mut derived_keys,
    };
    for declaration in &mut program.declarations {
        if let Declaration::Process(process) = declaration {
            process.body =
                crate::ExprFolder::fold_expr(&mut materializer, std::mem::replace(
                    &mut process.body,
                    Expr::Null,
                ));
        }
    }
    program.main = crate::ExprFolder::fold_expr(
        &mut materializer,
        std::mem::replace(&mut program.main, Expr::Null),
    );
    debug_assert!(derived_keys.is_empty());
    Ok(program)
}

fn collect_default_trigger_keys(
    expr: &Expr,
    bindings: &mut BTreeMap<String, StaticTriggerBinding>,
    seen: &mut BTreeSet<(String, String, String)>,
    derived_keys: &mut VecDeque<String>,
) -> Result<(), LinkError> {
    match expr {
        Expr::Block(expressions) => {
            for expression in expressions {
                collect_default_trigger_keys(expression, bindings, seen, derived_keys)?;
            }
            return Ok(());
        }
        Expr::Assign { target, expr } => {
            collect_default_trigger_keys(expr, bindings, seen, derived_keys)?;
            if target.is_simple() {
                if let Some(binding) = static_trigger_binding(expr, bindings) {
                    bindings.insert(target.root.to_string(), binding);
                } else {
                    bindings.remove(target.root.as_str());
                }
            }
            return Ok(());
        }
        Expr::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_default_trigger_keys(condition, bindings, seen, derived_keys)?;
            collect_default_trigger_keys(
                then_block,
                &mut bindings.clone(),
                seen,
                derived_keys,
            )?;
            collect_default_trigger_keys(
                else_block,
                &mut bindings.clone(),
                seen,
                derived_keys,
            )?;
            return Ok(());
        }
        Expr::For { iterable, body, .. } => {
            collect_default_trigger_keys(iterable, bindings, seen, derived_keys)?;
            collect_default_trigger_keys(body, &mut bindings.clone(), seen, derived_keys)?;
            return Ok(());
        }
        Expr::While { condition, body } => {
            collect_default_trigger_keys(condition, bindings, seen, derived_keys)?;
            collect_default_trigger_keys(body, &mut bindings.clone(), seen, derived_keys)?;
            return Ok(());
        }
        Expr::ReceiverCall {
            receiver,
            operation,
            args,
        } if operation.as_str() == crate::TriggerHostOperation::Register.receiver_method()
            && matches!(
                receiver.as_ref(),
                Expr::ResourceRef(resource) if crate::is_trigger_resource_type(resource.resource_type.as_str())
            ) =>
        {
            if let Ok(call) = crate::register_call_args(args)
                && call.subscription_key.is_none()
            {
                let Some((source_type, source_key)) =
                    static_trigger_source(call.source, bindings)
                else {
                    return Err(LinkError::UnresolvedDerivedTriggerSubscriptionKey {
                        span: None,
                    });
                };
                let Some(process) = static_trigger_target(call.target, bindings) else {
                    return Err(LinkError::UnresolvedDerivedTriggerSubscriptionKey {
                        span: None,
                    });
                };
                if !seen.insert((process.clone(), source_type.clone(), source_key.clone())) {
                    return Err(LinkError::DuplicateDerivedTriggerSubscriptionKey {
                        process,
                        source_type,
                        span: None,
                    });
                }
                derived_keys.push_back(semantic_trigger_subscription_key(
                    &process,
                    &source_type,
                    &source_key,
                ));
            }
        }
        _ => {}
    }
    for child in expr.children() {
        collect_default_trigger_keys(child, bindings, seen, derived_keys)?;
    }
    Ok(())
}

fn static_trigger_binding(
    expr: &Expr,
    bindings: &BTreeMap<String, StaticTriggerBinding>,
) -> Option<StaticTriggerBinding> {
    if let Some((source_type, source_key)) = static_trigger_source(expr, bindings) {
        return Some(StaticTriggerBinding::Source {
            source_type,
            source_key,
        });
    }
    if let Some(process) = static_trigger_target(expr, bindings) {
        return Some(StaticTriggerBinding::Target(process));
    }
    static_trigger_json(expr, bindings).map(StaticTriggerBinding::Json)
}

fn static_trigger_source(
    expr: &Expr,
    bindings: &BTreeMap<String, StaticTriggerBinding>,
) -> Option<(String, String)> {
    match expr {
        Expr::Variable(name) => match bindings.get(name.as_str())? {
            StaticTriggerBinding::Source {
                source_type,
                source_key,
            } => Some((source_type.clone(), source_key.clone())),
            StaticTriggerBinding::Target(_) | StaticTriggerBinding::Json(_) => None,
        },
        Expr::HostDescriptorConstructor { type_name, input } => {
            let source = static_trigger_json(input, bindings).or_else(|| {
                serde_json::to_value(input)
                    .ok()
                    .map(|input| serde_json::json!({ "dynamic_expression": input }))
            })?;
            Some((
                type_name.to_string(),
                semantic_trigger_source_key(type_name.as_str(), &source),
            ))
        }
        _ => None,
    }
}

fn semantic_trigger_source_key(source_type: &str, source: &serde_json::Value) -> String {
    lash_core::facade_support::default_trigger_source_key(source_type, source)
}

fn semantic_trigger_subscription_key(
    process_name: &str,
    source_type: &str,
    source_key: &str,
) -> String {
    lash_core::facade_support::derived_trigger_subscription_key(
        process_name,
        source_type,
        source_key,
    )
}

fn static_trigger_target(
    expr: &Expr,
    bindings: &BTreeMap<String, StaticTriggerBinding>,
) -> Option<String> {
    match expr {
        Expr::Variable(name) => match bindings.get(name.as_str())? {
            StaticTriggerBinding::Target(process) => Some(process.clone()),
            StaticTriggerBinding::Source { .. } | StaticTriggerBinding::Json(_) => None,
        },
        Expr::ProcessRef { process } => Some(process.to_string()),
        _ => None,
    }
}

fn static_trigger_json(
    expr: &Expr,
    bindings: &BTreeMap<String, StaticTriggerBinding>,
) -> Option<serde_json::Value> {
    match expr {
        Expr::Null => Some(serde_json::Value::Null),
        Expr::Bool(value) => Some((*value).into()),
        Expr::Number(value) => serde_json::Number::from_f64(*value).map(Into::into),
        Expr::String(value) => Some(value.to_string().into()),
        Expr::Variable(name) => match bindings.get(name.as_str())? {
            StaticTriggerBinding::Json(value) => Some(value.clone()),
            StaticTriggerBinding::Source { .. } | StaticTriggerBinding::Target(_) => None,
        },
        Expr::Tuple(items) | Expr::List(items) => items
            .iter()
            .map(|item| static_trigger_json(item, bindings))
            .collect::<Option<Vec<_>>>()
            .map(Into::into),
        Expr::Record(entries) => entries
            .iter()
            .map(|(name, value)| {
                Some((name.to_string(), static_trigger_json(value, bindings)?))
            })
            .collect::<Option<serde_json::Map<_, _>>>()
            .map(Into::into),
        _ => None,
    }
}
