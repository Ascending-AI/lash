use super::*;

impl<'module> Linker<'module> {
    pub(super) fn collect_default_trigger_key(
        &self,
        receiver: &Expr,
        operation: &AstString,
        args: &[Expr],
        scope: &Scope,
    ) -> Result<(), LinkError> {
        if !self.collect_trigger_keys.get()
            || operation.as_str() != crate::TriggerHostOperation::Register.receiver_method()
            || !matches!(
                receiver,
                Expr::ResourceRef(resource)
                    if crate::is_trigger_resource_type(resource.resource_type.as_str())
            )
        {
            return Ok(());
        }
        let Ok(call) = crate::register_call_args(args) else {
            return Ok(());
        };
        if call.subscription_key.is_some() {
            return Ok(());
        }
        let Some((source_type, source_key)) =
            static_trigger_source(call.source, &scope.static_trigger_bindings)
        else {
            return Err(LinkError::UnresolvedDerivedTriggerSubscriptionKey { span: scope.span });
        };
        let Some(process) = static_trigger_target(call.target, &scope.static_trigger_bindings)
        else {
            return Err(LinkError::UnresolvedDerivedTriggerSubscriptionKey { span: scope.span });
        };
        let mut collector = self.trigger_key_collector.borrow_mut();
        if !collector
            .seen
            .insert((process.clone(), source_type.clone(), source_key.clone()))
        {
            return Err(LinkError::DuplicateDerivedTriggerSubscriptionKey {
                process,
                source_type,
                span: scope.span,
            });
        }
        collector
            .derived_keys
            .push_back(semantic_trigger_subscription_key(
                &process,
                &source_type,
                &source_key,
            ));
        Ok(())
    }

    pub(super) fn static_trigger_binding_for(
        &self,
        expr: &Expr,
        scope: &Scope,
    ) -> Option<StaticTriggerBinding> {
        static_trigger_binding(expr, &scope.static_trigger_bindings)
    }

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

    pub(super) fn lower_trigger_operation_args(
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
                    let (expr, binding) = self.lower_expr(expr, scope)?;
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
        scope: &mut Scope,
    ) -> Result<(Vec<Expr>, TypeExpr), LinkError> {
        let call = crate::register_call_args(args)
            .map_err(|_| LinkError::InvalidTriggerRegistration { span: scope.span })?;
        let (source, source_binding) = self.lower_expr(call.source, scope)?;
        let source_ty = binding_type(&source_binding);
        let event_ty = self
            .surface
            .resources
            .trigger_source_event(&source_ty)
            .ok_or_else(|| LinkError::UnknownTriggerEventType {
                source_ty: format_type_expr(&source_ty),
                span: scope.span,
            })?;
        let (target, target_binding) = self.lower_expr(call.target, scope)?;
        let target_ty = binding_type(&target_binding);
        let params = self.trigger_target_params(&target_ty, scope.span)?;
        let process = trigger_target_process_label(call.target);

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

    pub(super) fn lower_trigger_input_record(
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
        let result = self.lower_expr(&process.body, &mut scope);
        self.collect_completion.set(false);
        result?;
        let completion = self
            .completion_facts
            .borrow()
            .get(&(&process.body as *const Expr as usize))
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StaticTriggerBinding {
    Source {
        source_type: String,
        source_key: String,
    },
    Target(String),
    Json(serde_json::Value),
}

#[derive(Default)]
pub(super) struct TriggerKeyCollector {
    pub(super) seen: BTreeSet<(String, String, String)>,
    pub(super) derived_keys: VecDeque<String>,
}

pub(super) fn materialize_default_trigger_keys(
    mut program: Program,
    mut derived_keys: VecDeque<String>,
) -> Result<Program, LinkError> {
    struct Materializer<'keys> {
        derived_keys: &'keys mut VecDeque<String>,
    }

    impl crate::ExprFolder for Materializer<'_> {
        fn fold_expr(&mut self, expr: Expr) -> Expr {
            // Lowering observes a call after its children have been lowered, so
            // consume collected keys in that same post-order.
            let expr = crate::fold_expr_children(self, expr);
            match expr {
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
            }
        }
    }

    let mut materializer = Materializer {
        derived_keys: &mut derived_keys,
    };
    for declaration in &mut program.declarations {
        if let Declaration::Process(process) = declaration {
            process.body = crate::ExprFolder::fold_expr(
                &mut materializer,
                std::mem::replace(&mut process.body, Expr::Null),
            );
        }
    }
    program.main = crate::ExprFolder::fold_expr(
        &mut materializer,
        std::mem::replace(&mut program.main, Expr::Null),
    );
    debug_assert!(derived_keys.is_empty());
    Ok(program)
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

pub(super) fn semantic_trigger_source_key(source_type: &str, source: &serde_json::Value) -> String {
    lash_core::facade_support::default_trigger_source_key(source_type, source)
}

pub(super) fn semantic_trigger_subscription_key(
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
            .map(|(name, value)| Some((name.to_string(), static_trigger_json(value, bindings)?)))
            .collect::<Option<serde_json::Map<_, _>>>()
            .map(Into::into),
        _ => None,
    }
}
