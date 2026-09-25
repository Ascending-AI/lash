//! Assignment targets and member reads: how a scalar `name = value`, a
//! member path `a.b[i]` on either side of an assignment, and the member
//! access special cases (a session slot through `globalThis`, a built-in
//! global's own surface, an async function's erased `constructor`) lower.

use lashlang::{AssignPathStep, AssignTarget, Expr as LashExpr, is_javascript_builtin_global};

use crate::adapter::{AssignTarget as TsAssignTarget, Expr, MemberProperty};
use crate::{Diagnostic, DiagnosticCode};

use super::stdlib::builtin_constant;
use super::triggers::{names_the_retired_trigger_event, retired_trigger_event_diagnostic};
use super::{BindingKind, Lowerer};

impl Lowerer {
    pub(super) fn lower_assign_target(
        &mut self,
        target: &TsAssignTarget,
    ) -> Result<AssignTarget, Diagnostic> {
        match target {
            TsAssignTarget::Ident(name) | TsAssignTarget::ParenIdent(name) => {
                // `eval` and `arguments` are never simple assignment
                // targets in strict code: ECMA-262 makes it an early
                // SyntaxError, which the parser misses only where the name
                // sits inside a destructuring pattern.
                if matches!(name.as_str(), "eval" | "arguments") {
                    return Err(Diagnostic::new(
                        DiagnosticCode::SyntaxError,
                        format!("`{name}` cannot be an assignment target in strict mode"),
                        None,
                    ));
                }
                let Some(binding) = self
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.bindings.get(name))
                    .cloned()
                else {
                    return Err(self.unknown_binding(name, None));
                };
                // `let`, `var`, parameters and `catch` bindings are mutable:
                // reassigning them is ordinary ECMA-262. `const` (tsc TS2588)
                // and function-declaration bindings stay refused.
                if matches!(binding.kind, BindingKind::Const | BindingKind::Function) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AssignConst,
                        format!("cannot assign to `{name}`"),
                        None,
                    ));
                }
                if binding.owner_function != self.current_function() {
                    // A closure assigning a binding its enclosing frame owns
                    // shares it: the write runs whenever the closure is
                    // called, which is what boxes the binding in a cell.
                    if !binding.initialized && !self.allow_uninitialized_declaration_capture {
                        return Err(Diagnostic::new(
                            DiagnosticCode::TemporalDeadZone,
                            format!(
                                "captured binding `{name}` is not initialized when the closure is created"
                            ),
                            None,
                        ));
                    }
                    self.capture(&binding);
                    self.capture_ledger.write_anytime(binding.id);
                }
                self.record_write(binding.id);
                Ok(AssignTarget::variable(binding.internal.into()))
            }
            TsAssignTarget::Member { object, property } => {
                self.member_assign_target(object, property)
            }
            TsAssignTarget::Pattern(_) => Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "destructuring targets are lowered as a pattern, not a scalar assignment",
                None,
            )),
        }
    }

    pub(super) fn member_assign_target(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<AssignTarget, Diagnostic> {
        let (root, mut steps) = self.member_path(object)?;
        steps.push(match property {
            MemberProperty::Field(field) => AssignPathStep::Field(field.as_str().into()),
            MemberProperty::Index(index) => AssignPathStep::Index(self.lower_expr(index)?),
        });
        Ok(AssignTarget {
            root: root.into(),
            steps,
        })
    }

    pub(super) fn member_path(
        &mut self,
        expr: &Expr,
    ) -> Result<(String, Vec<AssignPathStep>), Diagnostic> {
        match expr {
            Expr::Ident(name, _) => Ok((self.resolve(name)?, Vec::new())),
            Expr::Member {
                object, property, ..
            } => {
                let (root, mut steps) = self.member_path(object)?;
                steps.push(match property {
                    MemberProperty::Field(field) => AssignPathStep::Field(field.as_str().into()),
                    MemberProperty::Index(index) => AssignPathStep::Index(self.lower_expr(index)?),
                });
                Ok((root, steps))
            }
            _ => Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "assignment target must start at a lexical binding",
                None,
            )),
        }
    }

    pub(super) fn lower_member(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<LashExpr, Diagnostic> {
        if matches!(property, MemberProperty::Field(field) if field == "stack") {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: Error.stack is nondeterministic across engines. Inspect error.name and error.message instead.",
                None,
            ));
        }
        if matches!(object, Expr::Ident(name, _) if name == "globalThis" && !self.has_binding(name))
        {
            return match property {
                MemberProperty::Field(field)
                    if !matches!(field.as_str(), "undefined" | "NaN" | "Infinity") =>
                {
                    // The session slot, read live wherever the read runs: a
                    // function or closure reads the root frame's current
                    // value, as a global object property read does, never a
                    // copy and never a local of the same name.
                    self.refuse_global_this_in_process(field)?;
                    self.refuse_expired_global_read(field)?;
                    Ok(LashExpr::BuiltinCall {
                        name: "__typescript_global_get".into(),
                        args: vec![LashExpr::String(field.as_str().into())],
                    })
                }
                MemberProperty::Field(field) => Err(Diagnostic::new(
                    DiagnosticCode::ReservedIdentifier,
                    format!("globalThis.{field} is a reserved value identifier"),
                    None,
                )),
                MemberProperty::Index(_) => Err(Diagnostic::refusal(
                    DiagnosticCode::UnsupportedExpression,
                    "Unsupported: computed globalThis access. Use globalThis.identifier so session state remains statically named.",
                    None,
                )),
            };
        }
        if let Expr::Ident(owner, _) = object
            && is_javascript_builtin_global(owner)
            && !self.has_binding(owner)
        {
            // A boundary dropped the name for holding a function: a member
            // read is refused by name rather than answered from a fresh
            // built-in.
            self.refuse_expired_global_read(owner)?;
            let name = match property {
                MemberProperty::Field(field) => field.as_str(),
                MemberProperty::Index(_) => "",
            };
            if let Some(value) = builtin_constant(owner, name) {
                return Ok(LashExpr::Number(value));
            }
            // The rest of the built-in's surface is a read on the built-in
            // object itself — `Number.prototype`, `Math.constructor`, a miss
            // — and the heap answers what Node answers: the property value or
            // `undefined`.
            let target = Box::new(Self::stdlib_call(
                "Lash.Builtin",
                vec![LashExpr::String(owner.as_str().into())],
            ));
            return Ok(match property {
                MemberProperty::Field(field) => LashExpr::Field {
                    target,
                    field: field.as_str().into(),
                },
                MemberProperty::Index(index) => LashExpr::Index {
                    target,
                    index: Box::new(self.lower_expr(index)?),
                },
            });
        }
        // `(async function(){}).constructor` is %AsyncFunction%: the runtime
        // erases async-ness from a closure, so the lowerer answers from the
        // AST before the value ever materializes.
        if let Expr::Function(function) = object
            && function.is_async
            && matches!(property, MemberProperty::Field(field) if field == "constructor")
        {
            return Ok(Self::stdlib_call(
                "Lash.Builtin",
                vec![LashExpr::String("AsyncFunction".into())],
            ));
        }
        // The retired global, named and refused rather than left to reject as
        // an unknown binding, which said nothing about where the event went.
        if names_the_retired_trigger_event(object, property) && !self.has_binding("trigger") {
            return Err(retired_trigger_event_diagnostic());
        }
        let target = Box::new(self.lower_expr(object)?);
        Ok(match property {
            MemberProperty::Field(field) => LashExpr::Field {
                target,
                field: field.as_str().into(),
            },
            MemberProperty::Index(index) => LashExpr::Index {
                target,
                index: Box::new(self.lower_expr(index)?),
            },
        })
    }
}
