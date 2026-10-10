//! Process literals as entries of the document.
//!
//! Where the host offers the process effects, an `async` arrow bound by a
//! `const` of the cell's own code, or written as the `definition` of an object, is a
//! process: code a host starts, not a function the cell calls. It lowers to
//! a declared function, listed among the document's entries under the
//! signature its annotations state, and its value in the cell is a
//! reference to that function. The start effect takes the reference.
//!
//! A process runs in its own run, so its body sees none of the cell's
//! bindings: naming one is refused, and the value goes through the start's
//! arguments instead. The body is the arrow's own closure, called once by
//! the entry in the entry's task, so an `await` in it is a wait of the
//! process.

use std::collections::HashSet;

use lash_kernel_doc::{
    Action, Atom, Callee, EffectName, Expr, Literal, Name, Param, RecordType, RecordTypeField,
    Signature, Stmt, Type,
};

use super::{BindingKind, Buf, FunctionFrame, Lowerer, Lowering, Operand, Ty};
use crate::adapter::{self as ast, TypeAnnotation, TypeShape};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// The effects whose presence makes an `async` arrow a process.
const PROCESS_EFFECTS: &str = "processes.";

/// The effect that waits for a process's end.
const AWAIT_EFFECT: &str = "processes.await";

/// The field every handle record carries (`lash_sansio::handle`).
const HANDLE_FIELD: &str = "__handle__";

/// The object property a process is written inline as.
pub(super) const DEFINITION_PROPERTY: &str = "definition";

impl Lowerer<'_> {
    /// The arrow `expr` is, if it is one a process is written as and the
    /// host offers the process effects.
    pub(super) fn process_arrow<'e>(&self, expr: &'e ast::Expr) -> Option<&'e ast::Function> {
        let ast::Expr::Function(function) = expr else {
            return None;
        };
        (function.is_async
            && function.is_arrow
            && self
                .effects
                .keys()
                .any(|effect| effect.as_str().starts_with(PROCESS_EFFECTS)))
        .then_some(function)
    }

    /// Whether lowering stands in the cell's own code, outside every
    /// function and every process, where a `const` names a process.
    pub(super) fn in_cell_code(&self) -> bool {
        self.in_main() && self.lifting.is_none()
    }

    /// The signature a host starts `function` under: each parameter a
    /// plain name of a durable type.
    pub(super) fn process_signature(&self, function: &ast::Function) -> Lowering<Signature> {
        let mut params = Vec::with_capacity(function.params.len());
        for param in &function.params {
            let ast::Pattern::Ident(name, annotation) = param else {
                return Err(Diagnostic::new(
                    DiagnosticCode::ProcessParamTypeUnsupported,
                    "a process parameter is a plain name: start arguments are keyed by it",
                    self.span,
                ));
            };
            params.push(Param {
                name: Name::new(name.as_str()),
                ty: durable_type(
                    annotation.as_ref(),
                    DiagnosticCode::ProcessParamTypeUnsupported,
                )?,
                // `name?: T` is written as `T` or `undefined`.
                optional: annotation.as_ref().is_some_and(|annotation| {
                    matches!(&annotation.shape, TypeShape::Union(members)
                        if members.iter().any(|member| matches!(member.shape, TypeShape::Undefined)))
                }),
            });
        }
        let result = match function.return_ty.as_ref() {
            Some(annotation) => durable_type(
                Some(annotation),
                DiagnosticCode::ProcessReturnTypeUnsupported,
            )?,
            None => Type::Any,
        };
        Ok(Signature { params, result })
    }

    /// The saved function `expr` names, if it is one a host may start and
    /// the host offers the process effects.
    pub(super) fn saved_process(&self, expr: &ast::Expr) -> Option<Name> {
        let ast::Expr::Ident(name, _) = expr else {
            return None;
        };
        let bound = self
            .scopes
            .iter()
            .any(|scope| scope.bindings.contains_key(name));
        let name = Name::new(name.as_str());
        let startable = self
            .saved
            .get(&name)
            .and_then(|saved| saved.written.as_ref())
            .is_some_and(|written| written.start.is_some());
        (!bound
            && startable
            && self.lifting.is_none()
            && self
                .effects
                .keys()
                .any(|effect| effect.as_str().starts_with(PROCESS_EFFECTS)))
        .then_some(name)
    }

    /// Declares an entry that runs the saved function `name` under the
    /// signature it is started with, and gives the reference to it. The
    /// entry calls the function as a cell would, and waits for what it
    /// returns.
    pub(super) fn lower_saved_process(&mut self, name: &Name) -> Lowering<Operand> {
        let Some(Signature { params, result }) = self
            .saved
            .get(name)
            .and_then(|saved| saved.written.as_ref())
            .and_then(|written| written.start.clone())
        else {
            unreachable!("`saved_process` answered for a function a host may start");
        };
        let entry = self.fresh(&format!("{name}_process"));
        let args = self.fresh("args");
        let returned = self.fresh("returned");
        let value = self.fresh("value");
        let wait = self.function("ts.await")?;
        let same = self.function("same")?;
        let body = vec![
            Stmt::Let {
                name: args.clone(),
                value: lash_kernel_doc::Rhs::Expr(Expr::List(
                    params
                        .iter()
                        .map(|param| Expr::Variable(param.name.clone()))
                        .collect(),
                )),
            },
            Stmt::Let {
                name: returned.clone(),
                value: lash_kernel_doc::Rhs::Action(Action::Call {
                    callee: Callee::Declared(name.clone()),
                    args: vec![Atom::Literal(Literal::Absent), Atom::Variable(args)],
                }),
            },
            Stmt::Let {
                name: value.clone(),
                value: lash_kernel_doc::Rhs::Action(Action::Call {
                    callee: Callee::Library(wait),
                    args: vec![Atom::Variable(returned)],
                }),
            },
            // A process that returns nothing ends with null.
            Stmt::If {
                condition: Expr::Call {
                    function: same,
                    args: vec![
                        Expr::Variable(value.clone()),
                        Expr::Literal(Literal::Absent),
                    ],
                },
                then_block: vec![Stmt::Return {
                    value: Expr::Literal(Literal::Null),
                }],
                else_block: Vec::new(),
            },
            Stmt::Return {
                value: Expr::Variable(value),
            },
        ];
        self.saved_used.insert(name.clone());
        self.declared.insert(
            entry.clone(),
            lash_kernel_doc::Function {
                params: params.iter().map(|param| param.name.clone()).collect(),
                body,
            },
        );
        self.entries
            .insert(entry.clone(), Signature { params, result });
        Ok(Operand {
            atom: Atom::Literal(Literal::Function(entry)),
            ty: Ty::Unknown,
        })
    }

    /// Lifts the process `function` to an entry named `name`, or by a
    /// generated name, and gives the reference to it.
    pub(super) fn lower_process(
        &mut self,
        name: Option<&str>,
        function: &ast::Function,
    ) -> Lowering<Operand> {
        let entry = match name {
            Some(name)
                if !self.declared.contains_key(&Name::new(name))
                    && !self.saved.contains_key(&Name::new(name)) =>
            {
                Name::new(name)
            }
            _ => self.fresh("process"),
        };
        let Signature { params, result } = self.process_signature(function)?;

        // The body is lowered with none of the cell in scope.
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
                    this: None,
                    args: None,
                    arguments_used: false,
                    controls: Vec::new(),
                },
                FunctionFrame {
                    arrow: false,
                    this: None,
                    args: None,
                    arguments_used: false,
                    controls: Vec::new(),
                },
            ],
        );
        let narrowed = std::mem::take(&mut self.narrowed);
        let session = std::mem::replace(&mut self.session, &NO_SESSION);
        let span = self.span;
        self.lifting = Some(outer);
        let body = self.scoped_block(|lowerer| {
            let mut passed = Vec::with_capacity(params.len());
            for param in &params {
                let kernel = lowerer.declare(param.name.as_str(), BindingKind::Local);
                debug_assert_eq!(kernel, param.name, "an entry's parameter keeps its name");
                passed.push(Expr::Variable(kernel));
            }
            let run = lowerer.closure(function)?;
            let Atom::Variable(run) = run.atom else {
                unreachable!("a closure is bound to a temporary");
            };
            let args = lowerer.let_expr(Expr::List(passed), Ty::Unknown);
            let value = lowerer.emit_action(
                Action::Call {
                    callee: Callee::Value(run),
                    args: vec![Atom::Literal(Literal::Absent), args.atom],
                },
                Ty::Unknown,
            );
            // A process that returns nothing ends with null: `undefined`
            // is not data a process can end with.
            let nothing =
                lowerer.native("same", vec![value.expr(), Expr::Literal(Literal::Absent)])?;
            let ended = lowerer.block(|lowerer| {
                lowerer.emit(Stmt::Return {
                    value: Expr::Literal(Literal::Null),
                });
                Ok(())
            })?;
            lowerer.emit_if(nothing, ended, Buf::default());
            lowerer.emit(Stmt::Return {
                value: value.expr(),
            });
            Ok(())
        });
        self.lifting = None;
        self.span = span;
        self.session = session;
        self.narrowed = narrowed;
        self.functions = frames;
        self.scopes = scopes;
        let body = body?;

        self.declared.insert(
            entry.clone(),
            lash_kernel_doc::Function {
                params: params.iter().map(|param| param.name.clone()).collect(),
                body: body.stmts,
            },
        );
        self.entries
            .insert(entry.clone(), Signature { params, result });
        Ok(Operand {
            atom: Atom::Literal(Literal::Function(entry)),
            ty: Ty::Unknown,
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
        let offered =
            EffectName::new(AWAIT_EFFECT).is_ok_and(|effect| self.effects.contains_key(&effect));
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

    /// The refusal of a process body that names `name`, if `name` is the
    /// cell's.
    pub(super) fn captured(&self, name: &str) -> Option<Diagnostic> {
        self.lifting
            .as_ref()
            .is_some_and(|outer| outer.contains(name))
            .then(|| {
                Diagnostic::new(
                    DiagnosticCode::NonLiftableCapture,
                    format!(
                        "a process runs apart from the cell and cannot read the cell's `{name}`"
                    ),
                    self.span,
                )
            })
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
