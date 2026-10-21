//! f-strings.
//!
//! A replacement field's format specification is read here, when the
//! f-string is lowered, and its parts are passed to `py.format`; a
//! specification the dialect does not have is refused, not met at run
//! time.

use lash_kernel_doc::{Expr, Literal};

use ruff_python_ast::{self as ast};
use ruff_text_size::Ranged;

use super::{Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

/// A format specification:
/// `[[fill]align][sign][#][0][width][grouping][.precision][code]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    pub(crate) fill: char,
    /// `<`, `>`, `=`, `^`, or none.
    pub(crate) align: Option<char>,
    /// `+`, `-` or a space.
    pub(crate) sign: Option<char>,
    pub(crate) alternate: bool,
    pub(crate) zero: bool,
    pub(crate) width: u32,
    /// `,`, `_`, or none.
    pub(crate) grouping: Option<char>,
    pub(crate) precision: Option<u32>,
    pub(crate) code: Option<char>,
}

/// The format codes the dialect has.
const CODES: &str = "bdeEfFosxX%";

fn digits(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<Option<u32>, String> {
    let mut text = String::new();
    while let Some(digit) = chars.peek().copied().filter(char::is_ascii_digit) {
        text.push(digit);
        chars.next();
    }
    if text.is_empty() {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| "the width or precision is too large".to_string())
}

/// Reads a format specification, or says what in it the dialect lacks.
pub(crate) fn parse(spec: &str) -> Result<Spec, String> {
    let is_align = |c: char| matches!(c, '<' | '>' | '=' | '^');
    let mut parsed = Spec {
        fill: ' ',
        align: None,
        sign: None,
        alternate: false,
        zero: false,
        width: 0,
        grouping: None,
        precision: None,
        code: None,
    };
    let mut lookahead = spec.chars();
    let (first, second) = (lookahead.next(), lookahead.next());
    let mut chars = spec.chars().peekable();
    match (first, second) {
        (Some(fill), Some(align)) if is_align(align) => {
            parsed.fill = fill;
            parsed.align = Some(align);
            chars.next();
            chars.next();
        }
        (Some(align), _) if is_align(align) => {
            parsed.align = Some(align);
            chars.next();
        }
        _ => {}
    }
    if let Some(sign) = chars
        .peek()
        .copied()
        .filter(|c| matches!(c, '+' | '-' | ' '))
    {
        parsed.sign = Some(sign);
        chars.next();
    }
    if chars.peek() == Some(&'z') {
        return Err("the `z` option is not in the dialect".to_string());
    }
    if chars.peek() == Some(&'#') {
        parsed.alternate = true;
        chars.next();
    }
    if chars.peek() == Some(&'0') {
        parsed.zero = true;
        if !second.is_some_and(is_align) {
            parsed.fill = '0';
        }
        chars.next();
    }
    parsed.width = digits(&mut chars)?.unwrap_or(0);
    if let Some(grouping) = chars.peek().copied().filter(|c| matches!(c, ',' | '_')) {
        parsed.grouping = Some(grouping);
        chars.next();
    }
    if chars.peek() == Some(&'.') {
        chars.next();
        match digits(&mut chars)? {
            Some(precision) => parsed.precision = Some(precision),
            None => return Err("a `.` is followed by the precision".to_string()),
        }
    }
    if let Some(code) = chars.next() {
        if !CODES.contains(code) {
            return Err(format!(
                "the format code `{code}` is not in the dialect, which has {CODES}"
            ));
        }
        parsed.code = Some(code);
    }
    if chars.next().is_some() {
        return Err("the specification has text after its format code".to_string());
    }
    if parsed.zero && parsed.grouping.is_some() {
        return Err("zero padding with a grouping separator is not in the dialect".to_string());
    }
    Ok(parsed)
}

impl Lowerer<'_> {
    fn format_refusal(
        &self,
        problem: impl Into<String>,
        field: &ast::InterpolatedElement,
    ) -> lash_kernel_dialect::Diagnostic {
        let value = self.text(field.expression.range());
        let forms =
            ["d", "f", "e", "%", "x", "o", "b"].map(|spec| format!("`f\"{{{value}:{spec}}}\"`"));
        diagnostics::with_repair(
            Code::FormatSpecUnsupported,
            problem,
            field.range,
            format!(
                "format `{value}` with a supported literal specification: {}; width and precision must be literal",
                forms.join(", ")
            ),
        )
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
        if field.debug_text.is_some() {
            return Err(self.format_refusal(
                "the `=` form of a replacement field is not in the dialect",
                field,
            ));
        }
        let conversion = match field.conversion {
            ast::ConversionFlag::None => "",
            ast::ConversionFlag::Str => "s",
            ast::ConversionFlag::Repr => "r",
            ast::ConversionFlag::Ascii => {
                return Err(self.format_refusal("the `!a` conversion is not in the dialect", field));
            }
        };
        let value = self.expr(&field.expression)?;
        let Some(spec) = &field.format_spec else {
            let shown = match conversion {
                "r" => self.invoke("py.repr", &[value], Ty::Str)?,
                _ => self.invoke("py.str", &[value], Ty::Str)?,
            };
            return Ok(shown);
        };
        let mut written = String::new();
        for element in &*spec.elements {
            match element {
                ast::InterpolatedStringElement::Literal(literal) => {
                    written.push_str(&literal.value)
                }
                ast::InterpolatedStringElement::Interpolation(_) => {
                    return Err(self.format_refusal(
                        "a format specification is written out here, not computed",
                        field,
                    ));
                }
            }
        }
        if written.is_empty() {
            let helper = if conversion == "r" {
                "py.repr"
            } else {
                "py.str"
            };
            return self.invoke(helper, &[value], Ty::Str);
        }
        let spec = parse(&written).map_err(|problem| self.format_refusal(problem, field))?;
        let text = |value: Option<char>| Operand::text(value.map(String::from).unwrap_or_default());
        let precision = match spec.precision {
            Some(precision) => Operand::literal(Literal::Int(i64::from(precision).into()), Ty::Int),
            None => Operand::none(),
        };
        self.invoke(
            "py.format",
            &[
                value,
                Operand::text(conversion),
                Operand::text(spec.fill),
                text(spec.align),
                text(spec.sign),
                Operand::literal(Literal::Bool(spec.alternate), Ty::Bool),
                Operand::literal(Literal::Bool(spec.zero), Ty::Bool),
                Operand::literal(Literal::Int(i64::from(spec.width).into()), Ty::Int),
                text(spec.grouping),
                precision,
                text(spec.code),
            ],
            Ty::Str,
        )
    }
}
