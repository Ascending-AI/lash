use super::*;

impl<'module> Linker<'module> {
    pub(super) fn resolve_module_expr(
        &self,
        expr: &Expr,
        scope: &Scope,
    ) -> Option<ResourceRefExpr> {
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

    pub(super) fn resolve_module_operation_expr(
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

    pub(super) fn reject_trigger_event_special_form(
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
