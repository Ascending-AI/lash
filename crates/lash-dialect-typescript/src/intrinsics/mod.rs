//! Exact reading of the reserved kernel source surface.
//!
//! This is TypeScript syntax with kernel semantics: no JavaScript helper,
//! implicit receiver, coercion, async wrapper or temporary is introduced.
use std::collections::BTreeMap;

use lash_kernel_dialect::{Environment, Lowered};
use lash_kernel_doc as k;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as s;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

mod expressions;
mod statements;

pub(crate) struct Reader<'a> {
    pub(crate) environment: &'a Environment<'a>,
    pub(crate) encoded_names: bool,
    pub(crate) effects: BTreeMap<k::EffectName, k::Signature>,
    pub(crate) functions: BTreeMap<k::FunctionId, k::FunctionName>,
}

/// The envelope starts with `k.document`. Trivia is insignificant. This
/// lexical probe only selects a parser; it never extracts executable code.
fn source_probe(source: &str) -> (bool, usize) {
    let bytes = source.as_bytes();
    let mut position = 0;
    let mut kernel = false;
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
                if &source[start..position] == "k"
                    && source[position..].trim_start().starts_with('.')
                {
                    kernel = true;
                }
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
                position += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                position += 1;
            }
            _ => position += 1,
        }
    }
    (kernel, deepest)
}

pub(crate) fn lower(
    source: &str,
    environment: &Environment<'_>,
) -> Option<Result<Lowered, Diagnostic>> {
    let (kernel, depth) = source_probe(source);
    if !kernel {
        return None;
    }
    // At most four syntax delimiters per admitted kernel node, plus the
    // document/declaration envelope. Bound conversion and AST destruction
    // on the caller's stack without imposing the ordinary dialect's limits.
    if depth > k::MAX_NESTING_DEPTH * 4 + 16 {
        return Some(Err(invalid(
            "kernel source exceeds its nesting limit",
            None,
        )));
    }
    let parsed = std::thread::scope(|scope| {
        // The kernel's depth limit exceeds the ordinary dialect's source
        // limit. Reserve parser stack here, then validate the resulting tree.
        let handle = std::thread::Builder::new()
            .name("kernel-typescript-parse".into())
            .stack_size(8 * 1024 * 1024 + source.len().saturating_mul(40_000))
            .spawn_scoped(scope, || parse_module(source))
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
    });
    Some(parsed.and_then(|module| read_module(module, environment)))
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

fn read_module(module: s::Module, environment: &Environment<'_>) -> Result<Lowered, Diagnostic> {
    let mut reader = Reader {
        environment,
        encoded_names: false,
        effects: BTreeMap::new(),
        functions: BTreeMap::new(),
    };
    let mut document = k::Document::new(k::NumberPolicy::Float, vec![]);
    let mut items = module.body.iter();
    let first = items.next();
    let header = first
        .and_then(module_expr)
        .and_then(|expr| invocation(expr).ok())
        .filter(|(name, _)| name == "document");
    if let Some((_, args)) = header {
        reader.encoded_names = true;
        let [manifest, entries, private] = args.as_slice() else {
            return Err(invalid(
                "k.document needs manifest, entries and private bindings",
                None,
            ));
        };
        document.manifest = decode(&string(manifest)?)
            .map_err(|error| invalid(error.to_string(), Some(manifest.span())))?;
        document.entries = decode(&string(entries)?)
            .map_err(|error| invalid(error.to_string(), Some(entries.span())))?;
        document.private_bindings = array(private)?
            .iter()
            .map(|expr| string(expr).map(k::Name::new))
            .collect::<Result<_, _>>()?;
        let mut main_seen = false;
        for item in items {
            let expr = module_expr(item)
                .ok_or_else(|| invalid("expected k.declare or k.main", Some(item.span())))?;
            let (operation, args) = invocation(expr)?;
            match (operation.as_str(), args.as_slice()) {
                ("declare", [name, function]) if !main_seen => {
                    let name = k::Name::new(string(name)?);
                    let function = reader.function(function)?;
                    if document.functions.insert(name, function).is_some() {
                        return Err(invalid("duplicate declared function", Some(expr.span())));
                    }
                }
                ("main", [function]) if !main_seen => {
                    let function = reader.function(function)?;
                    if !function.params.is_empty() {
                        return Err(invalid("main cannot have parameters", Some(expr.span())));
                    }
                    document.main = function.body;
                    main_seen = true;
                }
                _ => {
                    return Err(invalid(
                        "expected one main after declarations",
                        Some(expr.span()),
                    ));
                }
            }
        }
        if !main_seen {
            return Err(invalid("kernel source has no main", None));
        }
    } else {
        document.main = module
            .body
            .iter()
            .map(|item| match item {
                s::ModuleItem::Stmt(stmt) => reader.statement(stmt),
                _ => Err(invalid("kernel source has no imports", Some(item.span()))),
            })
            .collect::<Result<_, _>>()?;
        document.manifest.functions = reader.functions;
        document.manifest.effects = reader.effects;
    }
    k::validate_document(&document, environment.library)
        .map_err(|error| invalid(error.to_string(), None))?;
    let identity = document
        .identity()
        .map_err(|error| invalid(error.to_string(), None))?;
    let mut annotations = k::Annotations::new(identity);
    annotations.dialect = Some("typescript".into());
    Ok(Lowered {
        document,
        annotations,
    })
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
    let args = call
        .args
        .iter()
        .map(|arg| {
            if arg.spread.is_none() {
                Ok(arg.expr.as_ref())
            } else {
                Err(invalid(
                    "k operations have no spread arguments",
                    Some(expr.span()),
                ))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok((name.sym.to_string(), args))
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
impl Reader<'_> {
    pub(crate) fn name(&self, ident: &s::Ident) -> Result<k::Name, Diagnostic> {
        if !self.encoded_names {
            if ident.sym == "k" {
                return Err(invalid("k is reserved", Some(ident.span)));
            }
            return Ok(k::Name::new(ident.sym.to_string()));
        }
        let hex = ident
            .sym
            .strip_prefix("v_")
            .ok_or_else(|| invalid("expected an encoded kernel name", Some(ident.span)))?;
        let bytes = k::Bytes::parse_hex(hex)
            .map_err(|error| invalid(error.to_string(), Some(ident.span)))?;
        let name = String::from_utf8(bytes.as_slice().to_vec())
            .map_err(|error| invalid(error.to_string(), Some(ident.span)))?;
        Ok(k::Name::new(name))
    }
    pub(crate) fn id(&mut self, expr: &s::Expr) -> Result<k::FunctionId, Diagnostic> {
        let id = k::FunctionId::try_from(string(expr)?)
            .map_err(|error| invalid(error.to_string(), Some(expr.span())))?;
        self.require(id, expr)?;
        Ok(id)
    }
    fn require(&mut self, id: k::FunctionId, expr: &s::Expr) -> Result<(), Diagnostic> {
        if self.functions.contains_key(&id) {
            return Ok(());
        }
        let definition = self.environment.library.definition(&id).ok_or_else(|| {
            invalid(
                format!("library function {id} is unavailable"),
                Some(expr.span()),
            )
        })?;
        self.functions.insert(id, definition.name.clone());
        // Definitions reference their own transitive requirements by identity.
        if let Some(body) = definition.body() {
            for function in body.functions.keys() {
                self.require(*function, expr)?;
            }
        }
        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, serde_json::Error> {
    // Match the kernel document decoder's non-recursive depth preflight.
    if source_probe(text).1 > k::MAX_NESTING_DEPTH * 5 + 32 {
        return Err(serde::de::Error::custom(
            "kernel metadata exceeds its nesting limit",
        ));
    }
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(value)
}
