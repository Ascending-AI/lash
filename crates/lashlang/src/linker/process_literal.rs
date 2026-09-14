//! The process-literal lift: `Expr::ProcessLiteral` acceptance (FIG-2997).
//!
//! Discovery is syntactic in the dialects; acceptance is type-directed here.
//! A literal lifts to a hoisted [`ProcessDecl`] exactly when the slot's
//! expected type contains `Process`, and is a type error naming the slot
//! anywhere else (ADR 0095), which is the rule that also serves the trigger
//! registration target and deleted its receiver special case.

use super::*;

impl<'module> Linker<'module> {
    /// The expected-type hook for `Expr::ProcessLiteral`.
    ///
    /// Discovery is syntactic — a dialect lowers an inline process body in
    /// argument position to this node — and acceptance is type-directed: the
    /// literal lifts to a hoisted process declaration exactly when the slot's
    /// expected type contains `Process`, and is a type error naming the slot
    /// anywhere else. There is no marker at the call site and no receiver
    /// special case; the catalogue's `Process` type is the only fact that
    /// decides (ADR 0095).
    pub(super) fn lower_process_literal(
        &self,
        literal: &crate::ast::ProcessLiteralExpr,
        original: &Expr,
        scope: &mut Scope,
        expected: Option<&TypeExpr>,
    ) -> Result<(Expr, Binding), LinkError> {
        let slot_is_process = expected
            .map(|expected| expected_type_contains_process(&self.resolve_type_aliases(expected)))
            .unwrap_or(false);
        if !slot_is_process {
            return Err(LinkError::ProcessLiteralOutsideProcessSlot {
                expected: expected
                    .map(|expected| format_type_expr(&self.resolve_type_aliases(expected)))
                    .unwrap_or_else(|| "a value".to_string()),
                span: scope.span,
            });
        }
        self.lift_process_literal(literal, original, scope)
    }

    /// Hoists one process literal to a declaration and resolves the slot to
    /// its reference.
    ///
    /// The declaration's name derives from the canonical body plus the
    /// literal's AST path, so re-linking the same cell lifts the same body to
    /// the same name — and therefore to the same `ProcessRef`, the identity
    /// every durable row pins. The body lowers once, in a process scope, with
    /// completion collection on: the `finish` types it reaches are the
    /// declaration's inferred output, exactly as a declared process's is.
    pub(super) fn lift_process_literal(
        &self,
        literal: &crate::ast::ProcessLiteralExpr,
        original: &Expr,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        self.ensure_feature(self.surface.abilities.processes, "processes", scope.span)?;
        let span = self
            .expression_spans
            .get(&(original as *const Expr as usize))
            .copied()
            .or(scope.span);
        let name = self.lifted_process_name(literal, original);
        let mut process_scope = Scope::new(true, span);
        let mut seen = BTreeSet::new();
        let mut start_params = literal.params.clone();
        for param in &literal.params {
            if !seen.insert(param.name.to_string()) {
                return Err(LinkError::DuplicateProcessParam {
                    name: param.name.to_string(),
                    span,
                });
            }
            self.validate_type_refs(&param.ty, span)?;
        }
        for hidden in &literal.hidden_args {
            if !seen.insert(hidden.name.to_string()) {
                return Err(LinkError::DuplicateProcessParam {
                    name: hidden.name.to_string(),
                    span,
                });
            }
            self.validate_type_refs(&hidden.ty, span)?;
            start_params.push(hidden.clone());
        }
        for param in start_params.clone() {
            process_scope.bind(param.name.as_str(), self.binding_for_type(&param.ty));
        }
        let previous_completion = self.collect_completion.replace(true);
        let previous_signals = self.collect_signals.replace(true);
        let previous_inferred = std::mem::take(&mut *self.inferred_signals.borrow_mut());
        let lowered = self.lower_expr(&literal.body, &mut process_scope);
        self.collect_completion.set(previous_completion);
        self.collect_signals.set(previous_signals);
        let signals = self
            .inferred_signals
            .borrow()
            .iter()
            .map(|(name, ty)| ProcessSignalDecl {
                name: name.as_str().into(),
                ty: ty.clone(),
            })
            .collect::<Vec<_>>();
        let body = lowered?.0;
        let completion = self
            .completion_facts
            .borrow()
            .get(&(literal.body.as_ref() as *const Expr as usize))
            .cloned()
            .unwrap_or_else(Completion::fallthrough);
        let mut outputs = completion.finishes;
        if completion.can_fallthrough {
            outputs.push(TypeExpr::Null);
        }
        let output = union_type(outputs);
        let signature =
            crate::ProcessSignature::try_new(start_params, output.clone()).map_err(|source| {
                LinkError::InvalidAst {
                    source: crate::InvalidAst::InvalidProcessSignature { source },
                }
            })?;
        let process_ty = TypeExpr::Process(crate::ProcessType::known(signature));
        self.lifted_declarations.borrow_mut().push((
            Declaration::Process(ProcessDecl {
                name: name.clone().into(),
                params: {
                    let mut linked = literal.params.clone();
                    linked.extend(literal.hidden_args.clone());
                    linked
                },
                signals,
                return_ty: Some(output),
                label: None,
                body,
            }),
            span,
        ));
        Ok((
            Expr::ProcessRef {
                process: name.into(),
            },
            Binding::Value(process_ty),
        ))
    }

    /// The name of the declaration a literal lifts to: a digest over the
    /// canonical body plus the literal's AST path.
    ///
    /// The body is hashed in its canonical serialized form, so the name is a
    /// function of what the body *is* and where it sits — never of link order,
    /// span tables, or anything else a re-link could reorder.
    fn lifted_process_name(
        &self,
        literal: &crate::ast::ProcessLiteralExpr,
        original: &Expr,
    ) -> String {
        let path = self
            .expression_paths
            .get(&(original as *const Expr as usize))
            .cloned()
            .unwrap_or_default();
        crate::lifted_process_identity(&literal.body, &path)
    }
}

/// Whether a resolved expected type admits a process value in the slot.
///
/// A union admits a literal when any branch does; every other shape does not,
/// including `Any` — an untyped slot is not a process slot, and the error is
/// what tells the model where the body belongs.
fn expected_type_contains_process(expected: &TypeExpr) -> bool {
    match expected {
        TypeExpr::Process(_) => true,
        TypeExpr::Union(items) => items.iter().any(expected_type_contains_process),
        _ => false,
    }
}
