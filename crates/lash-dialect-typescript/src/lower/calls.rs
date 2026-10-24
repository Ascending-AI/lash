//! Members, calls and the globals a built-in row answers for.

mod optional;

use lash_kernel_doc::{AssignOrder, Callee, Expr, Literal, Member, Place, Stmt};

use super::{Buf, Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::builtins::Receiver;
use crate::{Diagnostic, DiagnosticCode, DiagnosticKind, SourceSpan};

mod discovery;
mod methods;
mod values;
pub(crate) use discovery::tool_calls;

/// A statically spelled path and its root, before lexical resolution.
/// Computed properties and expression receivers are value operations.
/// `expr` as the segments of a dotted path from its root identifier.
/// `object["name"]` is the qualified escape, a segment written as a string
/// key, for a name the dialect cannot write as a property.
pub(super) fn path_segments(expr: &ast::Expr) -> Option<(&str, Vec<&str>)> {
    match expr {
        ast::Expr::Ident(name, _) => Some((name, vec![name.as_str()])),
        ast::Expr::Member {
            object, property, ..
        } => {
            let segment = match property {
                ast::MemberProperty::Field(field) => field.as_str(),
                ast::MemberProperty::Index(index) => match index.as_ref() {
                    ast::Expr::String(key) => key.as_str(),
                    _ => return None,
                },
            };
            let (root, mut path) = path_segments(object)?;
            path.push(segment);
            Some((root, path))
        }
        _ => None,
    }
}

pub(super) fn static_path(expr: &ast::Expr) -> Option<(&str, String)> {
    match expr {
        ast::Expr::Ident(name, _) => Some((name, name.clone())),
        ast::Expr::Member {
            object,
            property: ast::MemberProperty::Field(field),
            ..
        } => {
            let (root, path) = static_path(object)?;
            Some((root, format!("{path}.{field}")))
        }
        _ => None,
    }
}

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

/// A call's arguments when none is spread.
fn plain_arguments(args: &[ast::CallArg]) -> Option<Vec<&ast::Expr>> {
    args.iter()
        .map(|arg| match arg {
            ast::CallArg::Value(value) => Some(value),
            ast::CallArg::Spread(_) => None,
        })
        .collect()
}

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
        name: Operand,
    },
    /// A member not yet read: a call that follows it is a method call on
    /// the object.
    Member { object: Operand, key: Key },
}

impl Lowerer<'_> {
    /// A member's key, held so that later code cannot change it.
    pub(super) fn lower_key(&mut self, property: &ast::MemberProperty) -> Lowering<Key> {
        Ok(match property {
            ast::MemberProperty::Field(name) => Key::Static(name.clone()),
            ast::MemberProperty::Index(index) => {
                let index = self.lower_expr(index)?;
                if let Expr::Literal(Literal::Text(name)) = &index.expr {
                    Key::Static(name.clone())
                } else {
                    Key::Computed(index)
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
                Ok(Key::Computed(self.pin(key)))
            }
            Key::Computed(value) => Ok(Key::Computed(self.pin(value))),
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
        if matches!(object.ty, Ty::List(_)) {
            let object = self.reuse(object.clone());
            return self.get_list_member(&object, key);
        }
        self.get_record_member(object, key)
    }

    fn get_list_member(&mut self, object: &Operand, key: &Key) -> Lowering<Operand> {
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
                                "ts.get_numeric",
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
                            let length = this.native("num.to_float", vec![length])?;
                            Ok(this.inline(length, Ty::Float))
                        },
                        |this| this.invoke("ts.get", &[object.clone(), key.operand()], Ty::Number),
                    );
                }
                _ => {}
            }
        }
        self.get_record_member(object, key)
    }

    fn get_record_member(&mut self, object: &Operand, key: &Key) -> Lowering<Operand> {
        let ty = match key {
            Key::Static(name) => object.ty.property(name),
            Key::Computed(_) => Ty::Unknown,
        };
        if let Some(direct) = self.direct_read(object, key)? {
            if direct.tests.is_empty() && !direct.hole {
                return Ok(self.inline(direct.read, direct.ty));
            }
            let object = self.reuse(object.clone());
            let key = match key {
                Key::Static(name) => Key::Static(name.clone()),
                Key::Computed(value) => Key::Computed(self.pin(value.clone())),
            };
            let Some(direct) = self.direct_read(&object, &key)? else {
                unreachable!("holding a receiver preserves its representation");
            };
            let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
            let place = Place::Variable(super::statements::variable_of(&result));
            let read = direct.read;
            self.guard(
                &direct.tests,
                &mut |this| {
                    this.emit(Stmt::Assign {
                        place: place.clone(),
                        value: lash_kernel_doc::Rhs::Expr(read.clone()),
                        order: AssignOrder::ValueFirst,
                    });
                    if direct.hole {
                        let hole = this.same(result.expr(), Expr::Tuple(Vec::new()))?;
                        let absent = this.block(|this| {
                            this.emit(Stmt::Assign {
                                place: place.clone(),
                                value: lash_kernel_doc::Rhs::Expr(Expr::Literal(Literal::Absent)),
                                order: AssignOrder::ValueFirst,
                            });
                            Ok(())
                        })?;
                        this.emit_if(hole, absent, Buf::default());
                    }
                    Ok(())
                },
                &mut |this| {
                    let value = this.read_member(&object, &key, Ty::Unknown)?;
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
        let function = match key {
            // A numeric spelling cannot name an arbitrary built-in member.
            // Keep only the matching rows in the helper's executable reach.
            Key::Computed(index) if index.ty.is_number() => "ts.get_numeric",
            Key::Computed(_) => "ts.get_computed",
            Key::Static(_) => "ts.read",
        };
        self.invoke(function, &[object.clone(), key.operand()], ty)
    }

    /// The in-place form of a read of `object.key`, and the tests under
    /// which it gives what the helper would: `None` where no value of the
    /// object's type has one.
    ///
    /// A plain record, a record without the internal tuple marker, answers a name no
    /// built-in has with its field by that name, `undefined` when it has
    /// none (`ts.read`), and so a computed text proven to begin with a
    /// prefix no built-in name or the marker has; a name some built-in
    /// has, or any other computed text, only when the record holds a field
    /// by it (`ts.member.<name>`, `ts.property.<name>`, `ts.get_computed`). A
    /// list answers `length` with its length, and a number that is one of
    /// its positions with its element there, a hole being `undefined`
    /// (`ts.get`). Each test is a kernel expression that cannot raise once
    /// the tests before it held.
    fn direct_read(&mut self, object: &Operand, key: &Key) -> Lowering<Option<Direct>> {
        match key {
            Key::Static(name)
                if name == "length"
                    && object.plain.is_none()
                    && !matches!(object.ty, Ty::Object | Ty::Record(_)) =>
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
            // Numeric values need ToPropertyKey before a record lookup.
            // A text-only probe is invalid even behind a kind test.
            Key::Computed(key) if key.ty.is_numeric() && !key.ty.is_number() => Ok(None),
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
        if matches!(object.expr, Expr::Tuple(_)) {
            return Ok(None);
        }
        if object.plain.is_some() {
            return Ok(Some(Vec::new()));
        }
        let mut tests = Vec::new();
        match object.ty {
            Ty::Object => {}
            Ty::Unknown | Ty::Callable(_) | Ty::Map | Ty::Set | Ty::Record(_) | Ty::Union(_) => {
                let kind = self.native("kind", vec![object.expr()])?;
                tests.push(self.same(kind, Operand::text("record").expr())?);
            }
            _ => return Ok(None),
        }
        let branded = self.native(
            "record.contains",
            vec![object.expr(), Operand::text("__lash_brand").expr()],
        )?;
        tests.push(self.native("bool.not", vec![branded])?);
        Ok(Some(tests))
    }

    /// The tests that `key` is a text that names a field the plain record
    /// `object` holds, which every reader of a computed name gives before
    /// any built-in's member by that name (`ts.get_computed`). A key
    /// proven a text needs no kind test, and one proven to begin with an
    /// ordinary prefix no test: the record's field by it, or `undefined`,
    /// is what every reader gives (`builtins::ordinary_prefix`).
    fn own_field_tests(&mut self, object: &Operand, key: &Operand) -> Lowering<Vec<Expr>> {
        let mut tests = Vec::new();
        match key.text_prefix.as_deref() {
            Some(prefix) if crate::builtins::ordinary_prefix(prefix) => return Ok(tests),
            Some(_) => {}
            None => {
                let kind = self.native("kind", vec![key.expr()])?;
                tests.push(self.same(kind, Operand::text("text").expr())?);
            }
        }
        tests.push(self.native("record.contains", vec![object.expr(), key.expr()])?);
        Ok(tests)
    }

    /// The tests that `object` is a kernel list: none when the source built
    /// it, and `None` when its type says it is no list.
    fn list_tests(&mut self, object: &Operand) -> Lowering<Option<Vec<Expr>>> {
        if matches!(object.expr, Expr::Tuple(_)) {
            return Ok(None);
        }
        match object.ty {
            Ty::Array => Ok(Some(Vec::new())),
            Ty::Unknown | Ty::Callable(_) | Ty::Map | Ty::Set | Ty::Union(_) => {
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
        match &index.expr {
            Expr::Literal(Literal::Float(value)) => {
                let value = value.get();
                if !(value >= 0.0 && value.fract() == 0.0) {
                    return Ok(None);
                }
            }
            Expr::Literal(_) => return Ok(None),
            _ => {
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

    /// A discarded assignment evaluates its reference before its value.
    /// Keep a proven plain field write, with actions in the place, by a
    /// name the source spells or a key proven a text, which writes the
    /// field it names (`ts.set`); dynamic writes use the dialect helper
    /// without document-level write branches.
    pub(super) fn assign_member_statement(
        &mut self,
        object: &ast::Expr,
        property: &ast::MemberProperty,
        value: &ast::Expr,
    ) -> Lowering<()> {
        if let Some(refused) = self.reflection_on(object, "assigning a property of") {
            return Err(refused);
        }
        let (object_buf, object) = self.aside(|this| this.lower_expr(object))?;
        let (key_buf, key) = self.aside(|this| this.lower_key(property))?;
        let (value_buf, value) = self.aside(|this| this.lower_expr(value))?;
        let values = self.ordered(vec![
            (object_buf, object),
            (key_buf, key.operand()),
            (value_buf, value),
        ]);
        let [object, key_value, value]: [Operand; 3] = values
            .try_into()
            .unwrap_or_else(|_| unreachable!("three assignment operands"));
        let key = match key {
            Key::Static(name) => Key::Static(name),
            Key::Computed(_) => Key::Computed(key_value),
        };
        let place = match &key {
            Key::Static(field) => self
                .record_tests(&object)?
                .is_some_and(|tests| tests.is_empty())
                .then(|| Member::Field {
                    target: object.expr(),
                    field: field.clone(),
                }),
            Key::Computed(key) if key.text_prefix.is_some() && object.plain.is_some() => {
                Some(Member::Index {
                    target: object.expr(),
                    index: key.expr(),
                })
            }
            Key::Computed(_) => None,
        };
        if let Some(place) = place {
            self.emit(Stmt::Assign {
                place: Place::Member(place),
                value: lash_kernel_doc::Rhs::Expr(value.expr()),
                order: AssignOrder::PlaceFirst,
            });
        } else {
            let value = self.invoke("ts.set", &[object, key.operand(), value], Ty::Unknown)?;
            self.discard(value);
        }
        Ok(())
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
        let value = self.pin(value);
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
                    if key.text_prefix.is_none() {
                        let kind = self.native("kind", vec![key.expr()])?;
                        tests.push(self.same(kind, Operand::text("text").expr())?);
                    }
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
                    order: AssignOrder::ValueFirst,
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
        self.table.builtin(&path).is_some().then(|| {
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
        if let Some(read) = self.read_package(whole, Some(span)) {
            return read;
        }
        let object = self.lower_expr(object)?;
        let (buf, key) = self.aside(|this| this.lower_key(property))?;
        let object = if buf.is_empty() {
            object
        } else {
            self.pin(object)
        };
        self.append(buf);
        if let Key::Static(name) = &key {
            if let Ty::Package(brand) = &object.ty {
                let brand = brand.clone();
                return self.read_typed_member(&object, &brand, name);
            }
            if matches!(
                object.ty,
                Ty::Unknown | Ty::Callable(_) | Ty::Map | Ty::Set | Ty::Union(_)
            ) {
                let targets = self.selectable(name, false);
                if !targets.is_empty() || self.seals() {
                    let object = self.pin(object);
                    return self.select_read(&object, name, &targets);
                }
            }
        }
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

    pub(super) fn lower_call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::CallArg],
        span: SourceSpan,
        discard_result: bool,
    ) -> Lowering<Operand> {
        self.expression_block(|this| this.call(callee, args, span, discard_result, false))
    }

    /// A call. `awaited` is whether an `await` stands right before it: an
    /// asynchronous package member then gives its result rather than a
    /// promise.
    pub(super) fn call(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::CallArg],
        span: SourceSpan,
        discard_result: bool,
        awaited: bool,
    ) -> Lowering<Operand> {
        if let Some((root, path)) = static_path(callee)
            && !self.is_bound(root)
            && let Some(constructor) =
                lash_kernel_dialect::value_constructors::Constructor::named(&path)
        {
            let args = plain_arguments(args).ok_or_else(|| {
                Diagnostic::new(
                    DiagnosticCode::InvalidAst,
                    "value constructors take positional arguments",
                    Some(span),
                )
            })?;
            let operands = self.operands(&args)?;
            if matches!(
                constructor,
                lash_kernel_dialect::value_constructors::Constructor::CallToken
            ) {
                let Some((function, args)) = operands.split_first() else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::InvalidAst,
                        "callable.invoke_token requires a token",
                        Some(span),
                    ));
                };
                let args = self.inline(
                    Expr::List(args.iter().map(Operand::expr).collect()),
                    Ty::Array,
                );
                return self.apply(function.clone(), Operand::undefined(), args, true);
            }
            let value = constructor
                .build(operands.iter().map(Operand::expr).collect(), |name| {
                    self.function(name).ok()
                })
                .map_err(|message| {
                    Diagnostic::new(DiagnosticCode::LinkError, message, Some(span))
                })?;
            // Printed native constructors retain the installed helper's
            // result fact. In particular, a numeric comparison is already a
            // bool: adding ToBoolean would change the printed document.
            let ty = match &value {
                Expr::Call { function, .. } => {
                    self.library.header(function).map_or(Ty::Unknown, |header| {
                        Ty::annotated(&header.signature.result)
                    })
                }
                _ => Ty::Unknown,
            };
            return Ok(self.inline(value, ty));
        }
        self.refuse_intl(callee, span)?;
        if let Some(wait) = self.wait_of(callee) {
            return self.lower_wait_call(wait, args, span);
        }
        if let Some(called) = self.call_package(callee, args, awaited) {
            return called;
        }
        self.operation_refusal(callee, span)?;
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
                let passed = self.pin(passed);
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
            if let Some(positional) = self.table.positional.get(path.as_str()).copied()
                && let Some(values) = plain_arguments(args)
            {
                return self.call_positional(positional, &values);
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
                let ty = match path.as_str() {
                    "String" => Ty::Text,
                    "JSON.stringify" => Ty::Union(vec![Ty::Text, Ty::Undefined]),
                    _ => Ty::Unknown,
                };
                return self.invoke(function, &[receiver, args], ty);
            }
            // A method every function or object inherits is called on the
            // built-in's token; any other name a built-in lacks is refused.
            let inherited = path.rsplit_once('.').is_some_and(|(base, method)| {
                self.table.builtin(base).is_some() && INHERITED.contains(&method)
            });
            // A namespace or a prototype called is the TypeError of calling
            // any object, raised when the call runs.
            let object = matches!(
                self.table.builtin(&path),
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
        if let ast::Expr::Ident(name, _) = callee
            && let Some(values) = plain_arguments(args)
            && let Some(entry) = self
                .binding_mut(name)
                .and_then(|(_, binding)| binding.positional.clone())
        {
            let returned = self.lower_expr(callee)?.ty.returned();
            let passed = self.positional_arguments(&values, entry.arity)?;
            let passed = std::iter::once(Expr::Literal(Literal::Absent))
                .chain(passed.iter().map(Operand::expr))
                .collect();
            return Ok(self.action(
                lash_kernel_doc::ActKind::Call(Callee::Value(entry.name)),
                passed,
                returned,
            ));
        }

        match callee {
            ast::Expr::Member {
                object, property, ..
            } => {
                let proven = self.facts.primitive(object);
                let object = self.lower_expr(object)?;
                let mut object = self.pin(object);
                // Representation proof can refine unknown data, but a
                // contradictory trusted kind still owns its typed refusal.
                let proven = proven.filter(|ty| {
                    declared_receiver(&object.ty).is_none_or(|(receiver, _)| {
                        declared_receiver(ty).is_some_and(|(kind, _)| kind == receiver)
                    })
                });
                if let Some(ty) = &proven {
                    object.ty = ty.clone();
                }
                let key = self.lower_key(property)?;
                let key = self.reference_key(&object, key)?;
                self.check_method(&object, &key, args, span)?;
                if let (Ty::Package(brand), Key::Static(name)) = (&object.ty, &key) {
                    let brand = brand.clone();
                    return self.call_typed_member(&object, &brand, name, args, awaited);
                }
                if let Key::Static(name) = &key
                    && matches!(
                        object.ty,
                        Ty::Unknown | Ty::Callable(_) | Ty::Map | Ty::Set | Ty::Union(_)
                    )
                {
                    let targets = self.selectable(name, true);
                    if !targets.is_empty() || self.seals() {
                        return self.select_call(&object, name, &targets, args, awaited);
                    }
                }
                if let Key::Static(name) = &key
                    && let Some(called) =
                        self.call_declared(&object, name, args, discard_result, proven.is_some())?
                {
                    return Ok(called);
                }
                if let Key::Static(name) = &key
                    && let Some(called) = self.call_map(&object, name, args)?
                {
                    return Ok(called);
                }
                let function = self.get_member(&object, &key)?;
                let function = self.pin(function);
                let args = self.arguments(args)?;
                self.apply_method(function, object, key.operand(), args)
            }
            ast::Expr::OptionalChain { base, operations } => {
                let (function, receiver, name) =
                    self.optional_reference(base, operations, false)?;
                let args = self.arguments(args)?;
                self.apply_method(function, receiver, name, args)
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

    /// A Map cannot carry guest fields: its method read always selects its
    /// built-in row. Test its kind at the member-read site, before arguments
    /// can change bindings or park. Other receivers read and pin their method
    /// before arguments, retaining own fields, failures and borrowed methods.
    /// These are new document forms, charged normally by the kernel.
    /// The first `arity` of a call's arguments, evaluated in order with
    /// the rest: extra arguments still evaluate before the call, after the
    /// retained ones are held.
    fn positional_arguments(
        &mut self,
        args: &[&ast::Expr],
        arity: usize,
    ) -> Lowering<Vec<Operand>> {
        let values = self.operands(args)?;
        let extra_statements = values
            .iter()
            .skip(arity)
            .any(|value| !matches!(value.expr, Expr::Literal(_) | Expr::Variable(_)));
        let mut passed = Vec::with_capacity(arity);
        for (index, value) in values.into_iter().enumerate() {
            if index < arity {
                passed.push(if extra_statements {
                    self.pin(value)
                } else {
                    value
                });
            } else {
                self.discard(value);
            }
        }
        Ok(passed)
    }

    /// A positional built-in called with plain arguments: its helper over
    /// them, with no argument list (`builtins::Positional`).
    fn call_positional(
        &mut self,
        positional: crate::builtins::Positional,
        args: &[&ast::Expr],
    ) -> Lowering<Operand> {
        if args.is_empty()
            && let Some(empty) = positional.empty
        {
            return Ok(match empty {
                crate::builtins::Empty::Text => Operand::text(String::new()),
                crate::builtins::Empty::Zero => Operand::number(0.0),
            });
        }
        let mut passed = self.positional_arguments(args, positional.arity)?;
        passed.resize_with(positional.arity, Operand::undefined);
        let helper = match positional.float {
            Some(float) if passed[0].ty == Ty::Float => float,
            _ => positional.helper,
        };
        self.invoke(helper, &passed, Ty::Unknown)
    }

    fn call_map(
        &mut self,
        object: &Operand,
        name: &str,
        args: &[ast::CallArg],
    ) -> Lowering<Option<Operand>> {
        if !matches!(
            object.ty,
            Ty::Unknown | Ty::Callable(_) | Ty::Map | Ty::Set | Ty::Union(_)
        ) {
            return Ok(None);
        }
        let function = match name {
            "get" => "ts.map.get",
            "set" => "ts.map.set",
            "has" => "ts.map.has",
            _ => return Ok(None),
        };
        let kind = self.native("kind", vec![object.expr()])?;
        let test = self.same(kind, Operand::text("map").expr())?;
        let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let place = Place::Variable(super::statements::variable_of(&result));
        let direct = self.block(|this| {
            let passed = this.arguments(args)?;
            let value = this.invoke(function, &[object.clone(), passed], Ty::Unknown)?;
            this.store(place.clone(), value);
            Ok(())
        })?;
        let fallback = self.block(|this| {
            let method = this.get_member(object, &Key::Static(name.into()))?;
            let method = this.pin(method);
            let passed = this.arguments(args)?;
            let value = this.apply_method(method, object.clone(), Operand::text(name), passed)?;
            this.store(place, value);
            Ok(())
        })?;
        self.emit_if(test, direct, fallback);
        Ok(Some(result))
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
        discard_result: bool,
        proven: bool,
    ) -> Lowering<Option<Operand>> {
        let Some((receiver, check)) = declared_receiver(&object.ty) else {
            return Ok(None);
        };
        let Some(function) = self
            .table
            .methods
            .get(name)
            .and_then(|rows| rows.iter().find(|(kind, _)| *kind == receiver))
            .and_then(|(_, path)| self.table.functions.get(path))
            .copied()
        else {
            return Ok(None);
        };
        if let Some(result) = self.call_primitive_method(object, name, args, proven)? {
            return Ok(Some(result));
        }
        let fresh = crate::types::FRESH_LIST_METHODS.contains(&name);
        if object.ty == Ty::Array {
            // A list the source built takes its row without the test.
            let result = self.call_list_method(object, name, function, args, discard_result)?;
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
                    |this| this.call_list_method(object, name, function, args, discard_result),
                    |this| {
                        // This branch may hold a branded array, but the kernel
                        // still knows the operand's list facet. Let the member
                        // helper dispatch it; an inlined record probe cannot
                        // link against that facet, even behind a kind guard.
                        let method =
                            this.read_member(object, &Key::Static(name.into()), Ty::Unknown)?;
                        let method = this.pin(method);
                        let args = this.arguments(args)?;
                        this.apply_method(method, object.clone(), Operand::text(name), args)
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
        discard_result: bool,
    ) -> Lowering<Operand> {
        let values: Option<Vec<&ast::Expr>> = args
            .iter()
            .map(|arg| match arg {
                ast::CallArg::Value(value) => Some(value),
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        if let Some(values) = &values
            && let Some(callback) = values.first()
            && let Some(result) = self.call_array_callback(object, name, callback, values)?
        {
            return Ok(result);
        }
        if name == "push"
            && let Some(values) = values
        {
            // Each value is appended at the length the list has then
            // (`K-FORM-006`), after every argument has been evaluated.
            let values = self.operands(&values)?;
            let values: Vec<_> = values.into_iter().map(|value| self.pin(value)).collect();
            for value in values {
                let length = self.native("list.len", vec![object.expr()])?;
                let place = Place::Member(Member::Index {
                    target: object.expr(),
                    index: length,
                });
                self.store(place, value);
            }
            // A statement-position call has no consumer of the new length.
            // Its assignments are ordinary document forms: only the append
            // work is executed and charged (K-CHG-002), in this body's account.
            if discard_result {
                return Ok(Operand::undefined());
            }
            let length = self.native("list.len", vec![object.expr()])?;
            let length = self.native("num.to_float", vec![length])?;
            return Ok(self.inline(length, Ty::Float));
        }
        let args = self.arguments(args)?;
        self.invoke(function, &[object.clone(), args], Ty::Unknown)
    }

    /// `Object.hasOwn(Math, k)`, `Math.hasOwnProperty(k)` and the like: a
    /// call whose only subject is a built-in's own properties.
    fn reflective_call(&self, callee: &ast::Expr, args: &[ast::CallArg]) -> Option<Diagnostic> {
        let ast::Expr::Member {
            property: ast::MemberProperty::Field(_),
            ..
        } = callee
        else {
            return None;
        };
        let function = self.global_path(callee)?;
        let reflective = matches!(
            function.as_str(),
            "Object.defineProperty"
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
}
