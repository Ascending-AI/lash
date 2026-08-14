use std::cell::Cell;

use swc_common::{BytePos, Span, Spanned};
use swc_ecma_ast as swc;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

mod nesting;

use nesting::{guard_source_nesting, source_nesting_diagnostic};

/// Maximum source-level statement or expression nesting accepted by the
/// TypeScript dialect. This is deliberately below the shared AST and 2 MiB
/// native-stack limits.
pub const MAX_SOURCE_NESTING_DEPTH: usize = 28;

#[derive(Clone, Debug)]
pub(crate) struct Program {
    pub(crate) statements: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub(crate) enum Stmt {
    Empty,
    Expr(Expr),
    Block(Vec<Stmt>),
    Var {
        kind: VarKind,
        declarations: Vec<Var>,
    },
    Function {
        name: String,
        function: Function,
    },
    Return(Option<Expr>),
    If {
        test: Expr,
        consequent: Box<Stmt>,
        alternate: Option<Box<Stmt>>,
    },
    While {
        test: Expr,
        body: Box<Stmt>,
    },
    Break,
    Continue,
    Throw(Expr),
    Try {
        body: Vec<Stmt>,
        catch: Option<Catch>,
        finally: Option<Vec<Stmt>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VarKind {
    Let,
    Const,
}

#[derive(Clone, Debug)]
pub(crate) struct Var {
    pub(crate) name: String,
    pub(crate) init: Option<Expr>,
}

#[derive(Clone, Debug)]
pub(crate) struct Catch {
    pub(crate) binding: String,
    pub(crate) body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub(crate) struct Function {
    pub(crate) name: Option<String>,
    pub(crate) params: Vec<String>,
    pub(crate) body: FunctionBody,
}

#[derive(Clone, Debug)]
pub(crate) enum FunctionBody {
    Block(Vec<Stmt>),
    Expression(Box<Expr>),
}

#[derive(Clone, Debug)]
pub(crate) enum Expr {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Ident(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Assign {
        target: AssignTarget,
        value: Box<Expr>,
    },
    Member {
        object: Box<Expr>,
        property: MemberProperty,
    },
    Unary {
        op: UnaryOp,
        value: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Logical {
        left: Box<Expr>,
        op: LogicalOp,
        right: Box<Expr>,
    },
    Conditional {
        test: Box<Expr>,
        consequent: Box<Expr>,
        alternate: Box<Expr>,
    },
    Template {
        quasis: Vec<String>,
        expressions: Vec<Expr>,
    },
    Function(Function),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum AssignTarget {
    Ident(String),
    Member {
        object: Box<Expr>,
        property: MemberProperty,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum MemberProperty {
    Field(String),
    Index(Box<Expr>),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum UnaryOp {
    Plus,
    Minus,
    Not,
    TypeOf,
    Void,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    StrictEqual,
    StrictNotEqual,
    LooseEqual,
    LooseNotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LogicalOp {
    And,
    Or,
    Nullish,
}

/// The largest TypeScript cell this dialect will read. Sources above it reject
/// with `TS_SOURCE_TOO_LARGE`; the bound is what makes the parse stack
/// reservation below finite. 64 KiB is roughly 1 600 lines of TypeScript.
pub const MAX_SOURCE_BYTES: usize = 64 * 1024;

/// Stack reserved before any source-proportional allowance.
///
/// It covers the parser's fixed frames and everything downstream of the parse,
/// which is bounded independently of the source: [`Adapter`] refuses to convert
/// past [`MAX_SOURCE_NESTING_DEPTH`], so the normalized tree it produces is at
/// most that deep, the lowerer walks that tree, and `lashlang` then rejects any
/// shared AST deeper than its own `MAX_AST_NESTING_DEPTH`. Only the parse and
/// the drop of its output scale with the source, which is what the allowance
/// below pays for.
const PARSE_STACK_BASE_BYTES: usize = 8 * 1024 * 1024;

/// Parser stack reserved per source byte.
///
/// A nesting level costs at least two source bytes — a delimiter needs an
/// opener and a closer, a label needs a name byte and its `:` — so a source of
/// `n` bytes cannot nest deeper than `n / 2` levels. The worst measured frame
/// cost across the whole six-round abort corpus is 19 552 bytes per level, on
/// parenthesis nesting in a debug build (release: 2 812; label chains, the
/// densest in *source* terms, cost 10 845). Stack usage is linear in depth:
/// measured at depths 1 000, 2 000, 4 000 and 8 000, the per-level cost varies
/// by under half a percent.
///
/// So the requirement is at most `19 552 / 2 = 9 776` bytes per source byte,
/// and reserving 40 000 leaves a 4.09x margin over the worst debug shape and
/// 28x over the worst release shape. With [`MAX_SOURCE_BYTES`] at 64 KiB the
/// largest reservation is 8 MiB + 2.44 GiB, which is address space rather than
/// memory: pages commit only when touched, and an ordinary cell touches a few
/// hundred kilobytes. Overflow is arithmetically out of reach rather than
/// guarded against, which is the point — see `nesting.rs` for why that matters.
const PARSE_STACK_BYTES_PER_SOURCE_BYTE: usize = 40_000;

/// The stack a source of this size is parsed on.
fn parse_stack_size(source_bytes: usize) -> usize {
    PARSE_STACK_BASE_BYTES + PARSE_STACK_BYTES_PER_SOURCE_BYTE * source_bytes
}

fn guard_source_size(source: &str) -> Result<(), Diagnostic> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Diagnostic::new(
            DiagnosticCode::SourceTooLarge,
            format!(
                "TypeScript source is {} bytes, over the {MAX_SOURCE_BYTES}-byte limit; split the cell",
                source.len()
            ),
            None,
        ));
    }
    Ok(())
}

/// The full front-end entry: bound the source, reject nesting with a
/// source-level diagnostic, then parse on a stack sized for the source.
pub(crate) fn parse(source: &str) -> Result<Program, Diagnostic> {
    guard_source_size(source)?;
    guard_source_nesting(source)?;
    parse_on_proportional_stack(source)
}

/// The same path with the nesting preflight removed, so a test can demonstrate
/// that the no-abort guarantee rests on the stack reservation rather than on
/// the preflight agreeing with SWC about the shape of the source.
#[cfg(feature = "testing")]
pub(crate) fn parse_without_nesting_preflight(source: &str) -> Result<Program, Diagnostic> {
    guard_source_size(source)?;
    parse_on_proportional_stack(source)
}

/// Parse on the caller's own stack with no guard at all. Only a stack
/// measurement wants this; everything else goes through [`parse`].
#[cfg(feature = "testing")]
pub(crate) fn parse_unguarded(source: &str) -> Result<Program, Diagnostic> {
    parse_source(source)
}

fn parse_on_proportional_stack(source: &str) -> Result<Program, Diagnostic> {
    let stack_size = parse_stack_size(source.len());
    std::thread::scope(|scope| {
        let handle = std::thread::Builder::new()
            .name("typescript-parse".to_string())
            .stack_size(stack_size)
            .spawn_scoped(scope, || parse_source(source))
            .map_err(|error| {
                Diagnostic::new(
                    DiagnosticCode::InvalidAst,
                    format!("the TypeScript parse thread could not be started: {error}"),
                    None,
                )
            })?;
        handle.join().unwrap_or_else(|_| {
            Err(Diagnostic::new(
                DiagnosticCode::SyntaxError,
                "the TypeScript parser failed while reading this source",
                None,
            ))
        })
    })
}

fn parse_source(source: &str) -> Result<Program, Diagnostic> {
    let end = u32::try_from(source.len()).unwrap_or(u32::MAX);
    let lexer = Lexer::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..TsSyntax::default()
        }),
        Default::default(),
        StringInput::new(source, BytePos(0), BytePos(end)),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    let module = parser.parse_module().map_err(parser_diagnostic)?;
    if let Some(error) = parser.take_errors().into_iter().next() {
        return Err(parser_diagnostic(error));
    }
    Adapter::default()
        .convert_module_items(&module.body)
        .map(|statements| Program { statements })
}

#[derive(Default)]
struct Adapter {
    nesting_depth: Cell<usize>,
}

impl Adapter {
    fn with_statement_depth<T>(
        &self,
        span: Option<SourceSpan>,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        self.with_depth(span, convert)
    }

    fn with_expression_depth<T>(
        &self,
        span: Option<SourceSpan>,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        self.with_depth(span, convert)
    }

    fn with_depth<T>(
        &self,
        span: Option<SourceSpan>,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        let next = self.nesting_depth.get() + 1;
        if next > MAX_SOURCE_NESTING_DEPTH {
            return Err(source_nesting_diagnostic(span));
        }
        self.nesting_depth.set(next);
        let result = convert();
        self.nesting_depth.set(next - 1);
        result
    }

    fn convert_module_items(&self, items: &[swc::ModuleItem]) -> Result<Vec<Stmt>, Diagnostic> {
        items
            .iter()
            .map(|item| match item {
                swc::ModuleItem::Stmt(stmt) => self.convert_stmt(stmt),
                swc::ModuleItem::ModuleDecl(decl) => Err(reject(
                    DiagnosticCode::ImportExportUnsupported,
                    "static import/export declarations",
                    Some(source_span(decl.span())),
                )),
            })
            .collect()
    }

    fn convert_statements(&self, statements: &[swc::Stmt]) -> Result<Vec<Stmt>, Diagnostic> {
        statements
            .iter()
            .map(|stmt| self.convert_stmt(stmt))
            .collect()
    }

    fn convert_stmt(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        if !matches!(
            stmt,
            swc::Stmt::If(_) | swc::Stmt::While(_) | swc::Stmt::Try(_) | swc::Stmt::Decl(_)
        ) {
            return self.convert_stmt_inner(stmt);
        }
        self.with_statement_depth(Some(source_span(stmt.span())), || {
            self.convert_stmt_inner(stmt)
        })
    }

    fn convert_stmt_inner(&self, stmt: &swc::Stmt) -> Result<Stmt, Diagnostic> {
        let span = Some(source_span(stmt.span()));
        Ok(match stmt {
            swc::Stmt::Empty(_) => Stmt::Empty,
            swc::Stmt::Expr(stmt) => Stmt::Expr(self.convert_expr(&stmt.expr)?),
            swc::Stmt::Block(block) => Stmt::Block(self.convert_statements(&block.stmts)?),
            swc::Stmt::Return(stmt) => Stmt::Return(
                stmt.arg
                    .as_deref()
                    .map(|expr| self.convert_expr(expr))
                    .transpose()?,
            ),
            swc::Stmt::If(stmt) => Stmt::If {
                test: self.convert_expr(&stmt.test)?,
                consequent: Box::new(self.convert_stmt(&stmt.cons)?),
                alternate: stmt
                    .alt
                    .as_deref()
                    .map(|stmt| self.convert_stmt(stmt).map(Box::new))
                    .transpose()?,
            },
            swc::Stmt::While(stmt) => Stmt::While {
                test: self.convert_expr(&stmt.test)?,
                body: Box::new(self.convert_stmt(&stmt.body)?),
            },
            swc::Stmt::Break(_) => Stmt::Break,
            swc::Stmt::Continue(_) => Stmt::Continue,
            swc::Stmt::Throw(stmt) => Stmt::Throw(self.convert_expr(&stmt.arg)?),
            swc::Stmt::Try(stmt) => Stmt::Try {
                body: self.convert_statements(&stmt.block.stmts)?,
                catch: stmt
                    .handler
                    .as_ref()
                    .map(|handler| {
                        let Some(swc::Pat::Ident(binding)) = handler.param.as_ref() else {
                            return Err(Diagnostic::new(
                                DiagnosticCode::EmptyCatchBindingUnsupported,
                                "catch requires one identifier binding",
                                Some(source_span(handler.span)),
                            ));
                        };
                        Ok(Catch {
                            binding: binding.id.sym.to_string(),
                            body: self.convert_statements(&handler.body.stmts)?,
                        })
                    })
                    .transpose()?,
                finally: stmt
                    .finalizer
                    .as_ref()
                    .map(|block| self.convert_statements(&block.stmts))
                    .transpose()?,
            },
            swc::Stmt::Decl(decl) => return self.convert_decl(decl),
            swc::Stmt::With(_) => {
                return Err(reject(
                    DiagnosticCode::WithUnsupported,
                    "with statements",
                    span,
                ));
            }
            swc::Stmt::Labeled(_) => {
                return Err(reject(DiagnosticCode::LabelUnsupported, "labels", span));
            }
            swc::Stmt::Switch(_) => {
                return Err(reject(
                    DiagnosticCode::SwitchUnsupported,
                    "switch statements",
                    span,
                ));
            }
            swc::Stmt::DoWhile(_) => {
                return Err(reject(
                    DiagnosticCode::DoWhileUnsupported,
                    "do/while statements",
                    span,
                ));
            }
            swc::Stmt::For(_) => {
                return Err(reject(
                    DiagnosticCode::ForUnsupported,
                    "classic for statements",
                    span,
                ));
            }
            swc::Stmt::ForIn(_) => {
                return Err(reject(
                    DiagnosticCode::ForInUnsupported,
                    "for/in statements",
                    span,
                ));
            }
            swc::Stmt::ForOf(_) => {
                return Err(reject(
                    DiagnosticCode::ForOfUnsupported,
                    "for/of statements",
                    span,
                ));
            }
            swc::Stmt::Debugger(_) => {
                return Err(reject(
                    DiagnosticCode::DebuggerUnsupported,
                    "debugger statements",
                    span,
                ));
            }
        })
    }

    fn convert_decl(&self, decl: &swc::Decl) -> Result<Stmt, Diagnostic> {
        let span = Some(source_span(decl.span()));
        match decl {
            swc::Decl::Class(class) if !class.class.decorators.is_empty() => Err(reject(
                DiagnosticCode::DecoratorUnsupported,
                "decorators",
                span,
            )),
            swc::Decl::Class(_) => Err(reject(DiagnosticCode::ClassUnsupported, "classes", span)),
            swc::Decl::TsEnum(_) => Err(reject(
                DiagnosticCode::EnumUnsupported,
                "TypeScript enums",
                span,
            )),
            swc::Decl::TsModule(_) => Err(reject(
                DiagnosticCode::NamespaceUnsupported,
                "TypeScript namespaces/modules",
                span,
            )),
            swc::Decl::Using(_) => Err(reject(
                DiagnosticCode::UsingUnsupported,
                "using declarations",
                span,
            )),
            swc::Decl::TsInterface(_) | swc::Decl::TsTypeAlias(_) => Ok(Stmt::Empty),
            swc::Decl::Var(decl) => {
                if decl.declare {
                    return Err(reject(
                        DiagnosticCode::DeclareUnsupported,
                        "ambient declare declarations",
                        span,
                    ));
                }
                let kind = match decl.kind {
                    swc::VarDeclKind::Let => VarKind::Let,
                    swc::VarDeclKind::Const => VarKind::Const,
                    swc::VarDeclKind::Var => {
                        return Err(reject(
                            DiagnosticCode::VarUnsupported,
                            "var declarations",
                            span,
                        ));
                    }
                };
                let declarations = decl
                    .decls
                    .iter()
                    .map(|decl| {
                        let swc::Pat::Ident(name) = &decl.name else {
                            return Err(reject(
                                DiagnosticCode::DestructuringUnsupported,
                                "destructuring declarations",
                                Some(source_span(decl.span)),
                            ));
                        };
                        Ok(Var {
                            name: name.id.sym.to_string(),
                            init: decl
                                .init
                                .as_deref()
                                .map(|expr| self.convert_expr(expr))
                                .transpose()?,
                        })
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Stmt::Var { kind, declarations })
            }
            swc::Decl::Fn(decl) => {
                if decl.declare {
                    return Err(reject(
                        DiagnosticCode::DeclareUnsupported,
                        "ambient declare declarations",
                        span,
                    ));
                }
                if !decl.function.decorators.is_empty() {
                    return Err(reject(
                        DiagnosticCode::DecoratorUnsupported,
                        "decorators",
                        span,
                    ));
                }
                let function =
                    self.convert_function(Some(decl.ident.sym.to_string()), &decl.function)?;
                Ok(Stmt::Function {
                    name: decl.ident.sym.to_string(),
                    function,
                })
            }
        }
    }

    fn convert_function(
        &self,
        name: Option<String>,
        function: &swc::Function,
    ) -> Result<Function, Diagnostic> {
        let span = Some(source_span(function.span));
        if function.is_generator {
            return Err(reject(
                DiagnosticCode::GeneratorUnsupported,
                "generators",
                span,
            ));
        }
        if function.is_async {
            return Err(reject(
                DiagnosticCode::AsyncUnsupported,
                "async functions",
                span,
            ));
        }
        if !function.decorators.is_empty() {
            return Err(reject(
                DiagnosticCode::DecoratorUnsupported,
                "decorators",
                span,
            ));
        }
        let params = function
            .params
            .iter()
            .map(|param| binding_name(&param.pat, param.span))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|param| param != "this")
            .collect();
        let body = function.body.as_ref().ok_or_else(|| {
            reject(
                DiagnosticCode::UnsupportedStatement,
                "function declarations without bodies",
                span,
            )
        })?;
        Ok(Function {
            name,
            params,
            body: FunctionBody::Block(self.convert_statements(&body.stmts)?),
        })
    }

    fn convert_expr(&self, expr: &swc::Expr) -> Result<Expr, Diagnostic> {
        self.with_expression_depth(Some(source_span(expr.span())), || {
            self.convert_expr_inner(expr)
        })
    }

    fn convert_expr_inner(&self, expr: &swc::Expr) -> Result<Expr, Diagnostic> {
        let span = Some(source_span(expr.span()));
        Ok(match expr {
            swc::Expr::Ident(ident) => Expr::Ident(ident.sym.to_string()),
            swc::Expr::Lit(lit) => match lit {
                swc::Lit::Null(_) => Expr::Null,
                swc::Lit::Bool(value) => Expr::Bool(value.value),
                swc::Lit::Num(value) => Expr::Number(value.value),
                swc::Lit::Str(value) => Expr::String(
                    value
                        .value
                        .as_str()
                        .ok_or_else(|| {
                            reject(
                                DiagnosticCode::LoneSurrogateLiteralUnsupported,
                                "string literals containing lone UTF-16 surrogates",
                                span,
                            )
                        })?
                        .to_string(),
                ),
                swc::Lit::Regex(_) => {
                    return Err(reject(
                        DiagnosticCode::RegExpUnsupported,
                        "regular-expression literals",
                        span,
                    ));
                }
                swc::Lit::BigInt(_) => {
                    return Err(reject(
                        DiagnosticCode::BigIntUnsupported,
                        "BigInt literals",
                        span,
                    ));
                }
                swc::Lit::JSXText(_) => {
                    return Err(reject(DiagnosticCode::JsxUnsupported, "JSX", span));
                }
            },
            swc::Expr::Array(array) => Expr::Array(
                array
                    .elems
                    .iter()
                    .map(|element| {
                        let Some(element) = element else {
                            return Ok(Expr::Undefined);
                        };
                        if element.spread.is_some() {
                            return Err(reject(
                                DiagnosticCode::SpreadUnsupported,
                                "array spread",
                                Some(source_span(element.span())),
                            ));
                        }
                        self.convert_expr(&element.expr)
                    })
                    .collect::<Result<_, _>>()?,
            ),
            swc::Expr::Object(object) => Expr::Object(
                object
                    .props
                    .iter()
                    .map(|property| self.convert_property(property))
                    .collect::<Result<_, _>>()?,
            ),
            swc::Expr::Fn(function) => Expr::Function(self.convert_function(
                function.ident.as_ref().map(|name| name.sym.to_string()),
                &function.function,
            )?),
            swc::Expr::Arrow(function) => {
                if function.is_generator {
                    return Err(reject(
                        DiagnosticCode::GeneratorUnsupported,
                        "generators",
                        span,
                    ));
                }
                if function.is_async {
                    return Err(reject(
                        DiagnosticCode::AsyncUnsupported,
                        "async functions",
                        span,
                    ));
                }
                let params = function
                    .params
                    .iter()
                    .map(|param| binding_name(param, param.span()))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .filter(|param| param != "this")
                    .collect();
                let body = match function.body.as_ref() {
                    swc::BlockStmtOrExpr::BlockStmt(block) => {
                        FunctionBody::Block(self.convert_statements(&block.stmts)?)
                    }
                    swc::BlockStmtOrExpr::Expr(expr) => {
                        FunctionBody::Expression(Box::new(self.convert_expr(expr)?))
                    }
                };
                Expr::Function(Function {
                    name: None,
                    params,
                    body,
                })
            }
            swc::Expr::Unary(expr) => {
                let op = match expr.op {
                    swc::UnaryOp::Plus => UnaryOp::Plus,
                    swc::UnaryOp::Minus => UnaryOp::Minus,
                    swc::UnaryOp::Bang => UnaryOp::Not,
                    swc::UnaryOp::TypeOf => UnaryOp::TypeOf,
                    swc::UnaryOp::Void => UnaryOp::Void,
                    swc::UnaryOp::Delete => {
                        return Err(reject(DiagnosticCode::DeleteUnsupported, "delete", span));
                    }
                    swc::UnaryOp::Tilde => {
                        return Err(reject(
                            DiagnosticCode::BitwiseUnsupported,
                            "bitwise operators",
                            span,
                        ));
                    }
                };
                Expr::Unary {
                    op,
                    value: Box::new(self.convert_expr(&expr.arg)?),
                }
            }
            swc::Expr::Bin(expr) => self.convert_binary(expr)?,
            swc::Expr::Assign(expr) => {
                if expr.op != swc::AssignOp::Assign {
                    return Err(reject(
                        DiagnosticCode::AssignmentOperatorUnsupported,
                        "compound assignment operators",
                        span,
                    ));
                }
                Expr::Assign {
                    target: self.convert_assign_target(&expr.left)?,
                    value: Box::new(self.convert_expr(&expr.right)?),
                }
            }
            swc::Expr::Member(member) => self.convert_member(member)?,
            swc::Expr::Cond(expr) => Expr::Conditional {
                test: Box::new(self.convert_expr(&expr.test)?),
                consequent: Box::new(self.convert_expr(&expr.cons)?),
                alternate: Box::new(self.convert_expr(&expr.alt)?),
            },
            swc::Expr::Call(call) => {
                let callee = match &call.callee {
                    swc::Callee::Expr(expr) => self.convert_expr(expr)?,
                    swc::Callee::Import(_) => {
                        return Err(reject(
                            DiagnosticCode::DynamicImportUnsupported,
                            "dynamic import",
                            span,
                        ));
                    }
                    swc::Callee::Super(_) => {
                        return Err(reject(
                            DiagnosticCode::SuperUnsupported,
                            "super calls",
                            span,
                        ));
                    }
                };
                if matches!(&callee, Expr::Ident(name) if name == "eval") {
                    return Err(reject(DiagnosticCode::EvalUnsupported, "eval", span));
                }
                if matches!(&callee, Expr::Ident(name) if name == "Function") {
                    return Err(reject(
                        DiagnosticCode::FunctionConstructorUnsupported,
                        "Function constructor",
                        span,
                    ));
                }
                let args = call
                    .args
                    .iter()
                    .map(|arg| {
                        if arg.spread.is_some() {
                            return Err(reject(
                                DiagnosticCode::SpreadUnsupported,
                                "call spread",
                                Some(source_span(arg.span())),
                            ));
                        }
                        self.convert_expr(&arg.expr)
                    })
                    .collect::<Result<_, _>>()?;
                Expr::Call {
                    callee: Box::new(callee),
                    args,
                }
            }
            swc::Expr::Tpl(template) => Expr::Template {
                quasis: template
                    .quasis
                    .iter()
                    .map(|quasi| quasi.raw.to_string())
                    .collect(),
                expressions: template
                    .exprs
                    .iter()
                    .map(|expr| self.convert_expr(expr))
                    .collect::<Result<_, _>>()?,
            },
            swc::Expr::Paren(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsTypeAssertion(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsConstAssertion(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsNonNull(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsAs(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsInstantiation(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::TsSatisfies(expr) => self.convert_expr(&expr.expr)?,
            swc::Expr::This(_) => {
                return Err(reject(DiagnosticCode::ThisUnsupported, "this", span));
            }
            swc::Expr::Update(_) => {
                return Err(reject(
                    DiagnosticCode::UpdateUnsupported,
                    "update operators",
                    span,
                ));
            }
            swc::Expr::New(_) => {
                return Err(reject(
                    DiagnosticCode::NewUnsupported,
                    "new expressions",
                    span,
                ));
            }
            swc::Expr::Seq(_) => {
                return Err(reject(
                    DiagnosticCode::SequenceUnsupported,
                    "sequence expressions",
                    span,
                ));
            }
            swc::Expr::TaggedTpl(_) => {
                return Err(reject(
                    DiagnosticCode::TaggedTemplateUnsupported,
                    "tagged templates",
                    span,
                ));
            }
            swc::Expr::Class(_) => {
                return Err(reject(DiagnosticCode::ClassUnsupported, "classes", span));
            }
            swc::Expr::Yield(_) => {
                return Err(reject(DiagnosticCode::YieldUnsupported, "yield", span));
            }
            swc::Expr::MetaProp(_) => {
                return Err(reject(
                    DiagnosticCode::MetaPropertyUnsupported,
                    "meta properties",
                    span,
                ));
            }
            swc::Expr::Await(_) => {
                return Err(reject(DiagnosticCode::AwaitUnsupported, "await", span));
            }
            swc::Expr::SuperProp(_) => {
                return Err(reject(
                    DiagnosticCode::SuperUnsupported,
                    "super properties",
                    span,
                ));
            }
            swc::Expr::OptChain(_) => {
                return Err(reject(
                    DiagnosticCode::OptionalChainingUnsupported,
                    "optional chaining",
                    span,
                ));
            }
            swc::Expr::PrivateName(_) => {
                return Err(reject(
                    DiagnosticCode::PrivateNameUnsupported,
                    "private names",
                    span,
                ));
            }
            swc::Expr::JSXMember(_)
            | swc::Expr::JSXNamespacedName(_)
            | swc::Expr::JSXEmpty(_)
            | swc::Expr::JSXElement(_)
            | swc::Expr::JSXFragment(_) => {
                return Err(reject(DiagnosticCode::JsxUnsupported, "JSX", span));
            }
            swc::Expr::Invalid(_) => {
                return Err(reject(
                    DiagnosticCode::UnsupportedExpression,
                    "invalid expression",
                    span,
                ));
            }
        })
    }

    fn convert_property(&self, property: &swc::PropOrSpread) -> Result<(String, Expr), Diagnostic> {
        let swc::PropOrSpread::Prop(property) = property else {
            return Err(reject(
                DiagnosticCode::SpreadUnsupported,
                "object spread",
                Some(source_span(property.span())),
            ));
        };
        match property.as_ref() {
            swc::Prop::Shorthand(name) => {
                Ok((name.sym.to_string(), Expr::Ident(name.sym.to_string())))
            }
            swc::Prop::KeyValue(property) => Ok((
                property_name(&property.key)?,
                self.convert_expr(&property.value)?,
            )),
            swc::Prop::Getter(_) | swc::Prop::Setter(_) => Err(reject(
                DiagnosticCode::AccessorUnsupported,
                "getters/setters",
                Some(source_span(property.span())),
            )),
            swc::Prop::Method(_) => Err(reject(
                DiagnosticCode::ObjectMethodUnsupported,
                "object methods",
                Some(source_span(property.span())),
            )),
            swc::Prop::Assign(_) => Err(reject(
                DiagnosticCode::UnsupportedExpression,
                "assignment properties",
                Some(source_span(property.span())),
            )),
        }
    }

    fn convert_binary(&self, expr: &swc::BinExpr) -> Result<Expr, Diagnostic> {
        use swc::BinaryOp as S;
        let left = Box::new(self.convert_expr(&expr.left)?);
        let right = Box::new(self.convert_expr(&expr.right)?);
        let span = Some(source_span(expr.span));
        Ok(match expr.op {
            S::LogicalAnd => Expr::Logical {
                left,
                op: LogicalOp::And,
                right,
            },
            S::LogicalOr => Expr::Logical {
                left,
                op: LogicalOp::Or,
                right,
            },
            S::NullishCoalescing => Expr::Logical {
                left,
                op: LogicalOp::Nullish,
                right,
            },
            op => {
                let op = match op {
                    S::Add => BinaryOp::Add,
                    S::Sub => BinaryOp::Subtract,
                    S::Mul => BinaryOp::Multiply,
                    S::Div => BinaryOp::Divide,
                    S::Mod => BinaryOp::Remainder,
                    S::EqEqEq => BinaryOp::StrictEqual,
                    S::NotEqEq => BinaryOp::StrictNotEqual,
                    S::EqEq => BinaryOp::LooseEqual,
                    S::NotEq => BinaryOp::LooseNotEqual,
                    S::Lt => BinaryOp::Less,
                    S::LtEq => BinaryOp::LessEqual,
                    S::Gt => BinaryOp::Greater,
                    S::GtEq => BinaryOp::GreaterEqual,
                    S::Exp => {
                        return Err(reject(
                            DiagnosticCode::ExponentiationUnsupported,
                            "exponentiation",
                            span,
                        ));
                    }
                    S::In => {
                        return Err(reject(
                            DiagnosticCode::InOperatorUnsupported,
                            "in operator",
                            span,
                        ));
                    }
                    S::InstanceOf => {
                        return Err(reject(
                            DiagnosticCode::InstanceOfUnsupported,
                            "instanceof",
                            span,
                        ));
                    }
                    _ => {
                        return Err(reject(
                            DiagnosticCode::BitwiseUnsupported,
                            "bitwise/shift operators",
                            span,
                        ));
                    }
                };
                Expr::Binary { left, op, right }
            }
        })
    }

    fn convert_member(&self, member: &swc::MemberExpr) -> Result<Expr, Diagnostic> {
        let object = self.convert_expr(&member.obj)?;
        if matches!(&object, Expr::Ident(name) if name == "prototype") {
            return Err(reject(
                DiagnosticCode::PrototypeMutationUnsupported,
                "prototype access",
                Some(source_span(member.span)),
            ));
        }
        let property = match &member.prop {
            swc::MemberProp::Ident(name) => {
                if name.sym == "prototype" {
                    return Err(reject(
                        DiagnosticCode::PrototypeMutationUnsupported,
                        "prototype access",
                        Some(source_span(member.span)),
                    ));
                }
                MemberProperty::Field(name.sym.to_string())
            }
            swc::MemberProp::Computed(property) => {
                MemberProperty::Index(Box::new(self.convert_expr(&property.expr)?))
            }
            swc::MemberProp::PrivateName(_) => {
                return Err(reject(
                    DiagnosticCode::PrivateNameUnsupported,
                    "private names",
                    Some(source_span(member.span)),
                ));
            }
        };
        Ok(Expr::Member {
            object: Box::new(object),
            property,
        })
    }

    fn convert_assign_target(
        &self,
        target: &swc::AssignTarget,
    ) -> Result<AssignTarget, Diagnostic> {
        match target {
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Ident(name)) => {
                Ok(AssignTarget::Ident(name.id.sym.to_string()))
            }
            swc::AssignTarget::Simple(swc::SimpleAssignTarget::Member(member)) => match self
                .convert_member(member)?
            {
                Expr::Member { object, property } => Ok(AssignTarget::Member { object, property }),
                _ => unreachable!(),
            },
            _ => Err(reject(
                DiagnosticCode::DestructuringUnsupported,
                "destructuring or wrapped assignment targets",
                Some(source_span(target.span())),
            )),
        }
    }
}

fn binding_name(pattern: &swc::Pat, span: Span) -> Result<String, Diagnostic> {
    match pattern {
        swc::Pat::Ident(name) => Ok(name.id.sym.to_string()),
        swc::Pat::Assign(_) => Err(reject(
            DiagnosticCode::ParameterDefaultUnsupported,
            "default parameters",
            Some(source_span(span)),
        )),
        swc::Pat::Rest(_) => Err(reject(
            DiagnosticCode::ParameterRestUnsupported,
            "rest parameters",
            Some(source_span(span)),
        )),
        _ => Err(reject(
            DiagnosticCode::DestructuringUnsupported,
            "destructuring parameters",
            Some(source_span(span)),
        )),
    }
}

fn property_name(name: &swc::PropName) -> Result<String, Diagnostic> {
    match name {
        swc::PropName::Ident(name) => Ok(name.sym.to_string()),
        swc::PropName::Str(name) => Ok(name.value.to_string_lossy().into_owned()),
        swc::PropName::Num(name) => Ok(name.value.to_string()),
        swc::PropName::Computed(name) => Err(reject(
            DiagnosticCode::ComputedPropertyUnsupported,
            "computed object keys",
            Some(source_span(name.span)),
        )),
        swc::PropName::BigInt(name) => Err(reject(
            DiagnosticCode::BigIntUnsupported,
            "BigInt object keys",
            Some(source_span(name.span)),
        )),
    }
}

fn parser_diagnostic(error: swc_ecma_parser::error::Error) -> Diagnostic {
    let message = error.kind().msg().to_string();
    let code = if message.contains("'with' statement") {
        DiagnosticCode::WithUnsupported
    } else {
        DiagnosticCode::SyntaxError
    };
    Diagnostic::new(code, message, Some(source_span(error.span())))
}

fn reject(code: DiagnosticCode, construct: &str, span: Option<SourceSpan>) -> Diagnostic {
    Diagnostic::new(
        code,
        format!("{construct} are not in the TypeScript dialect"),
        span,
    )
}

fn source_span(span: Span) -> SourceSpan {
    SourceSpan {
        start: span.lo.0 as usize,
        end: span.hi.0 as usize,
    }
}
