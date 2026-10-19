//! Exact reading of the reserved kernel source surface.
//!
//! This is TypeScript syntax with kernel semantics: no JavaScript helper,
//! implicit receiver, coercion, async wrapper or temporary is introduced.
use std::collections::{BTreeMap, HashMap};

use lash_kernel_dialect::{GeneratedSourceLimits, check_document_depth, check_metadata_uniqueness};
use lash_kernel_doc as k;
use swc_common::{BytePos, Spanned};
use swc_ecma_ast as s;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::captions::SourceComments;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

mod expressions;
mod native;
mod readable;
mod statements;

pub(crate) struct Reader {
    /// The source's comments and the captions they spell.
    comments: SourceComments,
    /// The captions of each block read, by the address of its statements,
    /// which moving the block into the document keeps: each statement's,
    /// by its index, and those of its arms, by the arm's child index.
    captioned: HashMap<*const k::Stmt, Vec<StatementCaptions>>,
    /// The captions of declared functions.
    function_captions: Vec<(k::Name, k::Caption)>,
    limits: k::KernelDataLimits,
    names: BTreeMap<String, k::Name>,
    helpers: BTreeMap<String, k::FunctionId>,
    operators: crate::generated_operators::Operators,
    types: lash_kernel_dialect::source_types::Types,
}

/// The deepest open-delimiter nesting of `source`, outside comments and
/// quoted text. It bounds the parser's stack before SWC sees the source.
fn syntax_depth(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut position = 0;
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
    deepest
}

/// Read exported TypeScript back into the document it was printed from, or
/// into that document as edited, without library lookup or admission.
///
/// The source carries only code. The manifest, entries, private bindings,
/// helper identities and name renames come from `base`, the document the
/// source was printed from: printing `base` and reading the result with
/// `base` gives `base` again, with the same identity.
///
/// The captions are those the source spells, as the printer writes them: a
/// JSDoc block before a statement, a declared function or an `else`,
/// `catch` or `finally` keyword. Any other comment is trivia.
pub fn read(
    source: &str,
    base: &k::Document,
    limits: GeneratedSourceLimits,
) -> Result<(k::Document, k::Captions), Diagnostic> {
    let (document, entries) = read_document(source, base, limits)?;
    let captions = if entries.is_empty() {
        k::Captions::empty(document.identity())
    } else {
        k::Captions::new(&document, entries)
            .map_err(|refused| invalid(format!("misplaced caption: {refused}"), None))?
    };
    Ok((document, captions))
}

fn read_document(
    source: &str,
    base: &k::Document,
    limits: GeneratedSourceLimits,
) -> Result<(k::Document, Vec<k::CaptionEntry>), Diagnostic> {
    if source.len() > limits.max_bytes {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceBytes,
            "generated source exceeds byte budget",
            None,
        ));
    }
    if k::KernelVersion::of(base.manifest.kernel).is_none() {
        return Err(format_error("unsupported kernel version"));
    }
    let depth = syntax_depth(source);
    let node_limit = limits.max_depth.min(64);
    // At most four syntax delimiters per admitted kernel node, plus the
    // function declarations around them. Bound conversion and AST destruction
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
                let (mut document, captions) = parse_module(source)
                    .and_then(|(module, comments)| read_module(module, comments, base))?;
                // Source spells no start adapter: the base keeps them.
                k::carry_start_adapters(base, &mut document);
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
                Ok((document, captions))
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

fn parse_module(source: &str) -> Result<(s::Module, SourceComments), Diagnostic> {
    let end = u32::try_from(source.len())
        .map_err(|_| invalid("source exceeds parser address space", None))?;
    let comments = swc_common::comments::SingleThreadedComments::default();
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax::default()),
        Default::default(),
        StringInput::new(source, BytePos(0), BytePos(end)),
        Some(&comments),
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser
        .parse_module()
        .map_err(|error| invalid(format!("{error:?}"), Some(error.span())))?;
    if let Some(error) = parser.take_errors().first() {
        return Err(invalid(format!("{error:?}"), Some(error.span())));
    }
    drop(parser);
    Ok((module, SourceComments::collect(source, comments)))
}

fn read_module(
    module: s::Module,
    comments: SourceComments,
    base: &k::Document,
) -> Result<(k::Document, Vec<k::CaptionEntry>), Diagnostic> {
    let plan = lash_kernel_dialect::NamePlan::new(base, crate::printer::legal_identifier);
    let mut reader = Reader {
        comments,
        captioned: HashMap::new(),
        function_captions: Vec::new(),
        limits: k::KernelDataLimits::standard(),
        names: plan
            .names
            .into_iter()
            .filter(|(name, alias)| name.as_str() != alias)
            .map(|(name, alias)| (alias, name))
            .collect(),
        helpers: plan
            .helpers
            .into_iter()
            .map(|(id, alias)| (alias, id))
            .collect(),
        operators: crate::generated_operators::bindings(&base.manifest),
        types: Default::default(),
    };
    let mut document = k::Document::new(base.manifest.numbers, vec![]);
    document.manifest = base.manifest.clone();
    document.entries = base.entries.clone();
    document.private_bindings = base.private_bindings.clone();
    let items = module.body.iter();
    let mut main_seen = false;
    for item in items {
        let (declaration, exported) = match item {
            s::ModuleItem::Stmt(s::Stmt::Decl(s::Decl::Fn(declaration))) => (declaration, false),
            s::ModuleItem::ModuleDecl(s::ModuleDecl::ExportDecl(s::ExportDecl {
                decl: s::Decl::Fn(declaration),
                ..
            })) => (declaration, true),
            _ => {
                return Err(format_error(
                    "expected native function declarations and main",
                ));
            }
        };
        if declaration.declare || main_seen {
            return Err(format_error(
                "expected one main after declarations; trailing text is forbidden",
            ));
        }
        let function = reader.read_function(&declaration.function)?;
        let caption = reader.comments.before(item.span().lo.0 as usize).cloned();
        if declaration.ident.sym == "main" {
            if exported {
                return Err(format_error("main is not an entry and is not exported"));
            }
            if !function.params.is_empty() {
                return Err(format_error("main has no parameters"));
            }
            document.main = function.body;
            main_seen = true;
        } else {
            let name = reader.name(&declaration.ident)?;
            // Entries and their signatures are the base document's; the
            // source cannot add, drop or retype one.
            if exported != document.entries.contains_key(&name) {
                return Err(format_error(format!(
                    "`{name}` is {} in the base document; exporting a function is what makes it an entry",
                    if exported { "not an entry" } else { "an entry" }
                )));
            }
            if let Some(caption) = caption {
                reader.function_captions.push((name.clone(), caption));
            }
            if document.functions.insert(name, function).is_some() {
                return Err(format_error("duplicate declaration"));
            }
        }
    }
    if !main_seen {
        return Err(format_error("missing main"));
    }
    let captions = reader.located(&document);
    Ok((document, captions))
}

/// The captions of one statement of a block read: its own, and those of its
/// arms by their child index.
struct StatementCaptions {
    index: u32,
    caption: Option<k::Caption>,
    arms: Vec<(u32, k::Caption)>,
}

impl Reader {
    /// The captions spelled before `stmt` and before the keywords of its
    /// clause arms.
    fn statement_captions(&self, index: u32, stmt: &s::Stmt) -> Option<StatementCaptions> {
        let caption = self.comments.before(stmt.span().lo.0 as usize).cloned();
        let mut arms = Vec::new();
        let mut arm = |child: u32, end: BytePos| {
            if let Some(caption) = self.comments.before_next(end.0 as usize) {
                arms.push((child, caption.clone()));
            }
        };
        match stmt {
            s::Stmt::If(statement) if statement.alt.is_some() => arm(2, statement.cons.span().hi),
            s::Stmt::Try(statement) => {
                let mut end = statement.block.span.hi;
                let mut child = 1;
                if let Some(handler) = &statement.handler {
                    arm(child, end);
                    end = handler.span.hi;
                    child += 1;
                }
                if statement.finalizer.is_some() {
                    arm(child, end);
                }
            }
            _ => {}
        }
        (caption.is_some() || !arms.is_empty()).then_some(StatementCaptions {
            index,
            caption,
            arms,
        })
    }

    /// Keeps the captions of `block`, read from `statements`.
    fn keep_captions(&mut self, block: &k::Block, statements: &[s::Stmt]) {
        let captions: Vec<_> = (0u32..)
            .zip(statements)
            .filter_map(|(index, stmt)| self.statement_captions(index, stmt))
            .collect();
        if !captions.is_empty() {
            self.captioned.insert(block.as_ptr(), captions);
        }
    }

    /// Every caption read, at its site in `document`.
    fn located(&mut self, document: &k::Document) -> Vec<k::CaptionEntry> {
        let mut entries: Vec<_> = std::mem::take(&mut self.function_captions)
            .into_iter()
            .map(|(name, caption)| k::CaptionEntry {
                site: k::Site::new(k::Unit::Function(name), Vec::new()),
                caption,
            })
            .collect();
        if self.captioned.is_empty() {
            return entries;
        }
        let roots = std::iter::once((k::Unit::Main, &document.main)).chain(
            document
                .functions
                .iter()
                .map(|(name, function)| (k::Unit::Function(name.clone()), &function.body)),
        );
        for (unit, body) in roots {
            let mut pending = vec![(k::Node::Block(body), k::Site::new(unit, Vec::new()))];
            while let Some((node, site)) = pending.pop() {
                if let k::Node::Block(block) = node
                    && let Some(captions) = self.captioned.remove(&block.as_ptr())
                {
                    for statement in captions {
                        let at = site.child(statement.index);
                        for (child, caption) in statement.arms {
                            entries.push(k::CaptionEntry {
                                site: at.child(child),
                                caption,
                            });
                        }
                        if let Some(caption) = statement.caption {
                            entries.push(k::CaptionEntry { site: at, caption });
                        }
                    }
                }
                for (index, child) in (0u32..).zip(node.children()) {
                    pending.push((child, site.child(index)));
                }
            }
        }
        entries
    }
}
fn format_error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(DiagnosticCode::GeneratedSourceFormat, message, None)
}

pub(crate) fn invalid(message: impl Into<String>, span: Option<swc_common::Span>) -> Diagnostic {
    Diagnostic::with_repair(
        DiagnosticCode::InvalidAst,
        message,
        "use the documented generated TypeScript syntax and its exact operands",
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
            .or_else(|| {
                (crate::printer::legal_identifier(ident.sym.as_ref())
                    && !self
                        .names
                        .values()
                        .any(|name| name.as_str() == ident.sym.as_ref()))
                .then(|| k::Name::new(ident.sym.as_ref()))
            })
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
    if syntax_depth(text) > limits.document_json_depth() {
        return Err(Diagnostic::new(
            DiagnosticCode::GeneratedSourceDepth,
            "kernel result type exceeds its nesting limit",
            None,
        ));
    }
    check_metadata_uniqueness(text).map_err(|error| format_error(error.to_string()))?;
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let value = T::deserialize(&mut decoder).map_err(|error| format_error(error.to_string()))?;
    decoder
        .end()
        .map_err(|error| format_error(error.to_string()))?;
    Ok(value)
}

pub(super) fn json_string(value: &str) -> Result<String, Diagnostic> {
    serde_json::to_string(value).map_err(|error| invalid(error.to_string(), None))
}
