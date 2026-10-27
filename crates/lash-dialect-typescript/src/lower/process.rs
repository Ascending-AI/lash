//! Module entries and awaiting durable process handles.

use std::collections::{BTreeMap, HashSet};

use lash_kernel_doc::{Expr, Literal};

use {
    lash_kernel_value::AnnotationOrigin, lash_kernel_value::EffectName, lash_kernel_value::Name,
    lash_kernel_value::Param, lash_kernel_value::Signature, lash_kernel_value::SignatureOrigin,
    lash_kernel_value::Type,
};

use lash_kernel_dialect::workflow::{self, ModuleRefusal};

use super::{BindingKind, Buf, FunctionFrame, Lowerer, Lowering, Operand, Ty};
use crate::adapter::{self as ast, TypeAnnotation, TypeShape};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// The effect that waits for a process's end.
const AWAIT_EFFECT: &str = "processes.await";

/// The field every handle record carries (`lash_protocol_types::handle`).
const HANDLE_FIELD: &str = "__handle__";

impl Lowerer<'_> {
    /// The function a module's top-level `const` binds, if `init` is
    /// one: an entry of the module. A declaration inside a block, a
    /// function or an entry is ordinary code.
    pub(super) fn module_entry<'e>(&self, init: &'e ast::Expr) -> Option<&'e ast::Function> {
        let ast::Expr::Function(function) = init else {
            return None;
        };
        (self.module.is_some() && self.in_cell_code() && self.scopes.len() == 1).then_some(function)
    }

    /// Whether lowering stands in the cell's own code, outside every
    /// function and every process, where a `const` names a process.
    pub(super) fn in_cell_code(&self) -> bool {
        self.in_main() && self.lifting.is_none()
    }

    /// One declaration supplies both callable presentation and module promotion.
    /// Ordinary source can retain unresolved annotations; only promotion refuses.
    pub(super) fn entry_declaration(
        &self,
        function: &ast::Function,
    ) -> (Signature, SignatureOrigin) {
        let mut params = Vec::new();
        let mut origins = Vec::new();
        let mut optional = false;
        for (index, pattern) in function.params.iter().enumerate() {
            // JavaScript permits omission after the first default, including
            // later positional patterns. Rest consumes zero or more arguments;
            // the args-list convention retains its variadic calling semantics.
            optional |= matches!(pattern, ast::Pattern::Assign { .. } | ast::Pattern::Rest(_));
            let pattern = match pattern {
                ast::Pattern::Assign { target, .. } => target.as_ref(),
                other => other,
            };
            let ast::Pattern::Ident(name, annotation) = pattern else {
                let span = function.param_spans.get(index).copied();
                // Retain this one authored public pattern and its position;
                // never flatten private destructuring bindings into parameters.
                let name = span
                    .and_then(|span| self.source.get(span.start..span.end))
                    .unwrap_or("");
                params.push(Param {
                    name: Name::new(ast::public_pattern(name)),
                    ty: Type::Any,
                    optional,
                });
                origins.push(AnnotationOrigin {
                    span,
                    refusal: Some(lash_kernel_value::AnnotationResolutionReason::Unsupported {
                        construct: "a destructured or rest public parameter".into(),
                    }),
                });
                continue;
            };
            let (ty, origin) = self.resolve_entry_annotation(
                annotation.as_ref(),
                function.param_spans.get(index).copied(),
                false,
            );
            optional |= annotation.as_ref().is_some_and(|annotation| {
                matches!(&annotation.shape, TypeShape::Union(members) if members.iter().any(|member| matches!(member.shape, TypeShape::Undefined)))
            });
            params.push(Param {
                name: Name::new(name),
                ty,
                optional,
            });
            origins.push(origin);
        }
        let (result, result_origin) = self.resolve_entry_annotation(
            function.return_ty.as_ref(),
            Some(function.result_span),
            true,
        );
        (
            Signature { params, result },
            SignatureOrigin {
                name: function.name.clone(),
                params: origins,
                result: result_origin,
            },
        )
    }

    fn resolve_entry_annotation(
        &self,
        annotation: Option<&TypeAnnotation>,
        missing: Option<SourceSpan>,
        settled: bool,
    ) -> (Type, AnnotationOrigin) {
        let Some(annotation) = annotation else {
            return (
                Type::Any,
                AnnotationOrigin {
                    span: missing,
                    refusal: None,
                },
            );
        };
        match self.facts.entry_annotation(annotation, settled) {
            Ok(ty) => (
                ty,
                AnnotationOrigin {
                    span: Some(annotation.span),
                    refusal: None,
                },
            ),
            Err((span, reason)) => (
                Type::Any,
                AnnotationOrigin {
                    span: Some(span),
                    refusal: Some(reason),
                },
            ),
        }
    }

    /// Lifts the module entry `function`, bound to `name` at the module's
    /// top level, into the module, and gives the reference to it.
    pub(super) fn lower_module_entry(
        &mut self,
        name: &str,
        function: &ast::Function,
    ) -> Lowering<Operand> {
        let entry = Name::new(name);
        let span = self.span;
        let (signature, origin) = self.entry_declaration(function);
        origin
            .require(
                &signature,
                lash_kernel_value::FunctionLocation {
                    name: Some(name.into()),
                    site: None,
                },
            )
            .map_err(Diagnostic::entry_signature)?;
        let names: Vec<Name> = signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        self.declared_captions
            .extend(function.captions.entries(&entry, &names));
        let (params, body, packed) = self.lift(function)?;
        let (body, notes) = body.into_parts();
        let mut definition = lash_kernel_dialect::authoring::local_function(
            params,
            body,
            &self.imports.as_map(),
            self.library,
            &self.local_functions,
        );
        if let Ok(authored) = lash_kernel_value::FunctionName::new(name) {
            definition.name = authored;
        }
        let identity = definition.identity();
        let convention = self.entry_convention(packed)?;
        // Only a module's lowering finds an entry (`module_entry`).
        if let Some(module) = self.module.as_mut() {
            module
                .add(workflow::Entry {
                    name: entry.clone(),
                    span,
                    function: definition,
                    asynchronous: function.is_async,
                    rest: function
                        .params
                        .last()
                        .is_some_and(|param| matches!(param, ast::Pattern::Rest(_))),
                    signature,
                    convention,
                    notes,
                })
                .map_err(Diagnostic::module)?;
        }
        Ok(reference(identity))
    }

    /// The author's entry body, lowered with none of the cell or module
    /// in scope.
    fn lift(&mut self, function: &ast::Function) -> Lowering<(Vec<Name>, Buf, bool)> {
        let outer: HashSet<String> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.keys().cloned())
            .chain(self.session.iter().map(|name| name.as_str().to_owned()))
            .collect();
        let templates = std::mem::take(&mut self.templates);
        let scopes = std::mem::take(&mut self.scopes);
        let frames = std::mem::replace(
            &mut self.functions,
            vec![
                FunctionFrame {
                    implicit_text_conversions: Vec::new(),
                    arrow: false,
                    async_body: false,
                    this: None,
                    args: None,
                    arguments_used: false,
                    context_used: false,
                    controls: Vec::new(),
                    labels: Vec::new(),
                },
                FunctionFrame {
                    implicit_text_conversions: Vec::new(),
                    arrow: false,
                    async_body: false,
                    this: None,
                    args: None,
                    arguments_used: false,
                    context_used: false,
                    controls: Vec::new(),
                    labels: Vec::new(),
                },
            ],
        );
        let narrowed = std::mem::take(&mut self.narrowed);
        let session = std::mem::replace(&mut self.session, &NO_SESSION);
        let span = self.span;
        self.lifting = Some(outer);
        // The private module scope is outside the parameter scope, so a
        // helper's module reads cannot accidentally resolve to an entry argument.
        let statements = self.module_statements;
        let mut needed = self.facts.module_reads(function);
        loop {
            let before = needed.len();
            for statement in statements {
                if let ast::Stmt::Var { declarations, .. } = statement.unspanned() {
                    for declaration in declarations {
                        if let (ast::Pattern::Ident(name, _), Some(init)) =
                            (&declaration.pattern, &declaration.init)
                            && needed.contains(name)
                        {
                            static_reads(init, &self.facts, &mut needed);
                        }
                    }
                }
            }
            if before == needed.len() {
                break;
            }
        }
        let mut module_values = BTreeMap::new();
        let mut dynamic = false;
        for statement in statements {
            match statement.unspanned() {
                ast::Stmt::Empty | ast::Stmt::TypeAlias { .. } => {}
                ast::Stmt::Var {
                    kind: ast::VarKind::Const,
                    declarations,
                } => {
                    for declaration in declarations {
                        if let (ast::Pattern::Ident(name, _), Some(init)) =
                            (&declaration.pattern, &declaration.init)
                            && static_value(init, &module_values)
                        {
                            module_values.insert(name.clone(), scalar_value(init, &module_values));
                        } else {
                            dynamic = true;
                        }
                    }
                }
                _ => dynamic = true,
            }
        }
        let mut known = BTreeMap::new();
        let mut static_declarations = Vec::new();
        for statement in statements {
            let span = match statement {
                ast::Stmt::Spanned(span, _) => Some(*span),
                _ => None,
            };
            if let ast::Stmt::Var {
                kind: ast::VarKind::Const,
                declarations,
            } = statement.unspanned()
            {
                for declaration in declarations {
                    if let (ast::Pattern::Ident(name, _), Some(init)) =
                        (&declaration.pattern, &declaration.init)
                        && static_value(init, &known)
                    {
                        known.insert(name.clone(), scalar_value(init, &known));
                        if needed.contains(name)
                            && (!dynamic || matches!(init, ast::Expr::Function(_)))
                        {
                            static_declarations.push((span, declaration));
                        }
                    }
                }
            }
        }
        let mut shape = None;
        let body = self
            .scoped_block(|this| {
                for (_, declaration) in &static_declarations {
                    if let ast::Pattern::Ident(name, _) = &declaration.pattern {
                        this.declare(name, BindingKind::Const);
                    }
                }
                for (span, declaration) in &static_declarations {
                    this.span = *span;
                    this.lower_declaration(ast::VarKind::Const, declaration)?;
                }
                let (params, body, packed) = this.entry_body(function)?;
                this.append(body);
                shape = Some((params, packed));
                Ok(())
            })
            .map(|body| {
                let Some((params, packed)) = shape else {
                    unreachable!("a lowered entry has its shape");
                };
                (params, body, packed)
            });
        self.lifting = None;
        self.span = span;
        self.session = session;
        self.narrowed = narrowed;
        self.functions = frames;
        self.scopes = scopes;
        self.templates = templates;
        body
    }

    pub(super) fn entry_convention(
        &mut self,
        packed: bool,
    ) -> Lowering<lash_kernel_doc::StartConvention> {
        if !packed {
            return Ok(lash_kernel_doc::StartConvention::TsDirect {
                await_fn: self.function_target("ts.await")?,
            });
        }
        Ok(lash_kernel_doc::StartConvention::TsArgsList {
            await_fn: self.function_target("ts.await")?,
            same_fn: lash_kernel_doc::FunctionTarget::Local(self.function("same")?),
        })
    }

    /// What `await` gives for `awaited`, the settled value: a process
    /// handle is awaited to the process's end, as `processes.await` waits
    /// for it; any other value is itself. Written in the dialect's own
    /// terms, so the test and the wait mean what the source would.
    pub(super) fn await_process(
        &mut self,
        awaited: Operand,
        span: SourceSpan,
    ) -> Lowering<Operand> {
        let offered = EffectName::new(AWAIT_EFFECT)
            .is_ok_and(|effect| self.namespaces.operation(&effect).is_some());
        if !offered || self.is_bound("processes") {
            return Ok(awaited);
        }
        self.push_scope();
        let name = self.fresh("awaited").to_string();
        let kernel = self.declare(&name, BindingKind::Local);
        self.bind(kernel, awaited);
        let held = || Box::new(ast::Expr::Ident(name.clone(), None));
        let both = |left, right| {
            Box::new(ast::Expr::Logical {
                left,
                op: ast::LogicalOp::And,
                right,
            })
        };
        let is_object = both(
            Box::new(ast::Expr::Binary {
                operand_spans: None,
                left: held(),
                op: ast::BinaryOp::StrictNotEqual,
                right: Box::new(ast::Expr::Null),
            }),
            Box::new(ast::Expr::Binary {
                operand_spans: None,
                left: Box::new(ast::Expr::Unary {
                    op: ast::UnaryOp::TypeOf,
                    value: held(),
                }),
                op: ast::BinaryOp::StrictEqual,
                right: Box::new(ast::Expr::String("object".to_owned())),
            }),
        );
        let is_handle = both(
            is_object,
            Box::new(ast::Expr::Binary {
                operand_spans: None,
                left: Box::new(ast::Expr::String(HANDLE_FIELD.to_owned())),
                op: ast::BinaryOp::In,
                right: held(),
            }),
        );
        let wait = ast::Expr::Await {
            value: Box::new(ast::Expr::Call {
                callee: Box::new(ast::Expr::Member {
                    object: Box::new(ast::Expr::Ident("processes".to_owned(), None)),
                    property: ast::MemberProperty::Field("await".to_owned()),
                    span,
                }),
                args: vec![ast::CallArg::Value(ast::Expr::Object(vec![
                    ast::ObjectProperty::KeyValue(
                        ast::PropertyKey::Static("handle".to_owned()),
                        *held(),
                    ),
                ]))],
                span,
            }),
            span,
        };
        let result = self.lower_expr(&ast::Expr::Conditional {
            test: is_handle,
            consequent: Box::new(wait),
            alternate: held(),
        });
        self.pop_scope();
        result
    }

    /// A module entry cannot name a binding of its surrounding module.
    pub(super) fn captured(&self, name: &str) -> Option<Diagnostic> {
        self.lifting
            .as_ref()
            .is_some_and(|outer| outer.contains(name))
            .then(|| {
                Diagnostic::new(
                    DiagnosticCode::Module(ModuleRefusal::DynamicModuleInitialization),
                    format!("module binding `{name}` needs dynamic initialization"),
                    self.span,
                )
            })
    }
}

/// The value a reference to the declared function `entry` is.
fn reference(entry: lash_kernel_value::FunctionId) -> Operand {
    Operand {
        plain: None,
        text_prefix: None,
        expr: Expr::Literal(Literal::Function(entry)),
        stable: true,
        ty: Ty::Unknown,
    }
}

/// No session binding: what a process body sees of the session.
static NO_SESSION: std::collections::BTreeSet<Name> = std::collections::BTreeSet::new();

/// Only source-owned data construction and function declarations materialize.
/// Calls, awaits, member reads and writes must execute inside an entry.
fn scalar_value(value: &ast::Expr, known: &BTreeMap<String, bool>) -> bool {
    match value {
        ast::Expr::Null | ast::Expr::Bool(_) | ast::Expr::Number(_) | ast::Expr::String(_) => true,
        ast::Expr::Ident(name, _) => known.get(name).copied().unwrap_or(false),
        ast::Expr::As { value, .. } | ast::Expr::Unary { value, .. } => scalar_value(value, known),
        ast::Expr::Binary { left, right, .. } | ast::Expr::Logical { left, right, .. } => {
            scalar_value(left, known) && scalar_value(right, known)
        }
        ast::Expr::Conditional {
            test,
            consequent,
            alternate,
        } => [test, consequent, alternate]
            .iter()
            .all(|value| scalar_value(value, known)),
        _ => false,
    }
}

fn static_value(value: &ast::Expr, known: &BTreeMap<String, bool>) -> bool {
    if scalar_value(value, known) {
        return true;
    }
    match value {
        ast::Expr::Function(_) => true,
        ast::Expr::Ident(name, _) => known.contains_key(name),
        ast::Expr::As { value, .. } => static_value(value, known),
        ast::Expr::Array(items) => items.iter().all(|item| match item {
            ast::ArrayElement::Value(value) => static_value(value, known),
            ast::ArrayElement::Hole => true,
            ast::ArrayElement::Spread(_) => false,
        }),
        ast::Expr::Object(items) => items.iter().all(|item| matches!(item, ast::ObjectProperty::KeyValue(ast::PropertyKey::Static(_), value) if static_value(value, known))),
        _ => false,
    }
}

fn static_reads(
    value: &ast::Expr,
    facts: &crate::types::Facts,
    names: &mut std::collections::BTreeSet<String>,
) {
    match value {
        ast::Expr::Ident(name, _) => {
            names.insert(name.clone());
        }
        ast::Expr::Function(function) => names.extend(facts.module_reads(function)),
        ast::Expr::As { value, .. } | ast::Expr::Unary { value, .. } => {
            static_reads(value, facts, names)
        }
        ast::Expr::Binary { left, right, .. } | ast::Expr::Logical { left, right, .. } => {
            static_reads(left, facts, names);
            static_reads(right, facts, names);
        }
        ast::Expr::Conditional {
            test,
            consequent,
            alternate,
        } => {
            for value in [test, consequent, alternate] {
                static_reads(value, facts, names);
            }
        }
        ast::Expr::Array(items) => {
            for item in items {
                if let ast::ArrayElement::Value(value) = item {
                    static_reads(value, facts, names);
                }
            }
        }
        ast::Expr::Object(items) => {
            for item in items {
                if let ast::ObjectProperty::KeyValue(ast::PropertyKey::Static(_), value) = item {
                    static_reads(value, facts, names);
                }
            }
        }
        _ => {}
    }
}
