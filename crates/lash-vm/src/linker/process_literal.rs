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
        path: &AstPath,
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
        self.lift_process_literal(literal, path, scope)
    }

    /// Hoists one process literal to a declaration and resolves the slot to
    /// its reference.
    pub(super) fn lift_process_literal(
        &self,
        literal: &crate::ast::ProcessLiteralExpr,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        self.lift_process(
            LiftedSource {
                params: &literal.params,
                hidden_args: &literal.hidden_args,
                return_ty: literal.return_ty.as_ref(),
                body: &literal.body,
                body_path: path.child(0),
            },
            path,
            scope,
        )
    }

    /// Derives a lifted declaration of the program handed in again, at the
    /// reference to it at `path`, and resolves that reference to what was
    /// derived (FIG-5640).
    ///
    /// The first reference lowering meets stands where the literal stood:
    /// the declaration is lifted there exactly as its literal would be, so
    /// its capture types, signature, output, site and name are all derived
    /// and none is read from the declaration. Every later reference resolves
    /// to that one declaration by identity. A reference is a process value
    /// wherever it sits, so no slot is asked for.
    pub(super) fn rederive_lifted_process(
        &self,
        index: u32,
        declared: &ProcessDecl,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        match self.rederived.borrow().get(declared.name.as_str()) {
            Some(Rederived::Derived { name, ty }) => {
                return Ok((
                    Expr::ProcessRef {
                        process: name.as_str().into(),
                    },
                    Binding::Value(ty.clone()),
                ));
            }
            // A lifted process is named by its content, and content that
            // holds its own name has none.
            Some(Rederived::Deriving) => {
                return Err(LinkError::InvalidAst {
                    source: crate::InvalidAst::InvalidProcessOrigin {
                        process: declared.name.to_string(),
                        reason: "a lifted process cannot reference itself",
                    },
                });
            }
            None => {}
        }
        let hidden_params = match &declared.origin {
            ProcessOrigin::Lifted { hidden_params, .. } => *hidden_params as usize,
            ProcessOrigin::Declared => 0,
        };
        let (params, hidden) = declared
            .params
            .split_at(declared.params.len().saturating_sub(hidden_params));
        // A capture's type is refined from the scope the process is lifted
        // in, so it goes back to unknown first.
        let hidden_args = hidden
            .iter()
            .map(|hidden| ProcessParam {
                ty: TypeExpr::Any,
                ..hidden.clone()
            })
            .collect::<Vec<_>>();
        let return_ty = match &declared.origin {
            ProcessOrigin::Lifted {
                declared_return_ty, ..
            } => declared_return_ty.as_ref(),
            ProcessOrigin::Declared => declared.return_ty.as_ref(),
        };
        self.rederived
            .borrow_mut()
            .insert(declared.name.to_string(), Rederived::Deriving);
        let body_site = self.lifted_site(path).child(0);
        self.lifted_body_sites.borrow_mut().insert(index, body_site);
        let lifted = self.lift_process(
            LiftedSource {
                params,
                hidden_args: &hidden_args,
                return_ty,
                body: &declared.body,
                body_path: AstPath::declaration(index, Vec::new()),
            },
            path,
            scope,
        );
        let mut rederived = self.rederived.borrow_mut();
        match &lifted {
            Ok((Expr::ProcessRef { process }, binding)) => {
                rederived.insert(
                    declared.name.to_string(),
                    Rederived::Derived {
                        name: process.to_string(),
                        ty: binding_type(binding),
                    },
                );
            }
            _ => {
                rederived.remove(declared.name.as_str());
            }
        }
        lifted
    }

    /// Where the expression at `path` of the program handed in sat before
    /// any process was lifted: `path` itself, unless it is inside a submitted
    /// lifted declaration, whose body sat under the site it is derived at.
    fn lifted_site(&self, path: &AstPath) -> AstPath {
        let AstRoot::Declaration(index) = path.root else {
            return path.clone();
        };
        match self.lifted_body_sites.borrow().get(&index) {
            Some(body) => AstPath {
                root: body.root,
                steps: body.steps.iter().chain(&path.steps).copied().collect(),
            },
            None => path.clone(),
        }
    }

    /// Hoists the process `source` spells to a declaration, lifted at `path`,
    /// and answers its reference.
    ///
    /// The body lowers once, in a process scope, with completion collection
    /// on: the `finish` types it reaches are the declaration's inferred
    /// output, exactly as a declared process's is. The declaration is named
    /// by a digest of what was derived ([`crate::lifted_process_name`]), so
    /// lifting the same content at the same site gives the same name, and
    /// therefore the same `ProcessRef`, the identity every durable row pins.
    fn lift_process(
        &self,
        source: LiftedSource<'_>,
        path: &AstPath,
        scope: &mut Scope,
    ) -> Result<(Expr, Binding), LinkError> {
        let span = self.expression_span(path).or(scope.span);
        let mut process_scope = Scope::new(true, span);
        let mut seen = BTreeSet::new();
        let mut start_params = source.params.to_vec();
        for param in source.params {
            if !seen.insert(param.name.to_string()) {
                return Err(LinkError::DuplicateProcessParam {
                    name: param.name.to_string(),
                    span,
                });
            }
            self.validate_type_refs(&param.ty, span)?;
        }
        // A capture's declared type is whatever the enclosing scope already
        // knows about the name. The front end cannot type it — it has no type
        // environment — so it writes `Any` and the lift refines it here. That
        // is what lets one process literal name another: the captured binding
        // is a `Process` out here, and stays one inside the body.
        let mut hidden_args = Vec::with_capacity(source.hidden_args.len());
        for hidden in source.hidden_args {
            // A read of another literal's binding is not a capture: that
            // literal lifted to a module-level declaration, and the body
            // resolves the name to its `Expr::ProcessRef` (`lower_variable`).
            // Making it a start argument instead would not compose — the
            // enclosing start would have to carry every definition every
            // nested start needs, transitively — so the name never becomes a
            // parameter here.
            if self
                .lifted_process_aliases
                .borrow()
                .contains_key(hidden.name.as_str())
            {
                continue;
            }
            if !seen.insert(hidden.name.to_string()) {
                return Err(LinkError::DuplicateProcessParam {
                    name: hidden.name.to_string(),
                    span,
                });
            }
            self.validate_type_refs(&hidden.ty, span)?;
            let mut hidden = hidden.clone();
            if matches!(hidden.ty, TypeExpr::Any)
                && let Some(binding) = scope.get(&hidden.name)
            {
                hidden.ty = binding_type(&binding);
            }
            start_params.push(hidden.clone());
            hidden_args.push(hidden);
        }
        for param in start_params.clone() {
            process_scope.declare(param.name.as_str(), self.binding_for_type(&param.ty));
        }
        let previous_completion = self.collect_completion.replace(true);
        let lowered = self.lower_expr(source.body, &source.body_path, &mut process_scope);
        self.collect_completion.set(previous_completion);
        let body = lowered?.0;
        let completion = self
            .completion_facts
            .borrow()
            .get(&source.body_path)
            .cloned()
            .unwrap_or_else(Completion::fallthrough);
        let mut outputs = completion.finishes;
        if completion.can_fallthrough {
            outputs.push(TypeExpr::Null);
        }
        let inferred = union_type(outputs);
        let mut declaration = ProcessDecl {
            name: "".into(),
            params: start_params.clone(),
            return_ty: None,
            label: None,
            origin: ProcessOrigin::Lifted {
                site: self.lifted_site(path),
                declared_return_ty: source.return_ty.cloned(),
                hidden_params: u32::try_from(hidden_args.len()).unwrap_or(u32::MAX),
            },
            body,
        };
        let name = crate::lifted_process_name(&declaration);
        let output = match source.return_ty {
            Some(expected) => {
                self.validate_type_refs(expected, span)?;
                if !self.is_type_assignable(&inferred, expected) {
                    return Err(LinkError::IncompatibleProcessReturn {
                        process: name,
                        expected: format_type_expr(&self.resolve_type_aliases(expected)),
                        actual: format_type_expr(&self.resolve_type_aliases(&inferred)),
                        span,
                    });
                }
                expected.clone()
            }
            None => inferred,
        };
        let signature =
            crate::ProcessSignature::try_new(start_params, output.clone()).map_err(|source| {
                LinkError::InvalidAst {
                    source: crate::InvalidAst::InvalidProcessSignature { source },
                }
            })?;
        let process_ty = TypeExpr::Process(crate::ProcessType::known(signature));
        declaration.name = name.as_str().into();
        declaration.return_ty = Some(output);
        self.lifted_declarations.borrow_mut().push((
            Declaration::Process(declaration),
            span,
            source.body_path,
        ));
        Ok((
            Expr::ProcessRef {
                process: name.into(),
            },
            Binding::Value(process_ty),
        ))
    }
}

/// What a process is lifted from: a literal, or a lifted declaration of the
/// program handed in.
struct LiftedSource<'a> {
    params: &'a [ProcessParam],
    hidden_args: &'a [ProcessParam],
    /// The authored settled output annotation.
    return_ty: Option<&'a TypeExpr>,
    body: &'a Expr,
    /// Where `body` is in the program handed in.
    body_path: AstPath,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_union_process_slot_admits_process_literals() {
        assert!(expected_type_contains_process(
            &super::super::type_helpers::union_type(vec![
                TypeExpr::Null,
                TypeExpr::Process(crate::ProcessType::unknown())
            ])
        ));
        assert!(!expected_type_contains_process(&TypeExpr::Any));
    }
}
