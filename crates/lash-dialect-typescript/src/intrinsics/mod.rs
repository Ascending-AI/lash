//! Exact reading of the reserved kernel source surface.
//!
//! This is TypeScript syntax with kernel semantics: no JavaScript helper,
//! implicit receiver, coercion, async wrapper or temporary is introduced.
use std::collections::BTreeMap;

use lash_kernel_dialect::{GeneratedSourceLimits, check_document_depth};
use lash_kernel_doc as k;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as s;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

mod expressions;
mod metadata;
mod readable;
mod statements;

pub(crate) struct Reader {
    limits: k::KernelDataLimits,
    names: BTreeMap<String, k::Name>,
    helpers: BTreeMap<String, k::FunctionId>,
}

/// A generated module starts with `k.document` (or an incomplete declare/main). Trivia is insignificant. This
/// lexical probe only selects a parser; it never extracts executable code.
pub(crate) fn source_probe(source: &str) -> (bool, usize) {
    let bytes = source.as_bytes();
    let mut position = 0;
    let mut kernel = false;
    let mut envelope_token = 0;
    let mut depth = 0usize;
    let mut deepest = 0usize;
    while position < bytes.len() {
        match bytes[position] {
            b'/' if bytes.get(position + 1) == Some(&b'/') => {
                position += 2;
                while bytes.get(position).is_some_and(|byte| *byte != b'\n') {
                    position += 1;
                }
            }
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                position += 2;
                while position + 1 < bytes.len() && &bytes[position..position + 2] != b"*/" {
                    position += 1;
                }
                position += 2;
            }
            quote @ (b'\'' | b'"' | b'`') => {
                envelope_token = 3;
                position += 1;
                while position < bytes.len() {
                    if bytes[position] == b'\\' {
                        position += 2;
                    } else if bytes[position] == quote {
                        position += 1;
                        break;
                    } else {
                        position += 1;
                    }
                }
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' => {
                let start = position;
                position += 1;
                while bytes.get(position).is_some_and(|byte| {
                    byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$'
                }) {
                    position += 1;
                }
                let token = &source[start..position];
                if envelope_token == 2 && matches!(token, "document" | "declare" | "main") {
                    kernel = true;
                }
                envelope_token = if envelope_token == 0 && token == "k" {
                    1
                } else {
                    3
                };
            }
            b'.' => {
                envelope_token = if envelope_token == 1 { 2 } else { 3 };
                position += 1;
            }
            b'(' | b'[' | b'{' => {
                envelope_token = 3;
                depth += 1;
                deepest = deepest.max(depth);
                position += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                position += 1;
            }
            byte if byte.is_ascii_whitespace() => position += 1,
            _ => {
                envelope_token = 3;
                position += 1;
            }
        }
    }
    (kernel, deepest)
}

/// Decode format v1 without library lookup or admission.
pub fn compile_generated(
    source: &str,
    limits: GeneratedSourceLimits,
) -> Result<k::Document, Diagnostic> {
    if source.len() > limits.max_bytes {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceBytes,
            "generated source exceeds byte budget",
            None,
        ));
    }
    let (_, depth) = source_probe(source);
    let node_limit = limits.max_depth.min(64);
    // At most four syntax delimiters per admitted kernel node, plus the
    // document/declaration envelope. Bound conversion and AST destruction
    // on the caller's stack without imposing the ordinary dialect's limits.
    if depth > node_limit * 4 + 16 {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceDepth,
            "generated source exceeds syntax depth budget",
            None,
        ));
    }
    // Include operators and postfix chains as well as open delimiters: even
    // syntax the kernel reader will reject can recurse in SWC first. Measuring
    // does not impose the ordinary cell's much smaller nesting limit.
    let parse_depth = match crate::adapter::nesting::source_nesting_depth(source, usize::MAX) {
        Ok(depth) => depth,
        Err(span) => {
            return Err(Diagnostic::new(
                DiagnosticCode::ParseResourcesUnavailable,
                "kernel parser nesting exceeds address space",
                Some(span),
            ));
        }
    };
    if parse_depth > node_limit * 4 + 16 {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceDepth,
            "generated source exceeds parser depth budget",
            None,
        ));
    }
    std::thread::scope(|scope| {
        // The kernel's depth limit exceeds the ordinary dialect's source
        // limit. The 40,000-byte allowance per recursive level retains the
        // measured parser-frame margin of adapter::parser_stack, without
        // reserving stack for every byte of a wide printed document.
        let stack_size = parse_depth
            .checked_mul(40_000)
            .and_then(|bytes| bytes.checked_add(8 * 1024 * 1024))
            .ok_or_else(|| {
                Diagnostic::new(
                    DiagnosticCode::ParseResourcesUnavailable,
                    "kernel parser stack reservation exceeds address space",
                    None,
                )
            })?;
        let handle = std::thread::Builder::new()
            .name("kernel-typescript-parse".into())
            .stack_size(stack_size)
            .spawn_scoped(scope, || {
                let document = parse_module(source).and_then(read_module)?;
                check_document_depth(&document, limits).map_err(|error| {
                    Diagnostic::new(
                        match error {
                            lash_kernel_dialect::PrintError::Bytes { .. } => {
                                DiagnosticCode::GeneratedSourceBytes
                            }
                            _ => DiagnosticCode::GeneratedSourceDepth,
                        },
                        error.to_string(),
                        None,
                    )
                })?;
                Ok(document)
            })
            .map_err(|error| {
                Diagnostic::new(
                    DiagnosticCode::ParseResourcesUnavailable,
                    error.to_string(),
                    None,
                )
            })?;
        handle
            .join()
            .unwrap_or_else(|_| Err(invalid("kernel source parser failed", None)))
    })
}

fn parse_module(source: &str) -> Result<s::Module, Diagnostic> {
    let end = u32::try_from(source.len())
        .map_err(|_| invalid("source exceeds parser address space", None))?;
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax::default()),
        Default::default(),
        StringInput::new(source, BytePos(0), BytePos(end)),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_module()
        .map_err(|error| invalid(format!("{error:?}"), Some(error.span())))?;
    if let Some(error) = parser.take_errors().first() {
        return Err(invalid(format!("{error:?}"), Some(error.span())));
    }
    Ok(module)
}

fn read_module(module: s::Module) -> Result<k::Document, Diagnostic> {
    let mut reader = Reader {
        limits: k::KernelDataLimits::standard(),
        names: BTreeMap::new(),
        helpers: BTreeMap::new(),
    };
    let mut document = k::Document::new(k::NumberPolicy::Float, vec![]);
    let mut items = module.body.iter();
    let header = items
        .next()
        .and_then(module_expr)
        .and_then(|e| invocation(e).ok())
        .filter(|(name, _)| name == "document")
        .ok_or_else(|| format_error("missing generated-source v1 envelope"))?;
    let header_result = (|| -> Result<(), Diagnostic> {
        let [format, version, manifest, entries, private, names, helpers] = header.1.as_slice()
        else {
            return Err(format_error(
                "obsolete or malformed generated envelope; re-export the Document with this toolchain",
            ));
        };
        if string(format)? != "lash.generated.typescript" || string(version)? != "1" {
            return Err(format_error(
                "unsupported generated-source format or version",
            ));
        }
        document.manifest = decode(&string(manifest)?, reader.limits)?;
        if k::KernelVersion::of(document.manifest.kernel).is_none() {
            return Err(format_error("unsupported kernel version"));
        }
        document.entries = decode(&string(entries)?, reader.limits)?;
        for value in array(private)? {
            if !document
                .private_bindings
                .insert(k::Name::new(string(value)?))
            {
                return Err(format_error("duplicate private binding"));
            }
        }
        let aliases: Vec<(String, k::Name)> = decode(&string(names)?, reader.limits)?;
        let mut originals = std::collections::BTreeSet::new();
        for (alias, name) in aliases {
            let valid_alias = crate::printer::legal_identifier(&alias)
                || alias.strip_prefix("kernel_name_").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                });
            if !valid_alias
                || !originals.insert(name.clone())
                || reader.names.insert(alias, name).is_some()
            {
                return Err(format_error(
                    "duplicate, reserved or contradictory identifier alias",
                ));
            }
        }
        let aliases: Vec<(String, k::FunctionId)> = decode(&string(helpers)?, reader.limits)?;
        let mut ids = std::collections::BTreeSet::new();
        for (alias, id) in aliases {
            if !alias.strip_prefix("helper_").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.chars().all(swc_ecma_ast::Ident::is_valid_continue)
            }) || !ids.insert(id)
                || reader.helpers.insert(alias, id).is_some()
            {
                return Err(format_error(
                    "duplicate, reserved or contradictory helper alias",
                ));
            }
        }
        Ok(())
    })();
    header_result.map_err(|mut error| {
        if !matches!(
            error.code,
            DiagnosticCode::GeneratedSourceBytes | DiagnosticCode::GeneratedSourceDepth
        ) {
            error.code = DiagnosticCode::GeneratedSourceFormat;
        }
        error
    })?;
    let mut main_seen = false;
    for item in items {
        let expr =
            module_expr(item).ok_or_else(|| format_error("expected a declaration or main"))?;
        let (operation, args) = invocation(expr)?;
        match (operation.as_str(), args.as_slice()) {
            ("declare", [name, function]) if !main_seen => {
                let name = k::Name::new(string(name)?);
                let function = reader.function(function)?;
                if document.functions.insert(name, function).is_some() {
                    return Err(format_error("duplicate declaration"));
                }
            }
            ("main", [function]) if !main_seen => {
                let function = reader.function(function)?;
                if !function.params.is_empty() {
                    return Err(format_error("main has no parameters"));
                }
                document.main = function.body;
                main_seen = true;
            }
            _ => {
                return Err(format_error(
                    "expected one main after declarations; trailing text is forbidden",
                ));
            }
        }
    }
    if !main_seen {
        return Err(format_error("missing main"));
    }
    Ok(document)
}
fn format_error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(DiagnosticCode::GeneratedSourceFormat, message, None)
}
fn module_expr(item: &s::ModuleItem) -> Option<&s::Expr> {
    if let s::ModuleItem::Stmt(s::Stmt::Expr(statement)) = item {
        Some(&statement.expr)
    } else {
        None
    }
}

pub(crate) fn invalid(message: impl Into<String>, span: Option<swc_common::Span>) -> Diagnostic {
    Diagnostic::with_repair(
        DiagnosticCode::InvalidAst,
        message,
        "use the reserved k operation with its kernel operands",
        span.map(|span| SourceSpan {
            start: span.lo.0 as usize,
            end: span.hi.0 as usize,
        }),
    )
}

pub(crate) fn string(expr: &s::Expr) -> Result<String, Diagnostic> {
    if let s::Expr::Lit(s::Lit::Str(value)) = expr {
        value.value.as_str().map(str::to_owned).ok_or_else(|| {
            invalid(
                "kernel text must contain Unicode scalars",
                Some(expr.span()),
            )
        })
    } else {
        Err(invalid("expected a text literal", Some(expr.span())))
    }
}

pub(crate) fn invocation(expr: &s::Expr) -> Result<(String, Vec<&s::Expr>), Diagnostic> {
    let s::Expr::Call(call) = expr else {
        return Err(invalid("expected a k operation", Some(expr.span())));
    };
    let s::Callee::Expr(callee) = &call.callee else {
        return Err(invalid("expected k.operation", Some(expr.span())));
    };
    let s::Expr::Member(member) = callee.as_ref() else {
        return Err(invalid("expected k.operation", Some(expr.span())));
    };
    let s::Expr::Ident(root) = member.obj.as_ref() else {
        return Err(invalid(
            "expected the reserved k namespace",
            Some(expr.span()),
        ));
    };
    let s::MemberProp::Ident(name) = &member.prop else {
        return Err(invalid("expected a named k operation", Some(expr.span())));
    };
    if root.sym != "k" || call.type_args.is_some() {
        return Err(invalid(
            "expected the reserved k namespace",
            Some(expr.span()),
        ));
    }
    Ok((name.sym.to_string(), call_arguments(call)?))
}

fn call_arguments(call: &s::CallExpr) -> Result<Vec<&s::Expr>, Diagnostic> {
    if call.type_args.is_some() {
        return Err(invalid(
            "generated calls have no type arguments",
            Some(call.span()),
        ));
    }
    call.args
        .iter()
        .map(|arg| {
            if arg.spread.is_none() {
                Ok(arg.expr.as_ref())
            } else {
                Err(invalid(
                    "k operations have no spread arguments",
                    Some(call.span()),
                ))
            }
        })
        .collect()
}

pub(crate) fn array(expr: &s::Expr) -> Result<Vec<&s::Expr>, Diagnostic> {
    let s::Expr::Array(array) = expr else {
        return Err(invalid("expected an operand array", Some(expr.span())));
    };
    array
        .elems
        .iter()
        .map(|item| match item {
            Some(item) if item.spread.is_none() => Ok(item.expr.as_ref()),
            _ => Err(invalid(
                "kernel operand arrays have neither holes nor spread",
                Some(expr.span()),
            )),
        })
        .collect()
}
impl Reader {
    pub(crate) fn name(&self, ident: &s::Ident) -> Result<k::Name, Diagnostic> {
        self.names
            .get(ident.sym.as_ref())
            .cloned()
            .ok_or_else(|| invalid("undeclared identifier alias", Some(ident.span)))
    }
    pub(crate) fn id(&self, expr: &s::Expr) -> Result<k::FunctionId, Diagnostic> {
        let s::Expr::Ident(ident) = expr else {
            return Err(invalid(
                "expected a full-identity helper alias",
                Some(expr.span()),
            ));
        };
        self.helpers
            .get(ident.sym.as_ref())
            .copied()
            .ok_or_else(|| invalid("undeclared helper alias", Some(expr.span())))
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    text: &str,
    limits: k::KernelDataLimits,
) -> Result<T, Diagnostic> {
    // Match the kernel document decoder's non-recursive depth preflight.
    if source_probe(text).1 > limits.document_json_depth() {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceDepth,
            "kernel metadata exceeds its nesting limit",
            None,
        ));
    }
    metadata::check(text).map_err(|error| format_error(error.to_string()))?;
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|error| format_error(error.to_string()))?;
    decoder
        .end()
        .map_err(|error| format_error(error.to_string()))?;
    Ok(value)
}
