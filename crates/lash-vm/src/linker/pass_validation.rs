use super::*;

impl<'module> Linker<'module> {
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
        // twice. A submitted lifted declaration it derived is forgotten with
        // them, so the lowering that keeps the body derives it.
        let lifted = self.lifted_declarations.borrow().len();
        let rederived = self.rederived.borrow().clone();
        let lifted_body_sites = self.lifted_body_sites.borrow().clone();
        let result = self.lower_expr(&process.body, path, &mut scope);
        self.lifted_declarations.borrow_mut().truncate(lifted);
        self.rederived.replace(rederived);
        self.lifted_body_sites.replace(lifted_body_sites);
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
