//! f-strings evaluate the value, nested specifications and conversion in Python order.

use lash_kernel_doc::{Expr, Literal};

use crate::diagnostics::{self, Code};
use ruff_python_ast::{self as ast};
use ruff_text_size::Ranged;

use super::{Lowerer, Lowering, Operand};
use crate::scope::Ty;

impl Lowerer<'_> {
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
        // Python formats every field before it builds the text: with more
        // than one field, each is bound where it is formatted, so no
        // concatenation runs before a later field and no later field's
        // statements change an earlier one.
        let pinned = elements().filter(Result::is_ok).count() > 1;
        let mut parts: Vec<Expr> = Vec::new();
        for element in elements() {
            match (element, parts.last_mut()) {
                (Err(literal), Some(Expr::Literal(Literal::Text(text)))) => {
                    text.push_str(literal);
                }
                (Err(literal), _) => parts.push(Expr::Literal(Literal::Text(literal.to_string()))),
                (Ok(field), _) => {
                    let shown = self.field(field)?;
                    let shown = if pinned { self.pin(shown) } else { shown };
                    parts.push(shown.expr);
                }
            }
        }
        // Every part is a text: they are concatenated in order, with no
        // list to build and no general join (FIG-6846).
        let mut parts = parts.into_iter();
        let joined = match parts.next() {
            None => return Ok(Operand::text("")),
            Some(Expr::Literal(Literal::Text(text))) if parts.len() == 0 => {
                return Ok(Operand::text(text));
            }
            Some(first) => first,
        };
        let joined = parts.try_fold(joined, |joined, part| {
            self.native("text.concat", vec![joined, part])
        })?;
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
            self.invoke(
                match conversion {
                    "r" => "py.repr",
                    "a" => "py.ascii",
                    _ => "py.str",
                },
                &[value],
                Ty::Str,
            )?
        };
        if let Some(debug) = &field.debug_text {
            let prefix = Operand::text(debug.as_str());
            let joined = self.native("text.concat", vec![prefix.expr, shown.expr])?;
            return Ok(Operand::inline(joined, Ty::Str));
        }
        Ok(shown)
    }
}
