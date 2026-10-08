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

    pub(super) fn infer_process_output(
        &self,
        process: &ProcessDecl,
        path: &AstPath,
        span: Option<Span>,
    ) -> Result<TypeExpr, LinkError> {
        let mut scope = Scope::new(true, span);
        scope.expected_return = process.return_ty.clone();
        for param in &process.params {
            scope.declare(param.name.as_str(), self.binding_for_type(&param.ty));
        }
        scope.declare("input", Binding::Value(process_input_type(process)));
        scope.declare("inputs", Binding::Value(process_input_record_type(process)));
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
