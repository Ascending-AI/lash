//! Primitive method regions. Eligibility is decided before arguments are
//! lowered; coercible bounds, spreads and unknown receivers keep their helper.

use lash_kernel_doc::{Expr, Stmt};

use super::{Buf, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::constructs::methods::{self, Kind};

impl Lowerer<'_> {
    /// Constants are proof, not an annotation's promise about coercible input.
    /// `None` means absent; the outer option rejects any unknown expression.
    fn primitive_bound(&self, value: Option<&&ast::Expr>) -> Option<Option<f64>> {
        let Some(value) = value else {
            return Some(None);
        };
        match value {
            ast::Expr::Number(value) => Some(Some(*value)),
            ast::Expr::Ident(name, _) if !self.is_bound(name) => match name.as_str() {
                "undefined" => Some(None),
                "NaN" => Some(Some(f64::NAN)),
                "Infinity" => Some(Some(f64::INFINITY)),
                _ => None,
            },
            ast::Expr::Unary {
                op: ast::UnaryOp::Minus,
                value,
            } => self
                .primitive_bound(Some(&value.as_ref()))
                .map(|n| Some(-n.unwrap_or(f64::NAN))),
            ast::Expr::Unary {
                op: ast::UnaryOp::Plus,
                value,
            } => self
                .primitive_bound(Some(&value.as_ref()))
                .map(|n| Some(n.unwrap_or(f64::NAN))),
            _ => None,
        }
    }

    pub(super) fn call_primitive_method(
        &mut self,
        object: &Operand,
        name: &str,
        args: &[ast::CallArg],
        proven: bool,
    ) -> Lowering<Option<Operand>> {
        let text = object.ty == Ty::Text;
        let list = object.ty == Ty::Array;
        if !text && !list {
            return Ok(None);
        }
        let Some(values): Option<Vec<_>> = args
            .iter()
            .map(|arg| match arg {
                ast::CallArg::Value(value) => Some(value),
                ast::CallArg::Spread(_) => None,
            })
            .collect()
        else {
            return Ok(None);
        };
        let Some(declaration) = methods::method(name, text) else {
            return Ok(None);
        };
        let slice = declaration.kind() == Kind::Slice;
        let trim = declaration.kind() == Kind::Trim;
        let search = matches!(declaration.kind(), Kind::Search | Kind::EndsWith);
        let split = declaration.kind() == Kind::Split;
        let lower = if slice {
            self.primitive_bound(values.first())
        } else {
            Some(None)
        };
        let upper = if slice || search || split {
            self.primitive_bound(values.get(1))
        } else {
            Some(None)
        };
        let (Some(lower), Some(upper)) = (lower, upper) else {
            return Ok(None);
        };
        if search || split {
            let Some(ast::Expr::String(needle)) = values.first().copied() else {
                return Ok(None);
            };
            if split && (needle.is_empty() || upper.is_some()) {
                return Ok(None);
            }
        }
        // A trusted string annotation still checks the receiver at the method
        // read, before arguments can throw, change bindings or park.
        if text && !proven {
            let check = methods::TEXT_RECEIVER_CHECK
                .emit(&[object.expr()], &mut |name, args| self.native(name, args))?;
            let check = self.let_expr(check, Ty::Unknown);
            self.discard(check);
        }
        // Extra arguments still run, left to right, even when ignored. Pin all
        // values that a later argument could invalidate; no argument list exists.
        let passed = self.operands(&values)?;
        // Arguments omitted by the native expression still evaluate (and raise)
        // before it, in the same order as arguments retained in the expression.
        let passed: Vec<_> = passed.into_iter().map(|value| self.pin(value)).collect();
        if trim || split {
            let second = if trim {
                Operand::text(methods::trim_set()).expr()
            } else {
                passed[0].expr()
            };
            let result = declaration
                .expression()
                .unwrap_or_else(|| unreachable!("a declared primitive expression"))
                .emit(&[object.expr(), second], &mut |name, args| {
                    self.native(name, args)
                })?;
            return Ok(Some(
                self.inline(result, if trim { Ty::Text } else { Ty::Array }),
            ));
        }
        if search {
            let position = if declaration.kind() == Kind::EndsWith && upper.is_none() {
                methods::TEXT_LENGTH
                    .emit(&[object.expr()], &mut |name, args| self.native(name, args))?
            } else {
                self.primitive_position(object, upper.unwrap_or(0.0), false, true)?
            };
            let needle = passed[0].expr();
            if declaration.kind() == Kind::EndsWith {
                let start = methods::ENDS_WITH
                    .start
                    .emit(&[position, needle.clone()], &mut |name, args| {
                        self.native(name, args)
                    })?;
                let start = self.let_expr(start, Ty::Unknown);
                let negative = methods::ENDS_WITH
                    .negative
                    .emit(&[start.expr()], &mut |name, args| self.native(name, args))?;
                let result = self.let_expr(Operand::bool(false).expr(), Ty::Bool);
                let place =
                    lash_kernel_doc::Place::Variable(super::statements::variable_of(&result));
                let nonnegative = self.block(|this| {
                    let matched = methods::ENDS_WITH
                        .matched
                        .emit(&[object.expr(), needle, start.expr()], &mut |name, args| {
                            this.native(name, args)
                        })?;
                    let matched = this.let_expr(matched, Ty::Bool);
                    this.store(place, matched);
                    Ok(())
                })?;
                self.emit_if(negative, Buf::default(), nonnegative);
                return Ok(Some(result));
            }
            let result = declaration
                .expression()
                .unwrap_or_else(|| unreachable!("a declared search expression"))
                .emit(&[object.expr(), needle, position], &mut |name, args| {
                    self.native(name, args)
                })?;
            return Ok(Some(self.inline(
                result,
                if declaration.name == "indexOf" {
                    Ty::Float
                } else {
                    Ty::Bool
                },
            )));
        }
        let start = self.primitive_position(object, lower.unwrap_or(0.0), true, text)?;
        let end = if let Some(upper) = upper {
            self.primitive_position(object, upper, true, text)?
        } else {
            if text {
                methods::TEXT_LENGTH
            } else {
                methods::LIST_LENGTH
            }
            .emit(&[object.expr()], &mut |name, args| self.native(name, args))?
        };
        if text {
            // The small boundary wrapper preserves the dialect's typed refusal
            // for a split surrogate pair; receiver and bound admission are gone.
            let start = self.let_expr(start, Ty::Unknown);
            let end = self.let_expr(end, Ty::Unknown);
            return self
                .invoke(
                    methods::SLICE_UNITS,
                    &[object.clone(), start, end],
                    Ty::Text,
                )
                .map(Some);
        }
        let too_large = methods::SLICE_TOO_LARGE
            .emit(&[start.clone(), end.clone()], &mut |name, args| {
                self.native(name, args)
            })?;
        let refusal = self.block(|this| {
            let error =
                methods::SLICE_ERROR.emit(&[], &mut |name, args| this.native(name, args))?;
            this.emit(Stmt::Throw { value: error });
            Ok(())
        })?;
        self.emit_if(too_large, refusal, Buf::default());
        let result = methods::LIST_SLICE
            .emit(&[object.expr(), start, end], &mut |name, args| {
                self.native(name, args)
            })?;
        Ok(Some(self.inline(result, Ty::Array)))
    }

    fn primitive_position(
        &mut self,
        object: &Operand,
        value: f64,
        relative: bool,
        text: bool,
    ) -> Lowering<Expr> {
        methods::position(object.expr(), value, relative, text, &mut |name, args| {
            self.native(name, args)
        })
    }
}
