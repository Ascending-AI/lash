//! Primitive method regions. Eligibility is decided before arguments are
//! lowered; coercible bounds, spreads and unknown receivers keep their helper.

use lash_kernel_doc::{Expr, Integer, Literal, Stmt};

use super::{Buf, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;

const TRIM_SET: &str = "\u{0009}\u{000a}\u{000b}\u{000c}\u{000d} \u{00a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

fn int(value: i64) -> Expr {
    Expr::Literal(Literal::Int(Integer::from(value)))
}

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
        let slice = name == "slice";
        let trim = text && matches!(name, "trim" | "trimStart" | "trimEnd");
        let search = text && matches!(name, "indexOf" | "includes" | "startsWith" | "endsWith");
        let split = text && name == "split";
        if !slice && !trim && !search && !split {
            return Ok(None);
        }
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
            let check = self.native("text.len", vec![object.expr()])?;
            let check = self.let_expr(check, Ty::Unknown);
            self.discard(check);
        }
        // Extra arguments still run, left to right, even when ignored. Pin all
        // values that a later argument could invalidate; no argument list exists.
        let passed = self.operands(&values)?;
        if trim {
            let result = self.native(
                "text.trim_set",
                vec![
                    object.expr(),
                    Operand::text(TRIM_SET).expr(),
                    Operand::bool(name != "trimEnd").expr(),
                    Operand::bool(name != "trimStart").expr(),
                ],
            )?;
            return Ok(Some(self.let_expr(result, Ty::Text)));
        }
        if split {
            let result = self.native(
                "text.split",
                vec![object.expr(), passed[0].expr(), int(4_294_967_295)],
            )?;
            return Ok(Some(self.let_expr(result, Ty::Array)));
        }
        if search {
            let position = if name == "endsWith" && upper.is_none() {
                self.native("text.utf16_len", vec![object.expr()])?
            } else {
                self.primitive_position(object, upper.unwrap_or(0.0), false, true)?
            };
            let needle = passed[0].expr();
            if name == "endsWith" {
                let width = self.native("text.utf16_len", vec![needle.clone()])?;
                let start = self.native("num.sub", vec![position, width])?;
                let start = self.let_expr(start, Ty::Unknown);
                let negative = self.native("num.lt", vec![start.expr(), int(0)])?;
                let result = self.let_expr(Operand::bool(false).expr(), Ty::Bool);
                let place =
                    lash_kernel_doc::Place::Variable(super::statements::variable_of(&result));
                let nonnegative = self.block(|this| {
                    let found =
                        this.native("text.utf16_find", vec![object.expr(), needle, start.expr()])?;
                    let matched = this.native("eq", vec![found, start.expr()])?;
                    let matched = this.let_expr(matched, Ty::Bool);
                    this.store(place, matched);
                    Ok(())
                })?;
                self.emit_if(negative, Buf::default(), nonnegative);
                return Ok(Some(result));
            }
            let found = self.native(
                "text.utf16_find",
                vec![object.expr(), needle, position.clone()],
            )?;
            let (result, ty) = match name {
                "indexOf" => (self.native("num.to_float", vec![found])?, Ty::Float),
                "includes" => (self.native("num.le", vec![int(0), found])?, Ty::Bool),
                _ => (self.native("eq", vec![found, position])?, Ty::Bool),
            };
            return Ok(Some(self.let_expr(result, ty)));
        }
        let start = self.primitive_position(object, lower.unwrap_or(0.0), true, text)?;
        let end = if let Some(upper) = upper {
            self.primitive_position(object, upper, true, text)?
        } else {
            self.native(
                if text { "text.utf16_len" } else { "list.len" },
                vec![object.expr()],
            )?
        };
        if text {
            // The small boundary wrapper preserves the dialect's typed refusal
            // for a split surrogate pair; receiver and bound admission are gone.
            let start = self.let_expr(start, Ty::Unknown);
            let end = self.let_expr(end, Ty::Unknown);
            return self
                .invoke(
                    "ts.string.slice_units",
                    &[object.clone(), start, end],
                    Ty::Text,
                )
                .map(Some);
        }
        let count = self.native("num.sub", vec![end.clone(), start.clone()])?;
        let too_large = self.native("num.lt", vec![int(4_294_967_295), count])?;
        let refusal = self.block(|this| {
            let error = this.native(
                "error.new",
                vec![
                    Operand::text("RangeError").expr(),
                    Operand::text("Invalid array length").expr(),
                    Operand::null().expr(),
                ],
            )?;
            this.emit(Stmt::Throw { value: error });
            Ok(())
        })?;
        self.emit_if(too_large, refusal, Buf::default());
        let result = self.native("list.slice", vec![object.expr(), start, end])?;
        Ok(Some(self.let_expr(result, Ty::Array)))
    }

    /// ToIntegerOrInfinity, then either a relative bound or an absolute search
    /// position. Constant zero avoids even the length read. Clamp infinities
    /// before converting to Int, and use UTF-16 length for every text bound.
    fn primitive_position(
        &mut self,
        object: &Operand,
        value: f64,
        relative: bool,
        text: bool,
    ) -> Lowering<Expr> {
        let value = if value.is_nan() { 0.0 } else { value.trunc() };
        if value == 0.0 || (!relative && value < 0.0) {
            return Ok(int(0));
        }
        let length = self.native(
            if text { "text.utf16_len" } else { "list.len" },
            vec![object.expr()],
        )?;
        if value == f64::INFINITY {
            return Ok(length);
        }
        if value == f64::NEG_INFINITY {
            return Ok(int(0));
        }
        let mut bound = Operand::number(value).expr();
        if relative && value < 0.0 {
            bound = self.native("num.add", vec![length.clone(), bound])?;
        }
        let bound = self.native("num.max", vec![bound, int(0)])?;
        let bound = self.native("num.min", vec![bound, length])?;
        let bound = self.native("num.to_float", vec![bound])?;
        self.native("float.to_int", vec![bound])
    }
}
