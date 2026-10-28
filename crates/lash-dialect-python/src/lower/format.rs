//! f-strings evaluate the value, nested specifications and conversion in Python order.

use lash_kernel_doc::{Expr, Literal};

use crate::diagnostics::{self, Code};
use ruff_python_ast::{self as ast};
use ruff_text_size::Ranged;

use super::{Buf, Lowerer, Lowering, Operand};
use crate::scope::Ty;

impl Lowerer<'_> {
    /// Source-proven strings and integers have the kernel's text spelling.
    pub(super) fn string_value(&mut self, value: Operand) -> Lowering<Operand> {
        match value.ty {
            Ty::Str => Ok(value),
            Ty::Int => Ok(Operand::inline(
                self.native("num.to_text", vec![value.expr])?,
                Ty::Str,
            )),
            _ => self.invoke("py.str", &[value], Ty::Str),
        }
    }

    pub(super) fn fstring(&mut self, fstring: &ast::ExprFString) -> Lowering<Operand> {
        let elements = || {
            fstring.value.iter().flat_map(|part| match part {
                ast::FStringPart::Literal(literal) => vec![Err(&*literal.value)],
                ast::FStringPart::FString(inner) => inner
                    .elements
                    .iter()
                    .map(|element| match element {
                        ast::InterpolatedStringElement::Literal(literal) => Err(&*literal.value),
                        ast::InterpolatedStringElement::Interpolation(field) => Ok(field),
                    })
                    .collect(),
            })
        };
        // Effectful field conversion emits statements. Capture earlier
        // fields before those statements, using ordinary operand ordering;
        // pure typed conversions need no intermediate statement.
        let mut parts: Vec<(Buf, Operand)> = Vec::new();
        for element in elements() {
            match (element, parts.last_mut()) {
                (
                    Err(literal),
                    Some((
                        _,
                        Operand {
                            expr: Expr::Literal(Literal::Text(text)),
                            ..
                        },
                    )),
                ) => {
                    text.push_str(literal);
                }
                (Err(literal), _) => parts.push((Buf::default(), Operand::text(literal))),
                (Ok(field), _) => {
                    parts.push(self.aside(|this| this.field(field))?);
                }
            }
        }
        let parts: Vec<_> = self
            .ordered(parts)
            .into_iter()
            .map(|part| part.expr)
            .collect();
        let joined = match parts.as_slice() {
            [] => return Ok(Operand::text("")),
            [Expr::Literal(Literal::Text(text))] => {
                return Ok(Operand::text(text));
            }
            [part] => part.clone(),
            [left, right] => self.native("text.concat", vec![left.clone(), right.clone()])?,
            _ => self.native("text.join", vec![Expr::List(parts), Operand::text("").expr])?,
        };
        Ok(Operand::inline(joined, Ty::Str))
    }

    /// One replacement field as text.
    fn field(&mut self, field: &ast::InterpolatedElement) -> Lowering<Operand> {
        if let Some(spec) = &field.format_spec {
            let mut literal = String::new();
            let written = spec.elements.iter().all(|element| match element {
                ast::InterpolatedStringElement::Literal(part) => {
                    literal.push_str(&part.value);
                    true
                }
                ast::InterpolatedStringElement::Interpolation(_) => false,
            });
            if written && literal.ends_with('n') {
                return Err(diagnostics::with_repair(
                    Code::FormatSpecUnsupported,
                    "format code n requires an ambient locale",
                    field.range(),
                    "use d, f or g with explicit grouping instead",
                ));
            }
        }
        let conversion = match field.conversion {
            ast::ConversionFlag::None
                if field.debug_text.is_some() && field.format_spec.is_none() =>
            {
                "r"
            }
            ast::ConversionFlag::None => "",
            ast::ConversionFlag::Str => "s",
            ast::ConversionFlag::Repr => "r",
            ast::ConversionFlag::Ascii => "a",
        };
        let value = self.expr(&field.expression)?;
        let shown = if let Some(spec) = &field.format_spec {
            let value = self.pin(value);
            let mut parts = Vec::new();
            for element in &*spec.elements {
                let part = match element {
                    ast::InterpolatedStringElement::Literal(literal) => {
                        Operand::text(&*literal.value)
                    }
                    ast::InterpolatedStringElement::Interpolation(field) => self.field(field)?,
                };
                parts.push(self.pin(part).expr);
            }
            let mut specification = Operand::text("").expr;
            for part in parts {
                specification = self.native("text.concat", vec![specification, part])?;
            }
            let spec = Operand::inline(specification, Ty::Str);
            self.invoke(
                "py.format_spec",
                &[value, spec, Operand::text(conversion)],
                Ty::Str,
            )?
        } else {
            match conversion {
                "r" => self.invoke("py.repr", &[value], Ty::Str)?,
                "a" => self.invoke("py.ascii", &[value], Ty::Str)?,
                _ => self.string_value(value)?,
            }
        };
        if let Some(debug) = &field.debug_text {
            let prefix = Operand::text(debug.as_str());
            let joined = self.native("text.concat", vec![prefix.expr, shown.expr])?;
            return Ok(Operand::inline(joined, Ty::Str));
        }
        Ok(shown)
    }
}
