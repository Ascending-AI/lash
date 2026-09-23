use super::*;

impl<'module> Linker<'module> {
    pub(super) fn validate_process_arg_binding(
        &self,
        process: &str,
        arg: &str,
        expected_ty: &TypeExpr,
        actual: &Binding,
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        if let Some(expected_resource) = self.resource_type_for_type(expected_ty) {
            return match actual {
                Binding::Resource { resource_type } if *resource_type == expected_resource => {
                    Ok(())
                }
                Binding::Resource { resource_type } => {
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

    /// `call_path` is the [`AstPath`] of the `ReceiverCall` `args` belong to:
    /// `args[i]` is the call's `i + 1`-th child, and the fields the special
    /// forms pick out of `args[0]` are that record's children.
    pub(super) fn lower_trigger_operation_args(
        &self,
        operation: crate::TriggerHostOperation,
        args: &[Expr],
        call_path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Vec<Expr>, TypeExpr), LinkError> {
        match operation {
            crate::TriggerHostOperation::Register
            | crate::TriggerHostOperation::Update
            | crate::TriggerHostOperation::Revive => {
                self.lower_trigger_registration_args(operation, args, call_path, scope)
            }
            crate::TriggerHostOperation::List => {
                let call = crate::list_call_args(args)
                    .map_err(|_| LinkError::InvalidTriggerList { span: scope.span })?;
                let mut entries = Vec::with_capacity(call.entries.len());
                for (name, expr) in call.entries {
                    let expr_path = child_ast_path(&args[0], &call_path.child(1), expr);
                    let (expr, binding) = self.lower_expr(expr, &expr_path, scope)?;
                    let filter_ty = binding_type(&binding);
                    match name.as_str() {
                        "target" => {
                            self.trigger_target_signature(&filter_ty, scope.span)?;
                        }
                        "name" | "source_type" => {
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
                    entries.push((name.clone(), expr));
                }
                Ok((vec![Expr::Record(entries)], operation.output_ty()))
            }
            _ => unreachable!("only definition/list operations use specialized trigger lowering"),
        }
    }

    pub(super) fn lower_trigger_registration_args(
        &self,
        operation: crate::TriggerHostOperation,
        args: &[Expr],
        call_path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Vec<Expr>, TypeExpr), LinkError> {
        let call = crate::register_call_args(args)
            .map_err(|_| LinkError::InvalidTriggerRegistration { span: scope.span })?;
        // `call`'s fields all point into `args[0]`, the registration record.
        let record_path = call_path.child(1);
        let field_path = |field: &Expr| child_ast_path(&args[0], &record_path, field);
        let (source, source_binding) =
            self.lower_expr(call.source, &field_path(call.source), scope)?;
        let source_ty = binding_type(&source_binding);
        let event_ty = self
            .surface
            .resources
            .trigger_source_event(&source_ty)
            .ok_or_else(|| LinkError::UnknownTriggerEventType {
                source_ty: format_type_expr(&source_ty),
                span: scope.span,
            })?;
        let (target, target_binding) = self.lower_expr_expected(
            call.target,
            &field_path(call.target),
            scope,
            Some(&process_unknown_type()),
        )?;
        let target_ty = binding_type(&target_binding);
        let params = self.trigger_target_params(&target_ty, scope.span)?;
        let process = trigger_target_process_label(call.target);

        let inputs = match call.inputs {
            Some(inputs) => self.lower_trigger_input_record(
                process.as_str(),
                &params,
                &event_ty,
                inputs,
                &field_path(inputs),
                scope,
            )?,
            None => {
                self.default_trigger_input_record(process.as_str(), &params, &event_ty, scope.span)?
            }
        };
        let mut entries = vec![
            ("source".into(), source),
            ("target".into(), target),
            ("inputs".into(), inputs),
        ];
        if let Some(name) = call.name {
            entries.push((
                "name".into(),
                self.lower_expr(name, &field_path(name), scope)?.0,
            ));
        }
        if let Some(subscription_key) = call.subscription_key {
            entries.push((
                "subscription_key".into(),
                self.lower_expr(subscription_key, &field_path(subscription_key), scope)?
                    .0,
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
                self.lower_expr_expected(
                    expected_revision,
                    &field_path(expected_revision),
                    scope,
                    Some(&TypeExpr::Int),
                )?
                .0,
            ));
        }
        Ok((vec![Expr::Record(entries)], operation.output_ty()))
    }

    /// The record an omitted `inputs` stands for.
    ///
    /// Legal only when the target's authoritative signature (ADR 0090) has
    /// exactly one parameter and the source's event type is assignable to it:
    /// then there is one place the event can go and no fixed input to supply,
    /// so the mapping carries no information the signature does not already
    /// have. Every other arity is refused by name rather than defaulted,
    /// because guessing which of several parameters receives the event is the
    /// mistake the explicit form exists to prevent. A zero-parameter target is
    /// told to grow an event parameter, not to write an `inputs` record that
    /// could never be valid.
    pub(super) fn default_trigger_input_record(
        &self,
        process: &str,
        params: &[ProcessParam],
        event_ty: &TypeExpr,
        span: Option<Span>,
    ) -> Result<Expr, LinkError> {
        let [param] = params else {
            return Err(if params.is_empty() {
                LinkError::TriggerTargetTakesNoEvent {
                    process: process.to_string(),
                    span,
                }
            } else {
                LinkError::AmbiguousOmittedTriggerInputs {
                    process: process.to_string(),
                    params: params.len(),
                    span,
                }
            });
        };
        if !self.is_type_assignable(event_ty, &param.ty) {
            return Err(LinkError::TriggerEventMismatch {
                event: format_type_expr(&self.resolve_type_aliases(event_ty)),
                input_name: param.name.to_string(),
                input: format_type_expr(&self.resolve_type_aliases(&param.ty)),
                span,
            });
        }
        Ok(Expr::Record(vec![(
            param.name.clone(),
            crate::trigger_event_placeholder_expr(),
        )]))
    }

    pub(super) fn lower_trigger_input_record(
        &self,
        process: &str,
        params: &[ProcessParam],
        event_ty: &TypeExpr,
        inputs: &Expr,
        inputs_path: &AstPath,
        scope: &mut Scope,
    ) -> Result<Expr, LinkError> {
        let Expr::Record(entries) = inputs else {
            return Err(LinkError::InvalidTriggerInputs { span: scope.span });
        };
        let mut seen = BTreeSet::new();
        let mut saw_event = false;
        let mut lowered = Vec::with_capacity(entries.len());
        for (index, (name, value)) in entries.iter().enumerate() {
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
            if is_trigger_event_expr(value) || is_trigger_event_placeholder_expr(value) {
                saw_event = true;
                if !self.is_type_assignable(event_ty, &param.ty) {
                    return Err(LinkError::TriggerEventMismatch {
                        event: format_type_expr(&self.resolve_type_aliases(event_ty)),
                        input_name: name.to_string(),
                        input: format_type_expr(&self.resolve_type_aliases(&param.ty)),
                        span: scope.span,
                    });
                }
                // The dialect emits the marker record directly now; an older
                // `trigger.event` path is rewritten to the same bytes so both
                // spellings produce one canonical IR.
                lowered.push((name.clone(), crate::trigger_event_placeholder_expr()));
                continue;
            }
            let (lowered_value, binding) = self.lower_expr_expected(
                value,
                &inputs_path.child(index as u32),
                scope,
                Some(&param.ty),
            )?;
            self.validate_process_arg_binding(
                process,
                name.as_str(),
                &param.ty,
                &binding,
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

    pub(super) fn trigger_target_params(
        &self,
        target_ty: &TypeExpr,
        span: Option<Span>,
    ) -> Result<Vec<ProcessParam>, LinkError> {
        self.trigger_target_signature(target_ty, span)?
            .map(|signature| signature.params().to_vec())
            .ok_or_else(|| LinkError::InvalidTriggerTarget {
                actual: format_type_expr(target_ty),
                span,
            })
    }

    fn trigger_target_signature(
        &self,
        target_ty: &TypeExpr,
        span: Option<Span>,
    ) -> Result<Option<crate::ProcessSignature>, LinkError> {
        let resolved = self.resolve_type_aliases(target_ty);
        let signature = match &resolved {
            TypeExpr::Process(process) => process.as_signature().cloned(),
            TypeExpr::Union(items) => {
                let mut common: Option<crate::ProcessSignature> = None;
                let mut unknown = false;
                for item in items {
                    let TypeExpr::Process(process) = item else {
                        return Err(LinkError::InvalidTriggerTarget {
                            actual: format_type_expr(&resolved),
                            span,
                        });
                    };
                    let Some(signature) = process.as_signature() else {
                        unknown = true;
                        continue;
                    };
                    match &common {
                        Some(existing) if existing != signature => {
                            return Err(LinkError::InvalidTriggerTarget {
                                actual: format_type_expr(&resolved),
                                span,
                            });
                        }
                        Some(_) => {}
                        None => common = Some(signature.clone()),
                    }
                }
                (!unknown).then_some(common).flatten()
            }
            _ => {
                return Err(LinkError::InvalidTriggerTarget {
                    actual: format_type_expr(&resolved),
                    span,
                });
            }
        };
        Ok(signature)
    }

    pub(super) fn infer_process_output(
        &self,
        process: &ProcessDecl,
        path: &AstPath,
        span: Option<Span>,
    ) -> Result<TypeExpr, LinkError> {
        let mut scope = Scope::new(true, span);
        scope.expected_return = process.return_ty.clone();
        for param in &process.params {
            scope.bind(param.name.as_str(), self.binding_for_type(&param.ty));
        }
        scope.bind("input", Binding::Value(process_input_type(process)));
        scope.bind("inputs", Binding::Value(process_input_record_type(process)));
        self.completion_facts.borrow_mut().clear();
        self.collect_completion.set(true);
        // Inference lowers the body only to learn its output. The body is
        // lowered again for the program, and that lowering lifts its literals;
        // the literals this pass lifts are dropped, or each would be declared
        // twice.
        let lifted = self.lifted_declarations.borrow().len();
        let result = self.lower_expr(&process.body, path, &mut scope);
        self.lifted_declarations.borrow_mut().truncate(lifted);
        self.collect_completion.set(false);
        result?;
        let completion = self
            .completion_facts
            .borrow()
            .get(path)
            .cloned()
            .unwrap_or_else(Completion::fallthrough);
        let mut outputs = completion.finishes;
        if completion.can_fallthrough {
            outputs.push(TypeExpr::Null);
        }
        Ok(union_type(outputs))
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

pub(super) fn validate_trigger_operation_subscription_key(
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
