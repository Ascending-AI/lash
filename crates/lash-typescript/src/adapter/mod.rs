use std::cell::Cell;

use swc_common::{BytePos, Span, Spanned};
use swc_ecma_ast as swc;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax, lexer::Lexer};

use crate::{Diagnostic, DiagnosticCode, SourceSpan};

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

pub(crate) fn parse(source: &str) -> Result<Program, Diagnostic> {
    guard_source_nesting(source)?;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanMode {
    Code,
    SingleQuoted,
    DoubleQuoted,
    Template,
    LineComment,
    BlockComment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceDelimiter {
    Paren,
    Bracket,
    /// A brace in expression position: an object literal or an object type.
    Brace,
    /// A brace that opens a statement block, which ends the statement forms
    /// that introduced it.
    StatementBrace,
    TemplateExpression,
}

#[derive(Clone, Copy, Debug)]
struct SourceNestingFrame {
    delimiter: SourceDelimiter,
    outer_operators: usize,
}

/// The last significant token, which decides whether a `{` opens an object
/// literal or a statement block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviousToken {
    None,
    Byte(u8),
    /// A word that can only be followed by an expression (`return {…}`).
    ExpressionPrefixWord,
    OtherWord,
}

impl PreviousToken {
    fn opens_expression_brace(self) -> bool {
        match self {
            Self::ExpressionPrefixWord => true,
            Self::Byte(byte) => matches!(
                byte,
                b'=' | b'('
                    | b','
                    | b'['
                    | b':'
                    | b'?'
                    | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'%'
                    | b'<'
                    | b'>'
                    | b'!'
                    | b'~'
                    | b'&'
                    | b'|'
                    | b'^'
            ),
            Self::None | Self::OtherWord => false,
        }
    }
}

// Keep one cumulative lexical budget before SWC sees the source. Each open
// delimiter and each recursively nested operator/statement form consumes one
// unit; operator counts from outer delimiter frames remain active while the
// scanner visits an inner expression. A statement boundary — `;`, `,`, or the
// `}` that closes a statement block — releases the operator run it terminates,
// so a flat sequence of statement forms stays one level deep. The adapter
// repeats the guard with one shared counter for statement and expression
// conversion.
fn guard_source_nesting(source: &str) -> Result<(), Diagnostic> {
    let bytes = source.as_bytes();
    let mut mode = ScanMode::Code;
    let mut escaped = false;
    let mut frames = Vec::new();
    let mut current_operators = 0usize;
    let mut previous = PreviousToken::None;
    let mut index = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        match mode {
            ScanMode::Code => {
                // `None` leaves the previous significant token in place, so
                // whitespace and comments never change how a `{` is classified.
                let mut scanned = Some(PreviousToken::Byte(byte));
                match (byte, next) {
                    (b'/', Some(b'/')) => {
                        mode = ScanMode::LineComment;
                        index += 1;
                        scanned = None;
                    }
                    (b'/', Some(b'*')) => {
                        mode = ScanMode::BlockComment;
                        index += 1;
                        scanned = None;
                    }
                    (b'\'', _) => {
                        mode = ScanMode::SingleQuoted;
                        escaped = false;
                    }
                    (b'"', _) => {
                        mode = ScanMode::DoubleQuoted;
                        escaped = false;
                    }
                    (b'`', _) => {
                        mode = ScanMode::Template;
                        escaped = false;
                    }
                    (b'(', _) => enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Paren,
                        index,
                    )?,
                    (b')', _) => leave_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Paren,
                    ),
                    (b'[', _) => enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Bracket,
                        index,
                    )?,
                    (b']', _) => leave_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::Bracket,
                    ),
                    (b'{', _) => enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        if previous.opens_expression_brace() {
                            SourceDelimiter::Brace
                        } else {
                            SourceDelimiter::StatementBrace
                        },
                        index,
                    )?,
                    (b'}', _) => {
                        let closed =
                            frames
                                .last()
                                .map(|frame| frame.delimiter)
                                .filter(|delimiter| {
                                    matches!(
                                        delimiter,
                                        SourceDelimiter::Brace
                                            | SourceDelimiter::StatementBrace
                                            | SourceDelimiter::TemplateExpression
                                    )
                                });
                        if let Some(delimiter) = closed {
                            leave_source_delimiter(&mut frames, &mut current_operators, delimiter);
                        }
                        match closed {
                            // A statement block ends every statement form that
                            // introduced it, so the next statement starts over.
                            Some(SourceDelimiter::StatementBrace) => current_operators = 0,
                            Some(SourceDelimiter::TemplateExpression) => mode = ScanMode::Template,
                            _ => {}
                        }
                    }
                    (b';' | b',', _) => current_operators = 0,
                    _ if is_identifier_start(byte) => {
                        let start = index;
                        while bytes
                            .get(index + 1)
                            .is_some_and(|byte| is_identifier_continue(*byte))
                        {
                            index += 1;
                        }
                        let word = &source[start..=index];
                        if is_recursive_operator_word(word) {
                            increment_source_operators(&frames, &mut current_operators, start)?;
                        }
                        scanned = Some(if is_expression_prefix_word(word) {
                            PreviousToken::ExpressionPrefixWord
                        } else {
                            PreviousToken::OtherWord
                        });
                    }
                    _ if is_recursive_operator_start(byte) => {
                        increment_source_operators(&frames, &mut current_operators, index)?;
                        index += recursive_operator_extra_bytes(bytes, index);
                    }
                    _ if byte.is_ascii_whitespace() => scanned = None,
                    _ => {}
                }
                if let Some(token) = scanned {
                    previous = token;
                }
            }
            ScanMode::SingleQuoted => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'\'' {
                    mode = ScanMode::Code;
                    previous = PreviousToken::OtherWord;
                }
            }
            ScanMode::DoubleQuoted => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    mode = ScanMode::Code;
                    previous = PreviousToken::OtherWord;
                }
            }
            ScanMode::Template => {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'`' {
                    mode = ScanMode::Code;
                    previous = PreviousToken::OtherWord;
                } else if byte == b'$' && next == Some(b'{') {
                    index += 1;
                    enter_source_delimiter(
                        &mut frames,
                        &mut current_operators,
                        SourceDelimiter::TemplateExpression,
                        index,
                    )?;
                    mode = ScanMode::Code;
                    // A template hole is expression position, like `(`.
                    previous = PreviousToken::Byte(b'(');
                }
            }
            ScanMode::LineComment => {
                if matches!(byte, b'\n' | b'\r') {
                    mode = ScanMode::Code;
                }
            }
            ScanMode::BlockComment => {
                if byte == b'*' && next == Some(b'/') {
                    mode = ScanMode::Code;
                    index += 1;
                }
            }
        }
        index += 1;
    }
    Ok(())
}

fn enter_source_delimiter(
    frames: &mut Vec<SourceNestingFrame>,
    current_operators: &mut usize,
    delimiter: SourceDelimiter,
    index: usize,
) -> Result<(), Diagnostic> {
    let next_depth = source_nesting_depth(frames, *current_operators) + 1;
    if next_depth > MAX_SOURCE_NESTING_DEPTH {
        return Err(source_nesting_diagnostic(Some(SourceSpan {
            start: index,
            end: index + 1,
        })));
    }
    frames.push(SourceNestingFrame {
        delimiter,
        outer_operators: std::mem::take(current_operators),
    });
    Ok(())
}

fn leave_source_delimiter(
    frames: &mut Vec<SourceNestingFrame>,
    current_operators: &mut usize,
    delimiter: SourceDelimiter,
) {
    if frames
        .last()
        .is_some_and(|frame| frame.delimiter == delimiter)
    {
        let frame = frames.pop().expect("matching source nesting frame exists");
        *current_operators = frame.outer_operators;
    }
}

fn increment_source_operators(
    frames: &[SourceNestingFrame],
    current_operators: &mut usize,
    index: usize,
) -> Result<(), Diagnostic> {
    *current_operators += 1;
    if source_nesting_depth(frames, *current_operators) > MAX_SOURCE_NESTING_DEPTH {
        return Err(source_nesting_diagnostic(Some(SourceSpan {
            start: index,
            end: index + 1,
        })));
    }
    Ok(())
}

fn source_nesting_depth(frames: &[SourceNestingFrame], current_operators: usize) -> usize {
    frames.len()
        + current_operators
        + frames
            .iter()
            .map(|frame| frame.outer_operators)
            .sum::<usize>()
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'$')
}

fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

fn is_recursive_operator_word(word: &str) -> bool {
    matches!(
        word,
        "await"
            | "delete"
            | "do"
            | "for"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "typeof"
            | "void"
            | "while"
            | "with"
            | "yield"
    )
}

/// Words after which a `{` can only open an object literal, never a block.
fn is_expression_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "await"
            | "case"
            | "delete"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "typeof"
            | "void"
            | "yield"
    )
}

fn is_recursive_operator_start(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'~'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'|'
            | b'^'
            | b'&'
            | b'?'
            | b'='
            | b'.'
    )
}

fn recursive_operator_extra_bytes(bytes: &[u8], index: usize) -> usize {
    const COMPOUND_OPERATORS: &[&[u8]] = &[
        b">>>=", b"===", b"!==", b">>>", b"**=", b"<<=", b">>=", b"||=", b"&&=", b"??=", b"...",
        b"=>", b"++", b"--", b"+=", b"-=", b"*=", b"/=", b"%=", b"|=", b"^=", b"&=", b"==", b"!=",
        b"<=", b">=", b"<<", b">>", b"**", b"||", b"&&", b"??", b"?.",
    ];
    COMPOUND_OPERATORS
        .iter()
        .find(|operator| bytes[index..].starts_with(operator))
        .map_or(0, |operator| operator.len() - 1)
}

fn source_nesting_diagnostic(span: Option<SourceSpan>) -> Diagnostic {
    Diagnostic::new(
        DiagnosticCode::SourceNestingLimit,
        format!(
            "TypeScript source nesting exceeds the {MAX_SOURCE_NESTING_DEPTH}-level limit; flatten the source"
        ),
        span,
    )
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
