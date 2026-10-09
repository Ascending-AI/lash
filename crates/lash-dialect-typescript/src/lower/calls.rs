//! Members, calls and the globals a built-in row answers for.

use lash_kernel_doc::{Action, Atom, Callee, Expr, Literal, Place, Stmt};

use super::{Buf, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// A property key, already evaluated.
#[derive(Clone, Debug)]
pub(super) enum Key {
    /// A name the source spells: `object.name`.
    Static(String),
    /// `object[key]`.
    Computed(Operand),
}

impl Key {
    pub(super) fn operand(&self) -> Operand {
        match self {
            Self::Static(name) => Operand::text(name.clone()),
            Self::Computed(key) => key.clone(),
        }
    }
}

/// What an optional chain holds between two of its links.
enum Link {
    Value(Operand),
    /// A member not yet read: a call that follows it is a method call on
    /// the object.
    Member {
        object: Operand,
        key: Key,
    },
}

impl Lowerer<'_> {
    /// A member's key, held so that later code cannot change it.
    pub(super) fn lower_key(&mut self, property: &ast::MemberProperty) -> Lowering<Key> {
        Ok(match property {
            ast::MemberProperty::Field(name) => Key::Static(name.clone()),
            ast::MemberProperty::Index(index) => {
                let index = self.lower_expr(index)?;
                if let Atom::Literal(Literal::Text(name)) = &index.atom {
                    Key::Static(name.clone())
                } else {
                    Key::Computed(self.pin(index))
                }
            }
        })
    }

    /// A read-modify-write reference converts a guest key only once.
    pub(super) fn reference_key(&mut self, object: &Operand, key: Key) -> Lowering<Key> {
        match key {
            Key::Computed(value) if !value.ty.is_number() && value.ty != Ty::Text => {
                let checked = self.invoke(
                    "ts.require_object_coercible",
                    std::slice::from_ref(object),
                    Ty::Unknown,
                )?;
                self.discard(checked);
                let key = self.invoke("ts.to_property_key", &[value], Ty::Text)?;
                Ok(Key::Computed(key))
            }
            key => Ok(key),
        }
    }

    /// `object.key` or `object[key]`.
    ///
    /// An element of an array read by a number, and an array's length, are
    /// the kernel's own `list.get` and `list.len`. Every other read keeps
    /// JavaScript's meaning, and what it gives is believed to be what the
    /// object's type says.
    pub(super) fn get_member(&mut self, object: &Operand, key: &Key) -> Lowering<Operand> {
        if let Ty::List(element) = &object.ty {
            match key {
                Key::Computed(index) if index.ty.is_number() => {
                    let read = self.native("list.get", vec![object.expr(), index.expr()])?;
                    let read = self.let_expr(read, (**element).clone());
                    return self.invoke("ts.hole_value", &[read], (**element).clone());
                }
                Key::Static(name) if name == "length" => {
                    let length = self.native("list.len", vec![object.expr()])?;
                    return Ok(self.let_expr(length, Ty::Number));
                }
                _ => {}
            }
        }
        let ty = match key {
            Key::Static(name) => object.ty.property(name),
            Key::Computed(_) => Ty::Unknown,
        };
        if let Key::Static(name) = key
            && self.table.properties.contains_key(name.as_str())
        {
            return self.invoke(
                &format!("ts.property.{name}"),
                std::slice::from_ref(object),
                ty,
            );
        }
        if let Key::Static(name) = key
            && self.table.methods.contains_key(name.as_str())
        {
            return self.invoke(
                &format!("ts.member.{name}"),
                std::slice::from_ref(object),
                Ty::Unknown,
            );
        }
        let function = if matches!(key, Key::Computed(_)) {
            "ts.get_computed"
        } else {
            "ts.get"
        };
        self.invoke(function, &[object.clone(), key.operand()], ty)
    }

    /// `object[key] = value`.
    pub(super) fn set_member(
        &mut self,
        object: &Operand,
        key: &Key,
        value: Operand,
    ) -> Lowering<()> {
        let written = self.invoke(
            "ts.set",
            &[object.clone(), key.operand(), value],
            Ty::Unknown,
        )?;
        self.discard(written);
        Ok(())
    }

    /// The dotted path of an expression rooted at a name the source does
    /// not bind: `Math.max`, `console.log`.
    fn global_path(&self, expr: &ast::Expr) -> Option<String> {
        match expr {
            ast::Expr::Ident(name, _) => {
                (!self.is_bound(name) && crate::builtins::is_global(name)).then(|| name.clone())
            }
            ast::Expr::Member {
                object,
                property: ast::MemberProperty::Field(field),
                ..
            } => Some(format!("{}.{field}", self.global_path(object)?)),
            _ => None,
        }
    }

    /// A read of a global path: its value row, a function row as a closure,
    /// or `globalThis.name` as the name itself. `None` when no row knows
    /// the path's root.
    pub(super) fn read_global(
        &mut self,
        path: &str,
        span: Option<SourceSpan>,
    ) -> Option<Lowering<Operand>> {
        if let Some(name) = path.strip_prefix("globalThis.")
            && !name.contains('.')
        {
            return Some(self.read(name, span));
        }
        if let Some(function) = self.table.values.get(path).copied() {
            return Some(self.invoke(function, &[], Ty::Unknown));
        }
        if let Some(function) = self.table.functions.get(path).copied() {
            return Some(self.builtin_closure(function));
        }
        let root = path.split('.').next().unwrap_or(path);
        crate::builtins::is_global(root).then(|| {
            Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                format!("Unsupported: `{path}` as a value. Call one of its functions or read one of its constants."),
                span,
            ))
        })
    }

    pub(super) fn lower_member(
        &mut self,
        whole: &ast::Expr,
        object: &ast::Expr,
        property: &ast::MemberProperty,
        span: SourceSpan,
    ) -> Lowering<Operand> {
        if let Some(path) = self.global_path(whole)
            && let Some(result) = self.read_global(&path, Some(span))
        {
            return result;
        }
        let object = self.lower_expr(object)?;
        let object = match property {
            ast::MemberProperty::Index(index) if !super::walk::is_inert(index) => self.pin(object),
            _ => object,
        };
        let key = self.lower_key(property)?;
        self.get_member(&object, &key)
    }

    /// A call's arguments as one new list.
    pub(super) fn arguments(&mut self, args: &[ast::CallArg]) -> Lowering<Operand> {
        let items: Vec<(bool, &ast::Expr)> = args
            .iter()
            .map(|arg| match arg {
                ast::CallArg::Spread(value) => (true, value),
                ast::CallArg::Value(value) => (false, value),
            })
            .collect();
        self.list(&items)
    }

    fn args_are_inert(args: &[ast::CallArg]) -> bool {
        args.iter().all(|arg| match arg {
            ast::CallArg::Value(value) => super::walk::is_inert(value),
            ast::CallArg::Spread(_) => false,
        })
    }

    /// Calls a function value: `apply f(this, args)`.
    fn apply(&mut self, function: Operand, this: Operand, args: Operand) -> Operand {
        // A call gives what the function's declared return type says.
        let returned = function.ty.returned();
        let function = match function.atom {
            Atom::Variable(name) => name,
            Atom::Literal(_) => {
                // Calling a literal raises the kernel's `type_error`, which
                // is what the language asks for.
                let held = self.let_expr(function.expr(), function.ty);
                super::statements::variable_of(&held)
            }
        };
        self.emit_action(
            Action::Call {
                callee: Callee::Value(function),
                args: vec![this.atom, args.atom],
            },
            returned,
        )
    }

    /// `object.key(args)`: the name's dispatcher when built-in rows know
    /// the name, else the object's own function.
    fn call_member(&mut self, object: Operand, key: &Key, args: Operand) -> Lowering<Operand> {
        if let Key::Static(name) = key
            && self.table.methods.contains_key(name.as_str())
        {
            return self.invoke(&format!("ts.method.{name}"), &[object, args], Ty::Unknown);
        }
        self.invoke(
            if matches!(key, Key::Computed(_)) {
                "ts.call_computed"
            } else {
                "ts.call_member"
            },
            &[object, key.operand(), args],
            Ty::Unknown,
        )
    }

    pub(super) fn lower_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<Operand> {
        if let ast::Expr::Member {
            property: ast::MemberProperty::Field(name),
            ..
        } = callee
            && name == "toLocaleString"
        {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: toLocaleString/Intl formatting is locale-dependent. Build the deterministic string explicitly.",
                Some(span),
            ));
        }
        if let Some(wait) = self.wait_of(callee) {
            return self.lower_wait_call(wait, args, span);
        }
        if let Some(path) = self.global_path(callee) {
            // The dialect admits reads of the existing built-in prototype
            // methods. Calling their inherited call/apply needs no prototype
            // object: preserve the helper's receiver and argument convention.
            if let Some((base, method)) = path.rsplit_once('.')
                && matches!(method, "call" | "apply")
                && let Some(function) = self.table.functions.get(base).copied()
            {
                let passed = self.arguments(args)?;
                let padded = self.invoke(
                    "ts.pad",
                    &[passed.clone(), Operand::number(2.0)],
                    Ty::Unknown,
                )?;
                let receiver = self.let_expr(Self::element(&padded, 0), Ty::Unknown);
                let arguments = if method == "call" {
                    self.invoke("ts.rest", &[passed, Operand::number(1.0)], Ty::Unknown)?
                } else {
                    let arguments = self.let_expr(Self::element(&padded, 1), Ty::Unknown);
                    self.invoke("ts.string.apply_arguments", &[arguments], Ty::Unknown)?
                };
                return self.invoke(function, &[receiver, arguments], Ty::Unknown);
            }
            if let Some(function) = self.table.functions.get(path.as_str()).copied() {
                let args = self.arguments(args)?;
                let receiver = if path.starts_with("String.prototype.") {
                    Operand::text("")
                } else {
                    Operand::undefined()
                };
                return self.invoke(function, &[receiver, args], Ty::Unknown);
            }
            if !path.starts_with("globalThis.") && !self.table.values.contains_key(path.as_str()) {
                return Err(Diagnostic::refusal(
                    DiagnosticCode::MethodUnsupported,
                    format!("Unsupported: `{path}` is not a function the TypeScript dialect has."),
                    Some(span),
                ));
            }
        }
        match callee {
            ast::Expr::Ident(name, _) if name == "finish" && !self.is_bound(name) => {
                let value = match args {
                    [] => Operand::undefined(),
                    [ast::CallArg::Value(value)] => self.lower_expr(value)?,
                    _ => {
                        return Err(Diagnostic::defect(
                            DiagnosticCode::UnsupportedExpression,
                            "`finish` takes one value",
                            Some(span),
                        ));
                    }
                };
                let value = self.invoke("ts.boundary", &[value], Ty::Unknown)?;
                self.emit(Stmt::Finish {
                    value: value.expr(),
                });
                Ok(Operand::undefined())
            }
            ast::Expr::Member {
                object, property, ..
            } => {
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                let args = self.arguments(args)?;
                self.call_member(object, &key, args)
            }
            _ => {
                let function = self.lower_expr(callee)?;
                let function = if Self::args_are_inert(args) {
                    function
                } else {
                    self.pin(function)
                };
                let args = self.arguments(args)?;
                Ok(self.apply(function, Operand::undefined(), args))
            }
        }
    }

    pub(super) fn lower_new(
        &mut self,
        constructor: &str,
        args: &[ast::CallArg],
    ) -> Lowering<Operand> {
        let function = (!self.is_bound(constructor))
            .then(|| self.table.constructors.get(constructor).copied())
            .flatten();
        let Some(function) = function else {
            return Err(Diagnostic::new(
                DiagnosticCode::NewUnsupported,
                format!("`new {constructor}` is not in the TypeScript dialect"),
                self.span,
            ));
        };
        let args = self.arguments(args)?;
        self.invoke(function, &[Operand::undefined(), args], Ty::Unknown)
    }

    /// `base?.a.b?.(x)`: each `?.` ends the whole chain with `undefined`
    /// when what stands before it is `null` or `undefined`.
    pub(super) fn lower_optional_chain(
        &mut self,
        base: &ast::Expr,
        operations: &[ast::OptionalOperation],
    ) -> Lowering<Operand> {
        let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let place = Place::Variable(super::statements::variable_of(&result));
        let link = match base {
            ast::Expr::Member {
                object, property, ..
            } if self.global_path(base).is_none() => {
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                Link::Member { object, key }
            }
            _ => {
                let value = self.lower_expr(base)?;
                Link::Value(self.pin(value))
            }
        };
        self.chain(link, operations, &place)?;
        Ok(result)
    }

    fn chain(
        &mut self,
        link: Link,
        operations: &[ast::OptionalOperation],
        place: &Place,
    ) -> Lowering<()> {
        let Some((operation, rest)) = operations.split_first() else {
            let value = match link {
                Link::Value(value) => value,
                Link::Member { object, key } => self.get_member(&object, &key)?,
            };
            self.store(place.clone(), value);
            return Ok(());
        };
        match operation {
            ast::OptionalOperation::Member { property, optional } => {
                let object = match link {
                    Link::Value(value) => value,
                    Link::Member { object, key } => self.get_member(&object, &key)?,
                };
                self.unless_nullish(*optional, &object, |this| {
                    let key = this.lower_key(property)?;
                    this.chain(
                        Link::Member {
                            object: object.clone(),
                            key,
                        },
                        rest,
                        place,
                    )
                })
            }
            ast::OptionalOperation::Call { args, optional } => match link {
                Link::Member { object, key } if !optional => {
                    let args = self.arguments(args)?;
                    let value = self.call_member(object, &key, args)?;
                    self.chain(Link::Value(value), rest, place)
                }
                link => {
                    let (function, this) = match link {
                        Link::Value(value) => (value, Operand::undefined()),
                        Link::Member { object, key } => (self.get_member(&object, &key)?, object),
                    };
                    self.unless_nullish(*optional, &function, |lowerer| {
                        let args = lowerer.arguments(args)?;
                        let value = lowerer.apply(function.clone(), this, args);
                        lowerer.chain(Link::Value(value), rest, place)
                    })
                }
            },
        }
    }

    /// Runs `lower` in place, or, for an optional link, only when `value`
    /// is neither `null` nor `undefined`.
    fn unless_nullish(
        &mut self,
        optional: bool,
        value: &Operand,
        lower: impl FnOnce(&mut Self) -> Lowering<()>,
    ) -> Lowering<()> {
        if !optional {
            return lower(self);
        }
        let nullish = self.invoke("ts.is_nullish", std::slice::from_ref(value), Ty::Bool)?;
        let onward = self.block(lower)?;
        self.emit_if(nullish.expr(), Buf::default(), onward);
        Ok(())
    }
}
