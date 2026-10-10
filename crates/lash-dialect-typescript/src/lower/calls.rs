//! Members, calls and the globals a built-in row answers for.

use lash_kernel_doc::{Action, Atom, Callee, Expr, Literal, Member, Place, Stmt};

use super::{Buf, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::builtins::Receiver;
use crate::{Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};

/// The methods every built-in function or object inherits, which a call of
/// a built-in path may name although no row has the whole path.
const INHERITED: &[&str] = &[
    "call",
    "apply",
    "bind",
    "toString",
    "hasOwnProperty",
    "valueOf",
    "propertyIsEnumerable",
    "isPrototypeOf",
    "toLocaleString",
];

/// The built-in kind every value of `ty` is, with its native kind check.
/// Arrays use the dialect predicate because they include branded records.
fn declared_receiver(ty: &Ty) -> Option<(Receiver, Option<&'static str>)> {
    match ty {
        Ty::List(_) | Ty::Array => Some((Receiver::List, None)),
        Ty::Text => Some((Receiver::Text, Some("text.len"))),
        Ty::Float | Ty::Number => Some((Receiver::Number, Some("num.to_float"))),
        Ty::Bool => Some((Receiver::Bool, Some("bool.not"))),
        _ => None,
    }
}

/// A member read in place, and what must hold for it to read what the
/// helper would.
struct Direct {
    tests: Vec<Expr>,
    read: Expr,
    /// Whether the read may find a list's hole, which reads `undefined`.
    hole: bool,
    /// What the read gives where no test is needed.
    ty: Ty,
}

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
    Value {
        value: Operand,
        receiver: Operand,
    },
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
    /// An element of a kernel list read by a number, and its length, are
    /// the kernel's own `list.get` and `list.len`. Branded arrays and other reads keep
    /// JavaScript's meaning, and what it gives is believed to be what the
    /// object's type says.
    ///
    /// Any other object's member is read in place where the value turns out
    /// to be what the read is plainly answered by: a plain record's own
    /// field, a list's length, and a list's element at an index it has.
    /// Every other value, and a field a plain record lacks but a built-in
    /// might answer for, takes the helper (`direct_read`).
    pub(super) fn get_member(&mut self, object: &Operand, key: &Key) -> Lowering<Operand> {
        if let Ty::List(element) = &object.ty {
            match key {
                Key::Computed(index) if index.ty.is_number() => {
                    return self.array_branch(
                        object,
                        |this| {
                            let read =
                                this.native("list.get", vec![object.expr(), index.expr()])?;
                            let read = this.let_expr(read, (**element).clone());
                            this.absent_if_hole(&super::statements::variable_of(&read), &read)?;
                            Ok(read)
                        },
                        |this| {
                            this.invoke(
                                "ts.get_computed",
                                &[object.clone(), index.clone()],
                                (**element).clone(),
                            )
                        },
                    );
                }
                Key::Static(name) if name == "length" => {
                    return self.array_branch(
                        object,
                        |this| {
                            let length = this.native("list.len", vec![object.expr()])?;
                            Ok(this.let_expr(length, Ty::Number))
                        },
                        |this| this.invoke("ts.get", &[object.clone(), key.operand()], Ty::Number),
                    );
                }
                _ => {}
            }
        }
        let ty = match key {
            Key::Static(name) => object.ty.property(name),
            Key::Computed(_) => Ty::Unknown,
        };
        if let Some(direct) = self.direct_read(object, key)? {
            if direct.tests.is_empty() && !direct.hole {
                return Ok(self.let_expr(direct.read, direct.ty));
            }
            let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
            let place = Place::Variable(super::statements::variable_of(&result));
            let read = direct.read;
            self.guard(
                &direct.tests,
                &mut |this| {
                    this.emit(Stmt::Assign {
                        place: place.clone(),
                        value: lash_kernel_doc::Rhs::Expr(read.clone()),
                    });
                    if direct.hole {
                        let hole = this.same(result.expr(), Expr::Tuple(Vec::new()))?;
                        let absent = this.block(|this| {
                            this.emit(Stmt::Assign {
                                place: place.clone(),
                                value: lash_kernel_doc::Rhs::Expr(Expr::Literal(Literal::Absent)),
                            });
                            Ok(())
                        })?;
                        this.emit_if(hole, absent, Buf::default());
                    }
                    Ok(())
                },
                &mut |this| {
                    let value = this.read_member(object, key, Ty::Unknown)?;
                    this.store(place.clone(), value);
                    Ok(())
                },
            )?;
            let ty = if direct.tests.is_empty() {
                direct.ty
            } else {
                ty
            };
            return Ok(Operand { ty, ..result });
        }
        self.read_member(object, key, ty)
    }

    /// The helper that reads `object.key` with JavaScript's meaning.
    fn read_member(&mut self, object: &Operand, key: &Key, ty: Ty) -> Lowering<Operand> {
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
            && self.table.member_names.contains(name.as_str())
        {
            return self.invoke(
                &format!("ts.member.{name}"),
                std::slice::from_ref(object),
                ty,
            );
        }
        let function = if matches!(key, Key::Computed(_)) {
            "ts.get_computed"
        } else {
            "ts.read"
        };
        self.invoke(function, &[object.clone(), key.operand()], ty)
    }

    /// The in-place form of a read of `object.key`, and the tests under
    /// which it gives what the helper would: `None` where no value of the
    /// object's type has one.
    ///
    /// A plain record, a record whose `brand` is no text, answers a name no
    /// built-in has with its field by that name, `undefined` when it has
    /// none (`ts.read`); a name some built-in has, or a computed text, only
    /// when the record holds a field by it (`ts.member.<name>`,
    /// `ts.property.<name>`, `ts.get_computed`). A
    /// list answers `length` with its length, and a number that is one of
    /// its positions with its element there, a hole being `undefined`
    /// (`ts.get`). Each test is a kernel expression that cannot raise once
    /// the tests before it held.
    fn direct_read(&mut self, object: &Operand, key: &Key) -> Lowering<Option<Direct>> {
        match key {
            Key::Static(name)
                if name == "length" && !matches!(object.ty, Ty::Object | Ty::Record(_)) =>
            {
                let Some(tests) = self.list_tests(object)? else {
                    return Ok(None);
                };
                let length = self.native("list.len", vec![object.expr()])?;
                let read = self.native("num.to_float", vec![length])?;
                Ok(Some(Direct {
                    tests,
                    read,
                    hole: false,
                    ty: Ty::Float,
                }))
            }
            Key::Static(name) => {
                let Some(mut tests) = self.record_tests(object)? else {
                    return Ok(None);
                };
                if self.table.properties.contains_key(name.as_str())
                    || self.table.member_names.contains(name.as_str())
                {
                    let contains = self.native(
                        "record.contains",
                        vec![object.expr(), Operand::text(name.clone()).expr()],
                    )?;
                    tests.push(contains);
                }
                let read = Expr::Member(Box::new(Member::Field {
                    target: object.expr(),
                    field: name.clone(),
                }));
                Ok(Some(Direct {
                    tests,
                    read,
                    hole: false,
                    ty: Ty::Unknown,
                }))
            }
            Key::Computed(key) if !key.ty.is_number() => {
                let Some(mut tests) = self.record_tests(object)? else {
                    return Ok(None);
                };
                tests.extend(self.own_field_tests(object, key)?);
                let read = Expr::Member(Box::new(Member::Index {
                    target: object.expr(),
                    index: key.expr(),
                }));
                Ok(Some(Direct {
                    tests,
                    read,
                    hole: false,
                    ty: Ty::Unknown,
                }))
            }
            Key::Computed(index) => {
                let Some(mut tests) = self.list_tests(object)? else {
                    return Ok(None);
                };
                let Some(position) = self.position_tests(object, index, false)? else {
                    return Ok(None);
                };
                tests.extend(position);
                let read = Expr::Member(Box::new(Member::Index {
                    target: object.expr(),
                    index: index.expr(),
                }));
                Ok(Some(Direct {
                    tests,
                    read,
                    hole: true,
                    ty: Ty::Unknown,
                }))
            }
        }
    }

    /// The tests that `object` is a plain record: none when it is a record
    /// the source built, and `None` when its type says it is no record.
    fn record_tests(&mut self, object: &Operand) -> Lowering<Option<Vec<Expr>>> {
        let mut tests = Vec::new();
        match object.ty {
            Ty::Object => {}
            Ty::Unknown | Ty::Record(_) | Ty::Union(_) => {
                let kind = self.native("kind", vec![object.expr()])?;
                tests.push(self.same(kind, Operand::text("record").expr())?);
            }
            _ => return Ok(None),
        }
        let brand = Expr::Member(Box::new(Member::Field {
            target: object.expr(),
            field: "brand".to_string(),
        }));
        let kind = self.native("kind", vec![brand])?;
        let branded = self.same(kind, Operand::text("text").expr())?;
        tests.push(self.native("bool.not", vec![branded])?);
        Ok(Some(tests))
    }

    /// The tests that `key` is a text that names a field the plain record
    /// `object` holds, which every reader of a computed name gives before
    /// any built-in's member by that name (`ts.get_computed`).
    fn own_field_tests(&mut self, object: &Operand, key: &Operand) -> Lowering<Vec<Expr>> {
        let kind = self.native("kind", vec![key.expr()])?;
        let text = self.same(kind, Operand::text("text").expr())?;
        let contains = self.native("record.contains", vec![object.expr(), key.expr()])?;
        Ok(vec![text, contains])
    }

    /// The tests that `object` is a kernel list: none when the source built
    /// it, and `None` when its type says it is no list.
    fn list_tests(&mut self, object: &Operand) -> Lowering<Option<Vec<Expr>>> {
        match object.ty {
            Ty::Array => Ok(Some(Vec::new())),
            Ty::Unknown | Ty::Union(_) => {
                let kind = self.native("kind", vec![object.expr()])?;
                Ok(Some(vec![self.same(kind, Operand::text("list").expr())?]))
            }
            _ => Ok(None),
        }
    }

    /// The tests that `index`, a float, names a position of the list
    /// `object` holds: `ts.number_index`'s, an integer that is not negative
    /// and below the length. `append` admits the length itself, where a
    /// write appends. `None` when no such float can: the index may be no
    /// float, or is a literal that is no position.
    fn position_tests(
        &mut self,
        object: &Operand,
        index: &Operand,
        append: bool,
    ) -> Lowering<Option<Vec<Expr>>> {
        if index.ty != Ty::Float {
            return Ok(None);
        }
        let mut tests = Vec::new();
        match &index.atom {
            Atom::Literal(Literal::Float(value)) => {
                let value = value.get();
                if !(value >= 0.0 && value.fract() == 0.0) {
                    return Ok(None);
                }
            }
            Atom::Literal(_) => return Ok(None),
            Atom::Variable(_) => {
                // Only a whole number that is not negative is its own
                // absolute value rounded down; NaN is not, and -0 is 0.
                let floor = self.native("num.floor", vec![index.expr()])?;
                let magnitude = self.native("num.abs", vec![index.expr()])?;
                tests.push(self.native("eq", vec![floor, magnitude])?);
            }
        }
        let length = self.native("list.len", vec![object.expr()])?;
        let bound = if append { "num.le" } else { "num.lt" };
        tests.push(self.native(bound, vec![index.expr(), length])?);
        Ok(Some(tests))
    }

    /// Lowers `fast` under `tests`, each tested only once those before it
    /// held, and `slow` wherever one fails.
    fn guard(
        &mut self,
        tests: &[Expr],
        fast: &mut dyn FnMut(&mut Self) -> Lowering<()>,
        slow: &mut dyn FnMut(&mut Self) -> Lowering<()>,
    ) -> Lowering<()> {
        let Some((test, rest)) = tests.split_first() else {
            return fast(self);
        };
        let then_block = self.block(|this| this.guard(rest, fast, slow))?;
        let else_block = self.block(|this| slow(this))?;
        self.emit_if(test.clone(), then_block, else_block);
        Ok(())
    }

    /// Admit every dialect array while reserving kernel list operations for
    /// actual lists. Non-list arrays keep their branded access semantics.
    pub(super) fn array_is_list(&mut self, object: &Operand) -> Lowering<Operand> {
        let kind = self.native("kind", vec![object.expr()])?;
        let list = self.same(kind, Operand::text("list").expr())?;
        let list = self.let_expr(list, Ty::Bool);
        let branded = self.block(|this| {
            let array = this.invoke("ts.array.is", std::slice::from_ref(object), Ty::Bool)?;
            let invalid = this.block(|this| {
                let error = this.native(
                    "error.new",
                    vec![
                        Operand::text("type_error").expr(),
                        Operand::text("expected an array").expr(),
                        Expr::Literal(Literal::Null),
                    ],
                )?;
                this.emit(Stmt::Throw { value: error });
                Ok(())
            })?;
            this.emit_if(array.expr(), Buf::default(), invalid);
            Ok(())
        })?;
        self.emit_if(list.expr(), Buf::default(), branded);
        Ok(list)
    }

    fn array_branch(
        &mut self,
        object: &Operand,
        list: impl FnOnce(&mut Self) -> Lowering<Operand>,
        branded: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        let is_list = self.array_is_list(object)?;
        let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let place = Place::Variable(super::statements::variable_of(&result));
        let mut ty = Ty::Never;
        let list = self.block(|this| {
            let value = list(this)?;
            ty = ty.join(&value.ty);
            this.store(place.clone(), value);
            Ok(())
        })?;
        let branded = self.block(|this| {
            let value = branded(this)?;
            ty = ty.join(&value.ty);
            this.store(place, value);
            Ok(())
        })?;
        self.emit_if(is_list.expr(), list, branded);
        Ok(Operand { ty, ..result })
    }

    /// `object[key] = value`. A plain record's field by a name the source
    /// spells or a text key gives, and a list's element at one of its positions or just past
    /// them, are written in place, as `ts.set` writes them; any other write
    /// is the helper.
    pub(super) fn set_member(
        &mut self,
        object: &Operand,
        key: &Key,
        value: Operand,
    ) -> Lowering<()> {
        let direct = match key {
            Key::Static(name) => self.record_tests(object)?.map(|tests| {
                let place = Place::Member(Member::Field {
                    target: object.expr(),
                    field: name.clone(),
                });
                (tests, place)
            }),
            Key::Computed(key) if !key.ty.is_number() => match self.record_tests(object)? {
                Some(mut tests) => {
                    // A text key writes the field it names (`ts.set`).
                    let kind = self.native("kind", vec![key.expr()])?;
                    tests.push(self.same(kind, Operand::text("text").expr())?);
                    let place = Place::Member(Member::Index {
                        target: object.expr(),
                        index: key.expr(),
                    });
                    Some((tests, place))
                }
                None => None,
            },
            Key::Computed(index) => match self.list_tests(object)? {
                Some(mut tests) => self.position_tests(object, index, true)?.map(|position| {
                    tests.extend(position);
                    let place = Place::Member(Member::Index {
                        target: object.expr(),
                        index: index.expr(),
                    });
                    (tests, place)
                }),
                None => None,
            },
        };
        let mut helper = |this: &mut Self| {
            let written = this.invoke(
                "ts.set",
                &[object.clone(), key.operand(), value.clone()],
                Ty::Unknown,
            )?;
            this.discard(written);
            Ok(())
        };
        let Some((tests, place)) = direct else {
            return helper(self);
        };
        self.guard(
            &tests,
            &mut |this| {
                this.emit(Stmt::Assign {
                    place: place.clone(),
                    value: lash_kernel_doc::Rhs::Expr(value.expr()),
                });
                Ok(())
            },
            &mut helper,
        )
    }

    /// The refusal of reflection on a built-in the source names: an
    /// own-property test, a descriptor, or a write or `delete` of one of its
    /// properties. `None` when `subject` is not a built-in value.
    pub(super) fn reflection_on(&self, subject: &ast::Expr, what: &str) -> Option<Diagnostic> {
        let path = self.global_path(subject)?;
        self.table.builtins.contains_key(path.as_str()).then(|| {
            Diagnostic::with_repair(
                DiagnosticCode::ReflectionUnsupported,
                format!("Unsupported: {what} `{path}`, which is reflection on a built-in"),
                format!("call `{path}` or read the value it gives; keep data of your own in a plain object"),
                self.span,
            )
        })
    }

    /// The dotted path of an expression rooted at a name the source does
    /// not bind: `Math.max`, `console.log`.
    fn global_path(&self, expr: &ast::Expr) -> Option<String> {
        match expr {
            ast::Expr::Ident(name, _) => (!self.is_bound(name)
                && crate::builtins::is_global(name)
                && !matches!(name.as_str(), "NaN" | "Infinity" | "undefined"))
            .then(|| name.clone()),
            ast::Expr::Member {
                object,
                property: ast::MemberProperty::Field(field),
                ..
            } => {
                let path = self.global_path(object)?;
                // Constants are receivers, rather than namespaces of global
                // functions: Number.NaN.toString uses the Number method row.
                (!self.table.values.contains_key(path.as_str())).then(|| format!("{path}.{field}"))
            }
            _ => None,
        }
    }

    /// A read of a global path: its value row, the token of the built-in
    /// it names (`builtins/mod.rs`), or `globalThis.name` as the name
    /// itself. `None` when no row knows the path, which is then read as a
    /// member of the value before it.
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
        if let Some(token) = self.token(path) {
            return Some(token);
        }
        (!path.contains('.') && crate::builtins::is_global(path)).then(|| {
            Err(Diagnostic {
                kind: DiagnosticKind::Refusal,
                ..Diagnostic::with_repair(
                    DiagnosticCode::UnsupportedExpression,
                    format!("Unsupported: `{path}` as a value"),
                    format!("call a function of `{path}` or read one of its constants"),
                    span,
                )
            })
        })
    }

    /// The token a built-in path is as a value (`builtins/mod.rs`), with
    /// the closure that calls it when it is a function.
    fn token(&mut self, path: &str) -> Option<Lowering<Operand>> {
        let path = crate::builtins::canonical(path);
        let builtin = *self.table.builtins.get(path)?;
        Some(self.make_token(path, builtin))
    }

    fn make_token(&mut self, path: &str, builtin: crate::builtins::Builtin) -> Lowering<Operand> {
        let text = |value: &str| Expr::Literal(Literal::Text(value.to_string()));
        let items = match builtin {
            crate::builtins::Builtin::Function { call, length } => {
                let (helper, class) = call.callee(path);
                let function = self.function(helper)?;
                let this = self.fresh("this");
                let args = self.fresh("args");
                let body = self.block(|lowerer| {
                    let passed = match class {
                        Some(class) => vec![lash_kernel_doc::Atom::Literal(Literal::Text(
                            class.to_string(),
                        ))],
                        None => vec![
                            lash_kernel_doc::Atom::Variable(this.clone()),
                            lash_kernel_doc::Atom::Variable(args.clone()),
                        ],
                    };
                    let result = lowerer.emit_action(
                        Action::Call {
                            callee: Callee::Library(function),
                            args: passed,
                        },
                        Ty::Unknown,
                    );
                    lowerer.emit(Stmt::Return {
                        value: result.expr(),
                    });
                    Ok(())
                })?;
                let call = self.emit_closure(vec![this, args], body);
                vec![
                    text(crate::FUNCTION_TAG),
                    text(path),
                    text(crate::builtins::function_name(path)),
                    Expr::Literal(Literal::Float(lash_kernel_doc::Float::new(f64::from(
                        length,
                    )))),
                    call.expr(),
                ]
            }
            crate::builtins::Builtin::Object => vec![
                text("ts.object"),
                text(path),
                text(crate::builtins::object_tag(path)),
            ],
        };
        Ok(self.let_expr(Expr::Tuple(items), Ty::Unknown))
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

    /// Calls a function value: `apply f(this, args)`. A value that may not
    /// be a function's token is made callable by `ts.callable`; a function
    /// declaration nothing assigns to always holds its token, whose closure
    /// is called directly.
    fn apply(
        &mut self,
        function: Operand,
        this: Operand,
        args: Operand,
        declared: bool,
    ) -> Lowering<Operand> {
        // A call gives what the function's declared return type says.
        let returned = function.ty.returned();
        // A function the source made, held where nothing else is ever
        // assigned, is its token (`types::facts`).
        let declared = declared || matches!(function.ty, Ty::Function(_));
        let function = match function.atom {
            Atom::Variable(_) if declared => {
                let closure = self.let_expr(Self::element(&function, 4), Ty::Unknown);
                super::statements::variable_of(&closure)
            }
            Atom::Variable(_) => {
                let callable = self.invoke("ts.callable", &[function], Ty::Unknown)?;
                super::statements::variable_of(&callable)
            }
            Atom::Literal(_) => {
                // Calling a literal raises the kernel's `type_error`, which
                // is what the language asks for.
                let held = self.let_expr(function.expr(), function.ty);
                super::statements::variable_of(&held)
            }
        };
        Ok(self.emit_action(
            Action::Call {
                callee: Callee::Value(function),
                args: vec![this.atom, args.atom],
            },
            returned,
        ))
    }

    pub(super) fn lower_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::CallArg],
        span: SourceSpan,
    ) -> Lowering<Operand> {
        if let ast::Expr::Member {
            object,
            property: ast::MemberProperty::Field(name),
            span: member_span,
            ..
        } = callee
            && name == "toLocaleString"
            && matches!(
                object.as_ref(),
                ast::Expr::Array(_) | ast::Expr::Number(_) | ast::Expr::String(_)
            )
        {
            let method = &self.source[member_span.start..member_span.end];
            return Err(Diagnostic {
                kind: DiagnosticKind::Refusal,
                ..Diagnostic::with_repair(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: toLocaleString/Intl formatting is locale-dependent",
                    format!(
                        "build the deterministic string explicitly from the receiver of `{method}`"
                    ),
                    Some(span),
                )
            });
        }
        if let Some(wait) = self.wait_of(callee) {
            return self.lower_wait_call(wait, args, span);
        }
        if let Some(refused) = self.reflective_call(callee, args) {
            return Err(refused);
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
                } else if path.starts_with("Number.prototype.") {
                    Operand::number(0.0)
                } else if path.starts_with("Boolean.prototype.") {
                    Operand::bool(false)
                } else if let Some((prototype, _)) = path.rsplit_once('.')
                    && prototype.ends_with(".prototype")
                    && let Some(token) = self.token(prototype)
                {
                    token?
                } else {
                    Operand::undefined()
                };
                return self.invoke(function, &[receiver, args], Ty::Unknown);
            }
            // A method every function or object inherits is called on the
            // built-in's token; any other name a built-in lacks is refused.
            let inherited = path.rsplit_once('.').is_some_and(|(base, method)| {
                self.table.builtins.contains_key(base) && INHERITED.contains(&method)
            });
            // A namespace or a prototype called is the TypeError of calling
            // any object, raised when the call runs.
            let object = matches!(
                self.table.builtins.get(path.as_str()),
                Some(crate::builtins::Builtin::Object)
            );
            if !inherited
                && !object
                && !path.starts_with("globalThis.")
                && !self.table.values.contains_key(path.as_str())
            {
                return Err(Diagnostic {
                    kind: DiagnosticKind::Refusal,
                    ..Diagnostic::with_repair(
                        DiagnosticCode::MethodUnsupported,
                        format!(
                            "Unsupported: `{path}` is not a function the TypeScript dialect has"
                        ),
                        format!(
                            "replace `{path}` with a method the dialect's standard-library contract lists for that receiver"
                        ),
                        Some(span),
                    )
                });
            }
        }
        match callee {
            ast::Expr::Member {
                object, property, ..
            } => {
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                if let Key::Static(name) = &key
                    && let Some(called) = self.call_declared(&object, name, args)?
                {
                    return Ok(called);
                }
                let function = self.get_member(&object, &key)?;
                let function = self.pin(function);
                let args = self.arguments(args)?;
                self.apply(function, object, args, false)
            }
            ast::Expr::OptionalChain { base, operations } => {
                let (function, receiver) = self.optional_reference(base, operations)?;
                let args = self.arguments(args)?;
                self.apply(function, receiver, args, false)
            }
            _ => {
                let declared = matches!(callee, ast::Expr::Ident(name, _) if self.holds_declared_function(name));
                let function = self.lower_expr(callee)?;
                let function = if Self::args_are_inert(args) {
                    function
                } else {
                    self.pin(function)
                };
                let args = self.arguments(args)?;
                self.apply(function, Operand::undefined(), args, declared)
            }
        }
    }

    /// `object.name(args)` where the object's type is one built-in kind
    /// (`TS_TYPED_*_METHOD`): the call is the method row that kind's
    /// dispatcher would choose, without the dispatch. `push` on an array is
    /// the kernel's own append. The kind is checked where JavaScript reads
    /// the method, before the arguments, and a value of another kind raises
    /// `type_error`. `None` when the type names no single kind or the kind
    /// has no row by that name.
    fn call_declared(
        &mut self,
        object: &Operand,
        name: &str,
        args: &[ast::CallArg],
    ) -> Lowering<Option<Operand>> {
        let Some((receiver, check)) = declared_receiver(&object.ty) else {
            return Ok(None);
        };
        let Some(function) = self
            .table
            .methods
            .get(name)
            .and_then(|rows| rows.iter().find(|(kind, _)| *kind == receiver))
            .map(|(_, function)| *function)
        else {
            return Ok(None);
        };
        let fresh = crate::types::FRESH_LIST_METHODS.contains(&name);
        if object.ty == Ty::Array {
            // A list the source built takes its row without the test.
            let result = self.call_list_method(object, name, function, args)?;
            return Ok(Some(if fresh {
                Operand {
                    ty: Ty::Array,
                    ..result
                }
            } else {
                result
            }));
        }
        if receiver == Receiver::List {
            return self
                .array_branch(
                    object,
                    |this| this.call_list_method(object, name, function, args),
                    |this| {
                        let generic = Operand {
                            ty: Ty::Unknown,
                            ..object.clone()
                        };
                        let method = this.get_member(&generic, &Key::Static(name.into()))?;
                        let method = this.pin(method);
                        let args = this.arguments(args)?;
                        this.apply(method, object.clone(), args, false)
                    },
                )
                .map(|result| {
                    // Either way the method is the built-in one, which
                    // makes a new list.
                    Some(if fresh {
                        Operand {
                            ty: Ty::Array,
                            ..result
                        }
                    } else {
                        result
                    })
                });
        }
        if let Some(check) = check {
            let checked = self.native(check, vec![object.expr()])?;
            let checked = self.let_expr(checked, Ty::Unknown);
            self.discard(checked);
        }
        let args = self.arguments(args)?;
        Ok(Some(self.invoke(
            function,
            &[object.clone(), args],
            Ty::Unknown,
        )?))
    }

    fn call_list_method(
        &mut self,
        object: &Operand,
        name: &str,
        function: &str,
        args: &[ast::CallArg],
    ) -> Lowering<Operand> {
        let values: Option<Vec<&ast::Expr>> = args
            .iter()
            .map(|arg| match arg {
                ast::CallArg::Value(value) => Some(value),
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        if name == "push"
            && let Some(values) = values
        {
            // Each value is appended at the length the list has then
            // (`K-FORM-006`), after every argument has been evaluated.
            for value in self.operands(&values)? {
                let length = self.native("list.len", vec![object.expr()])?;
                let place = Place::Member(Member::Index {
                    target: object.expr(),
                    index: length,
                });
                self.store(place, value);
            }
            let length = self.native("list.len", vec![object.expr()])?;
            let length = self.native("num.to_float", vec![length])?;
            return Ok(self.let_expr(length, Ty::Float));
        }
        let args = self.arguments(args)?;
        self.invoke(function, &[object.clone(), args], Ty::Unknown)
    }

    /// `Object.hasOwn(Math, k)`, `Math.hasOwnProperty(k)` and the like: a
    /// call whose only subject is a built-in's own properties.
    fn reflective_call(&self, callee: &ast::Expr, args: &[ast::CallArg]) -> Option<Diagnostic> {
        let ast::Expr::Member {
            object,
            property: ast::MemberProperty::Field(method),
            ..
        } = callee
        else {
            return None;
        };
        if matches!(method.as_str(), "hasOwnProperty" | "propertyIsEnumerable") {
            return self.reflection_on(object, "testing an own property of");
        }
        let function = self.global_path(callee)?;
        let reflective = matches!(
            function.as_str(),
            "Object.hasOwn"
                | "Object.getOwnPropertyDescriptor"
                | "Object.getOwnPropertyDescriptors"
                | "Object.defineProperty"
                | "Object.defineProperties"
                | "Object.getPrototypeOf"
                | "Object.setPrototypeOf"
        );
        match args.first() {
            Some(ast::CallArg::Value(subject)) if reflective => {
                self.reflection_on(subject, &format!("`{function}` of"))
            }
            _ => None,
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
            let Some(idiom) = DiagnosticCode::NewUnsupported.accepted_idiom() else {
                unreachable!("constructors name their accepted forms");
            };
            return Err(Diagnostic::with_repair(
                DiagnosticCode::NewUnsupported,
                format!("`new {constructor}` is not in the TypeScript dialect"),
                format!("use a plain-object factory for `{constructor}`; {}", idiom),
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
        self.optional_reference(base, operations)
            .map(|(value, _)| value)
    }

    /// Parentheses end short-circuiting but retain a member reference's receiver.
    fn optional_reference(
        &mut self,
        base: &ast::Expr,
        operations: &[ast::OptionalOperation],
    ) -> Lowering<(Operand, Operand)> {
        let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let place = Place::Variable(super::statements::variable_of(&result));
        let receiver = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let receiver_place = Place::Variable(super::statements::variable_of(&receiver));
        let link = match base {
            ast::Expr::Member {
                object, property, ..
            } if self.global_path(base).is_none() => {
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                Link::Member { object, key }
            }
            ast::Expr::OptionalChain { base, operations } => {
                let (value, receiver) = self.optional_reference(base, operations)?;
                Link::Value { value, receiver }
            }
            _ => {
                let value = self.lower_expr(base)?;
                Link::Value {
                    value: self.pin(value),
                    receiver: Operand::undefined(),
                }
            }
        };
        self.chain(link, operations, &place, &receiver_place)?;
        Ok((result, receiver))
    }

    fn chain(
        &mut self,
        link: Link,
        operations: &[ast::OptionalOperation],
        place: &Place,
        receiver_place: &Place,
    ) -> Lowering<()> {
        let Some((operation, rest)) = operations.split_first() else {
            let (value, receiver) = match link {
                Link::Value { value, receiver } => (value, receiver),
                Link::Member { object, key } => (self.get_member(&object, &key)?, object),
            };
            self.store(place.clone(), value);
            self.store(receiver_place.clone(), receiver);
            return Ok(());
        };
        match operation {
            ast::OptionalOperation::Member { property, optional } => {
                let object = match link {
                    Link::Value { value, .. } => value,
                    Link::Member { object, key } => self.get_member(&object, &key)?,
                };
                let object = self.pin(object);
                self.unless_nullish(*optional, &object, |this| {
                    let key = this.lower_key(property)?;
                    this.chain(
                        Link::Member {
                            object: object.clone(),
                            key,
                        },
                        rest,
                        place,
                        receiver_place,
                    )
                })
            }
            ast::OptionalOperation::Call { args, optional } => {
                let (function, receiver) = match link {
                    Link::Value { value, receiver } => (value, receiver),
                    Link::Member { object, key } => (self.get_member(&object, &key)?, object),
                };
                let function = self.pin(function);
                self.unless_nullish(*optional, &function, |lowerer| {
                    let args = lowerer.arguments(args)?;
                    let value = lowerer.apply(function.clone(), receiver, args, false)?;
                    lowerer.chain(
                        Link::Value {
                            value,
                            receiver: Operand::undefined(),
                        },
                        rest,
                        place,
                        receiver_place,
                    )
                })
            }
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
