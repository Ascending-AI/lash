//! Module entries and awaiting durable process handles.

use std::collections::HashSet;

use lash_kernel_doc::{Expr, Literal};

use lash_vocabulary::{EffectName, Name, Param, RecordType, RecordTypeField, Signature, Type};

use lash_kernel_dialect::workflow::{self, ModuleRefusal};

use super::{BindingKind, Buf, FunctionFrame, Lowerer, Lowering, Operand, Ty};
use crate::adapter::{self as ast, TypeAnnotation, TypeShape};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// The effect that waits for a process's end.
const AWAIT_EFFECT: &str = "processes.await";

/// The field every handle record carries (`lash_protocol_types::handle`).
const HANDLE_FIELD: &str = "__handle__";

impl Lowerer<'_> {
    /// The async function a module's top-level `const` binds, if `init` is
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

    /// The signature a host starts `function` under: each parameter a
    /// plain name of a durable type. A module entry's annotations map under
    /// the workflow contract: a default is refused, and so is an annotation
    /// with no kernel type.
    pub(super) fn start_signature(&self, function: &ast::Function) -> Lowering<Signature> {
        let module = self.module.is_some();
        let unmappable = DiagnosticCode::Module(ModuleRefusal::UnmappableEntryAnnotation);
        let mut params = Vec::with_capacity(function.params.len());
        for param in &function.params {
            let (name, annotation) = match param {
                ast::Pattern::Ident(name, annotation) => (name, annotation),
                ast::Pattern::Assign { .. } if module => {
                    return Err(Diagnostic::new(
                        DiagnosticCode::Module(ModuleRefusal::EntryDefault),
                        "an entry's parameter has no default: a start passes every argument",
                        self.span,
                    ));
                }
                _ => {
                    return Err(Diagnostic::new(
                        unmappable,
                        "a process parameter is a plain name: start arguments are keyed by it",
                        self.span,
                    ));
                }
            };
            params.push(Param {
                name: Name::new(name.as_str()),
                ty: durable_type(annotation.as_ref(), unmappable)?,
                // `name?: T` is written as `T` or `undefined`.
                optional: annotation.as_ref().is_some_and(|annotation| {
                    matches!(&annotation.shape, TypeShape::Union(members)
                        if members.iter().any(|member| matches!(member.shape, TypeShape::Undefined)))
                }),
            });
        }
        let result = match function.return_ty.as_ref() {
            Some(annotation) => durable_type(Some(annotation), unmappable)?,
            None => Type::Any,
        };
        Ok(Signature { params, result })
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
        let signature = self.start_signature(function)?;
        let names: Vec<Name> = signature
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect();
        self.declared_captions
            .extend(function.captions.entries(&entry, &names));
        let (params, body, packed) = self.lift(function)?;
        let (body, notes) = body.into_parts();
        let definition = lash_kernel_dialect::authoring::local_function(
            params,
            body,
            &self.imports.as_map(),
            self.library,
            &self.local_functions,
        );
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
        let scopes = std::mem::take(&mut self.scopes);
        let frames = std::mem::replace(
            &mut self.functions,
            vec![
                FunctionFrame {
                    arrow: false,
                    async_body: false,
                    this: None,
                    args: None,
                    arguments_used: false,
                    context_used: false,
                    controls: Vec::new(),
                },
                FunctionFrame {
                    arrow: false,
                    async_body: false,
                    this: None,
                    args: None,
                    arguments_used: false,
                    context_used: false,
                    controls: Vec::new(),
                },
            ],
        );
        let narrowed = std::mem::take(&mut self.narrowed);
        let session = std::mem::replace(&mut self.session, &NO_SESSION);
        let span = self.span;
        self.lifting = Some(outer);
        let body = self.entry_body(function);
        self.lifting = None;
        self.span = span;
        self.session = session;
        self.narrowed = narrowed;
        self.functions = frames;
        self.scopes = scopes;
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
                    DiagnosticCode::Module(ModuleRefusal::CapturedModuleBinding),
                    format!(
                        "an entry runs apart from the module and cannot read the module's `{name}`"
                    ),
                    self.span,
                )
            })
    }
}

/// The value a reference to the declared function `entry` is.
fn reference(entry: lash_vocabulary::FunctionId) -> Operand {
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

/// The kernel type a durable annotation states. An absent annotation is
/// every value.
fn durable_type(annotation: Option<&TypeAnnotation>, refusal: DiagnosticCode) -> Lowering<Type> {
    let Some(annotation) = annotation else {
        return Ok(Type::Any);
    };
    Ok(match &annotation.shape {
        // A named type is checked where its definition is known: the host's.
        TypeShape::Unknown | TypeShape::Reference(_) => Type::Any,
        TypeShape::String => Type::Text,
        TypeShape::Number => Type::Number,
        TypeShape::Boolean => Type::Bool,
        TypeShape::Null => Type::Null,
        // A missing argument is absent; JSON spells it null.
        TypeShape::Undefined => Type::Union(vec![Type::Null, Type::Absent]),
        TypeShape::StringLiteral => Type::Text,
        TypeShape::Array(element) => Type::List(Box::new(durable_type(Some(element), refusal)?)),
        TypeShape::Object(fields) => Type::Record(RecordType {
            fields: fields
                .iter()
                .map(|field| {
                    Ok(RecordTypeField {
                        name: field.name.clone(),
                        ty: durable_type(Some(&field.ty), refusal)?,
                        optional: field.optional,
                    })
                })
                .collect::<Lowering<_>>()?,
            rest: None,
        }),
        TypeShape::Union(members) => {
            let mut types = Vec::new();
            for member in members {
                match durable_type(Some(member), refusal)? {
                    Type::Any => return Ok(Type::Any),
                    Type::Union(inner) => types.extend(inner),
                    other => types.push(other),
                }
            }
            types.dedup();
            match types.len() {
                0 => Type::Any,
                1 => types.remove(0),
                _ => Type::Union(types),
            }
        }
        TypeShape::Unsupported(_) => {
            return Err(Diagnostic::new(
                refusal,
                "this type is not one a process is started or ended with",
                Some(annotation.span),
            ));
        }
    })
}
