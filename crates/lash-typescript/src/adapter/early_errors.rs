//! The early errors ECMA-262 attaches to single tokens: identifiers and
//! reserved words, escapes in literals, and the few token-level rules the
//! parser does not enforce itself.
//!
//! Every check here answers with `TS_SYNTAX_ERROR`, the dialect's early
//! `SyntaxError`. The adapter calls them where it converts the node they are
//! about, so each rule is written once.

use swc_common::Spanned;
use swc_ecma_ast as swc;

use super::prototype_chain::builtin_prototype_mutation;
use super::rejections::{reject, source_span};
use super::{Adapter, Expr, Goal};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// The words strict code reserves (ECMA-262 §12.7.2, *ReservedWord*, and the
/// strict-mode additions of §13.1.1). `await` is reserved by context and is
/// decided separately.
const STRICT_RESERVED_WORDS: [&str; 45] = [
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "let",
    "static",
    "implements",
    "interface",
    "package",
    "private",
    "protected",
    "public",
];

fn is_strict_reserved_word(name: &str) -> bool {
    STRICT_RESERVED_WORDS.contains(&name)
}

pub(super) fn syntax_error(message: impl Into<String>, span: Option<SourceSpan>) -> Diagnostic {
    Diagnostic::new(DiagnosticCode::SyntaxError, message, span)
}

impl Adapter<'_> {
    /// The name `ident` binds or references, once it is known to be one:
    /// neither a reserved word (§13.1.1) nor spelled with a malformed escape.
    ///
    /// A property name is an IdentifierName, never an identifier, and goes
    /// through [`Self::identifier_name`] instead.
    pub(super) fn identifier(&self, ident: &swc::Ident) -> Result<String, Diagnostic> {
        let name = if self.respelled_awaits.contains(&ident.span.lo.0) {
            "await"
        } else {
            ident.sym.as_ref()
        };
        let span = Some(source_span(ident.span));
        self.validate_respelling(ident.span);
        self.check_identifier_spelling(ident.span, name)?;
        if is_strict_reserved_word(name) {
            return Err(syntax_error(
                format!("`{name}` is a reserved word and cannot be used as an identifier"),
                span,
            ));
        }
        if name == "await" && (self.in_async_function.get() || self.goal == Goal::Module) {
            if !self.in_async_function.get() {
                self.module_goal_await.set(true);
            }
            return Err(syntax_error(
                "`await` is a reserved word here and cannot be used as an identifier",
                span,
            ));
        }
        Ok(name.to_string())
    }

    /// The name of a property, which may be any IdentifierName, reserved words
    /// included, but is still spelled by the identifier rules.
    pub(super) fn identifier_name(&self, name: &swc::IdentName) -> Result<String, Diagnostic> {
        self.validate_respelling(name.span);
        if self.respelled_awaits.contains(&name.span.lo.0) {
            return Ok("await".to_owned());
        }
        self.check_identifier_spelling(name.span, name.sym.as_ref())?;
        Ok(name.sym.to_string())
    }

    /// An identifier written with escapes must spell a valid identifier
    /// (§12.7.1): its first code point is an *IdentifierStartChar* even when
    /// escaped, and each `\u{…}` escape holds hex digits only.
    fn check_identifier_spelling(
        &self,
        span: swc_common::Span,
        name: &str,
    ) -> Result<(), Diagnostic> {
        let Some(raw) = self.source_text(span) else {
            return Ok(());
        };
        if !raw.contains('\\') {
            return Ok(());
        }
        let source_span = Some(source_span(span));
        if name
            .chars()
            .next()
            .is_some_and(|first| !swc::Ident::is_valid_start(first))
        {
            return Err(syntax_error(
                "an identifier cannot start with this character, escaped or not",
                source_span,
            ));
        }
        check_escapes(raw, EscapeContext::Identifier, source_span)
    }

    /// TypeScript's `this` parameter: a first parameter spelled `this`, which
    /// only annotates the receiver's type and is erased before run time.
    pub(super) fn is_this_parameter(&self, index: usize, pattern: &swc::Pat) -> bool {
        index == 0
            && matches!(pattern, swc::Pat::Ident(name)
                if self.source_text(name.id.span) == Some("this"))
    }

    pub(super) fn source_text(&self, span: swc_common::Span) -> Option<&str> {
        self.source.get(span.lo.0 as usize..span.hi.0 as usize)
    }

    /// Marks a respelled word as read by the adapter, which has now judged
    /// it by ECMA-262's rules instead of SWC's.
    fn validate_respelling(&self, span: swc_common::Span) {
        if self.respellings.contains_key(&span.lo.0) {
            self.validated_respellings.borrow_mut().insert(span.lo.0);
        }
    }

    /// The error SWC reported at the first respelled word the adapter never
    /// read: a word inside a construct it refused unread, or in a position it
    /// does not validate. SWC's verdict stands there, as it did before the
    /// word was respelled.
    pub(super) fn unvalidated_respelling(&self) -> Option<&swc_ecma_parser::error::Error> {
        let validated = self.validated_respellings.borrow();
        self.respellings
            .iter()
            .find(|(start, _)| !validated.contains(start))
            .map(|(_, error)| error)
    }

    /// Runs `convert` inside a function (or arrow parameters) with this
    /// `async`ness, which decides whether `await` is reserved there
    /// (§15.8.1): always under the Module goal, and under the Script goal only
    /// in async functions.
    pub(super) fn in_function<T>(
        &self,
        is_async: bool,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let enclosing = self.in_async_function.replace(is_async);
        let parameters = self.in_arrow_parameters.replace(false);
        let result = convert();
        self.in_arrow_parameters.set(parameters);
        self.in_async_function.set(enclosing);
        result
    }

    /// Runs `convert` over an arrow function's parameters, where an `await`
    /// expression is an early error (§15.3.1).
    pub(super) fn in_arrow_parameters<T>(
        &self,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let enclosing = self.in_arrow_parameters.replace(true);
        let result = convert();
        self.in_arrow_parameters.set(enclosing);
        result
    }

    pub(super) fn check_await_expression(&self, span: swc_common::Span) -> Result<(), Diagnostic> {
        if self.in_arrow_parameters.get() {
            return Err(syntax_error(
                "an arrow function's parameters cannot contain an `await` expression",
                Some(source_span(span)),
            ));
        }
        Ok(())
    }

    /// `delete` removes a property. Of a bare identifier it is an early error
    /// in strict code (§13.5.1.1). Of any other operand that is not a
    /// property reference it evaluates the operand and yields `true`, which
    /// `tsc --strict` refuses (TS2703), so the dialect refuses it by name
    /// rather than implement it (ADR 0064).
    pub(super) fn convert_delete(
        &self,
        operand: &swc::Expr,
        span: Option<SourceSpan>,
    ) -> Result<Expr, Diagnostic> {
        let mut unparenthesized = operand;
        while let swc::Expr::Paren(inner) = unparenthesized {
            unparenthesized = &inner.expr;
        }
        if matches!(unparenthesized, swc::Expr::Ident(_)) {
            return Err(syntax_error(
                "`delete` of an unqualified identifier is not allowed in strict mode",
                span,
            ));
        }
        if let Some(diagnostic) = unparenthesized
            .as_member()
            .and_then(builtin_prototype_mutation)
        {
            return Err(diagnostic);
        }
        match self.convert_expr(operand)? {
            Expr::Member {
                object, property, ..
            } => Ok(Expr::Delete { object, property }),
            Expr::OptionalChain { .. } => Err(reject(
                DiagnosticCode::SyntaxError,
                "Unsupported: delete on a non-member expression. Use delete object.member.",
                span,
            )),
            _ => Err(Diagnostic::new(
                DiagnosticCode::DeleteNonReferenceUnsupported,
                "`delete` of an operand that is not a property reference is refused, as `tsc --strict` refuses it (TS2703)",
                span,
            )),
        }
    }

    /// A string literal in strict code has no legacy octal or non-octal
    /// decimal escape (§12.9.4.1), and each `\u{…}` escape holds hex digits.
    pub(super) fn check_string_literal(&self, span: swc_common::Span) -> Result<(), Diagnostic> {
        match self.source_text(span) {
            Some(raw) => check_escapes(raw, EscapeContext::String, Some(source_span(span))),
            None => Ok(()),
        }
    }

    /// An untagged template's text has no *NotEscapeSequence* (§13.2.8.1):
    /// no `\1`–`\9`, no `\0` before a digit, no malformed `\u{…}`.
    pub(super) fn check_template_text(&self, span: swc_common::Span) -> Result<(), Diagnostic> {
        match self.source_text(span) {
            Some(raw) => check_escapes(raw, EscapeContext::Template, Some(source_span(span))),
            None => Ok(()),
        }
    }

    /// `of` is a contextual keyword, and a keyword may not be written with an
    /// escape (§12.7.2): the text between a `for`-`of` head's two halves holds
    /// the plain word.
    pub(super) fn check_for_of_keyword(
        &self,
        left: swc_common::Span,
        right: swc_common::Span,
    ) -> Result<(), Diagnostic> {
        let between = swc_common::Span::new(left.hi, right.lo);
        if self
            .source_text(between)
            .is_some_and(|text| code_outside_comments(text).contains('\\'))
        {
            return Err(syntax_error(
                "a keyword cannot contain escaped characters",
                Some(source_span(between)),
            ));
        }
        Ok(())
    }

    /// A classic `for` head holds exactly two `;`, and automatic semicolon
    /// insertion never supplies one there (§12.10.2): `for (a; b)` is an early
    /// error, whichever parts the head leaves out.
    pub(super) fn check_for_head(&self, stmt: &swc::ForStmt) -> Result<(), Diagnostic> {
        let head_span = swc_common::Span::new(stmt.span.lo, stmt.body.span().lo);
        let Some(head) = self.source_text(head_span) else {
            return Ok(());
        };
        let mut punctuation = head.to_string();
        let parts = [
            stmt.init.as_ref().map(Spanned::span),
            stmt.test.as_ref().map(|test| test.span()),
            stmt.update.as_ref().map(|update| update.span()),
        ];
        for part in parts.into_iter().flatten() {
            let start = (part.lo.0 - head_span.lo.0) as usize;
            let end = (part.hi.0 - head_span.lo.0) as usize;
            if let Some(text) = punctuation.get(start..end) {
                let blank = " ".repeat(text.len());
                punctuation.replace_range(start..end, &blank);
            }
        }
        if code_outside_comments(&punctuation).matches(';').count() != 2 {
            return Err(syntax_error(
                "a `for` statement's head needs both of its `;` separators",
                Some(source_span(head_span)),
            ));
        }
        Ok(())
    }

    /// `async` begins an async method only when no line terminator follows it
    /// (§15.8, *AsyncMethod*).
    pub(super) fn check_async_method_head(
        &self,
        method: &swc::MethodProp,
    ) -> Result<(), Diagnostic> {
        if !method.function.is_async {
            return Ok(());
        }
        let head = swc_common::Span::new(method.function.span.lo, method.key.span().lo);
        if self
            .source_text(head)
            .is_some_and(|text| code_outside_comments(text).contains(is_line_terminator))
        {
            return Err(syntax_error(
                "a line terminator cannot follow `async` in an async method",
                Some(source_span(head)),
            ));
        }
        Ok(())
    }
}

/// A regular-expression literal holds no line terminator (§12.9.5).
pub(super) fn check_regex_body(pattern: &str, span: Option<SourceSpan>) -> Result<(), Diagnostic> {
    if pattern.contains(is_line_terminator) {
        return Err(syntax_error(
            "a regular-expression literal cannot contain a line terminator",
            span,
        ));
    }
    Ok(())
}

fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscapeContext {
    Identifier,
    String,
    Template,
}

/// Checks each escape in `raw`, the source text of one token.
fn check_escapes(
    raw: &str,
    context: EscapeContext,
    span: Option<SourceSpan>,
) -> Result<(), Diagnostic> {
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            continue;
        }
        let Some(escaped) = characters.next() else {
            break;
        };
        match escaped {
            'u' if characters.peek() == Some(&'{') => {
                characters.next();
                let mut digits = 0_usize;
                let mut well_formed = true;
                for digit in characters.by_ref() {
                    if digit == '}' {
                        break;
                    }
                    digits += 1;
                    well_formed &= digit.is_ascii_hexdigit();
                }
                if !well_formed || digits == 0 {
                    return Err(syntax_error(
                        "invalid Unicode escape sequence: `\\u{…}` holds hex digits only",
                        span,
                    ));
                }
            }
            '8' | '9' if context == EscapeContext::String => {
                return Err(syntax_error(
                    "`\\8` and `\\9` are not allowed in strict mode",
                    span,
                ));
            }
            '0' if context != EscapeContext::Identifier
                && characters.peek().is_some_and(char::is_ascii_digit) =>
            {
                return Err(legacy_escape(context, span));
            }
            '1'..='9' if context != EscapeContext::Identifier => {
                return Err(legacy_escape(context, span));
            }
            _ => {}
        }
    }
    Ok(())
}

fn legacy_escape(context: EscapeContext, span: Option<SourceSpan>) -> Diagnostic {
    syntax_error(
        if context == EscapeContext::Template {
            "octal escape sequences are not allowed in template strings"
        } else {
            "octal escape sequences are not allowed in strict mode"
        },
        span,
    )
}

/// `text` with its comments removed: the code between two tokens.
fn code_outside_comments(text: &str) -> String {
    let mut code = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |end| &after[end + 2..]);
        } else if let Some(after) = rest.strip_prefix("//") {
            rest = after
                .find(is_line_terminator)
                .map_or("", |end| &after[end..]);
        } else {
            let mut characters = rest.chars();
            if let Some(character) = characters.next() {
                code.push(character);
            }
            rest = characters.as_str();
        }
    }
    code
}
