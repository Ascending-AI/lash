//! Package code in TypeScript source (§2.3 and §2.4 of the host-package
//! design). A package is in scope by its path and imports nothing: every
//! segment after the path is the TypeScript spelling of a canonical name,
//! camelCase for a function or member and PascalCase for a type or an
//! error.
//!
//! - `dataframe.col(x)` invokes the function's identity with its slots
//!   filled by the canonical convention ([`dispatch::fill`]);
//!   `dataframe.Context.create()` invokes a static the same way.
//! - A member call on a value whose package type is known (a member's
//!   declared result, or an annotation) invokes that type's member with the
//!   receiver first; a member it lacks is refused here (sealed values).
//!   `expr.name`, a property, invokes its getter.
//! - A member call on a receiver of unknown type selects, before any
//!   argument, a package member by the receiver's brand, the ordinary
//!   member read, or the attribute error ([`dispatch::Selection`]).
//! - A code member read as a value is a function value that invokes the
//!   member's identity; `const { col } = dataframe` binds one by name. A
//!   namespace, a value type, an error and an operation are never values.

use lash_kernel_dialect::dispatch::{self, Adapt, Brand, Filled, Selection, Target};
use lash_kernel_dialect::{
    FunctionMember, NamespaceKind, NamespaceView, Resolution, SpellKind, Spelling, TypeMember,
    ValueType,
};
use lash_kernel_doc::{ActKind, Callee, Expr, Literal, Place, Rhs, Stmt};

use lash_vocabulary::{PackagePath, Segment};

use super::calls::Key;
use super::{Lowerer, Lowering, Operand, Ty};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

const SPELLING: Spelling = Spelling::TypeScript;

/// What a dotted path in the source names among the packages in view.
pub(super) enum PackageRef<'a> {
    /// A package, or a prefix of package paths: a namespace.
    Namespace(String),
    /// A package function, or a static of a value type.
    Callable {
        member: &'a FunctionMember,
        spelled: String,
    },
    /// A value type.
    Type { spelled: String },
    /// An error, by its kernel kind: the error's canonical path.
    Error { kind: String, spelled: String },
}

/// One argument list, evaluated once: each argument by itself, or, when
/// one is spread, the whole list.
enum Passed {
    Each(Vec<Operand>),
    List(Operand),
}

impl<'a> Lowerer<'a> {
    /// The segments of a static path whose root the source does not bind
    /// and that a package path starts with. A computed key that is a string
    /// literal is a segment as written: the qualified escape,
    /// `dataframe["delete"]`.
    fn package_segments<'e>(&'e self, expr: &'e ast::Expr) -> Option<Vec<&'e str>> {
        let (root, path) = super::calls::path_segments(expr)?;
        if self.is_bound(root) {
            return None;
        }
        if let Some(prefix) = self.imported.get(root) {
            return Some(
                prefix
                    .iter()
                    .map(String::as_str)
                    .chain(path.iter().skip(1).copied())
                    .collect(),
            );
        }
        self.namespaces.is_root(root).then_some(path)
    }

    /// What `expr` names among the packages in view: `None` when it is no
    /// package path, or when it names an operation, which is a tool call's.
    ///
    /// # Errors
    ///
    /// A path under a code package that names nothing the package exports,
    /// or a segment written in another dialect's spelling, with the repair.
    pub(super) fn package_ref(&self, expr: &ast::Expr) -> Lowering<Option<PackageRef<'a>>> {
        let Some(path) = self.package_segments(expr) else {
            return Ok(None);
        };
        let spelled = path.join(".");
        let found = match self.namespaces.resolve(&path, SPELLING) {
            Resolution::Operation { .. } | Resolution::Deferred { .. } => return Ok(None),
            Resolution::Namespace => PackageRef::Namespace(spelled),
            Resolution::Export {
                package,
                segment,
                export,
                member,
            } => {
                let name = lash_vocabulary::QualifiedName {
                    package: package.clone(),
                    member: segment.clone(),
                    type_member: None,
                };
                let brand = lash_vocabulary::Brand {
                    package: package.clone(),
                    name: segment.clone(),
                };
                match export {
                    NamespaceKind::Type(_) => {
                        self.imports.insert(lash_vocabulary::Import {
                            name,
                            kind: lash_vocabulary::ImportKind::Type { brand },
                        });
                    }
                    NamespaceKind::Error => {
                        self.imports.insert(lash_vocabulary::Import {
                            name,
                            kind: lash_vocabulary::ImportKind::Error { kind: brand },
                        });
                    }
                    _ => {}
                }
                match (export, member) {
                    (NamespaceKind::Function(member), None) => {
                        PackageRef::Callable { member, spelled }
                    }
                    (NamespaceKind::Type(_), None) => PackageRef::Type { spelled },
                    (NamespaceKind::Error, None) => PackageRef::Error {
                        kind: format!("{package}.{segment}"),
                        spelled,
                    },
                    (NamespaceKind::Operation(_), _) => {
                        return Err(self.unknown_member(
                            format!("`{spelled}` is an operation this session does not offer"),
                            "call an operation the prompt lists".to_owned(),
                        ));
                    }
                    (_, Some((_, TypeMember::Static(member)))) => {
                        PackageRef::Callable { member, spelled }
                    }
                    (_, Some((_, TypeMember::Method(_) | TypeMember::Property(_)))) => {
                        let (owner, name) = spelled.rsplit_once('.').unwrap_or((&spelled, ""));
                        return Err(self.unknown_member(
                            format!(
                                "`{name}` is a member of `{owner}` values, not of `{owner}` itself"
                            ),
                            format!("call it on a value: `value.{name}(...)`"),
                        ));
                    }
                }
            }
            Resolution::Unknown => return self.unknown_path(&path).map_or(Ok(None), Err),
        };
        Ok(Some(found))
    }

    /// The refusal of `path`, which `Namespaces::resolve` found nothing at:
    /// `None` when no code package is above it, so that a path only
    /// operations are under stays a tool call's.
    fn unknown_path(&self, path: &[&str]) -> Option<Diagnostic> {
        let (package, exports, rest) = (1..path.len()).rev().find_map(|end| {
            let package = path[..end]
                .iter()
                .map(|segment| SPELLING.unspell(SpellKind::PathSegment, segment))
                .collect::<Option<Vec<_>>>()
                .and_then(|segments| PackagePath::try_from(segments).ok())?;
            let exports = self.namespaces.exports(&package)?;
            Some((path[..end].join("."), exports, &path[end..]))
        })?;
        let written = rest[0];
        let export = exports.iter().find(|(segment, export)| {
            let kind = export_kind(&export.kind);
            SPELLING.spell(kind, segment) == written || loosely(segment.as_str(), written)
        });
        let Some((segment, export)) = export else {
            return Some(self.unknown_member(
                format!("`{package}` exports nothing named `{written}`"),
                format!("call one of the exports the prompt lists under `{package}`"),
            ));
        };
        let owner = format!(
            "{package}.{}",
            SPELLING.spell(export_kind(&export.kind), segment)
        );
        if SPELLING.spell(export_kind(&export.kind), segment) != written {
            if let Some(misspelled) = self.namespaces.misspelled(path, SPELLING) {
                return Some(Diagnostic::with_repair(
                    DiagnosticCode::OperationSpelling,
                    format!(
                        "`{}` names the operation `{}` in another dialect's spelling",
                        path.join("."),
                        misspelled.operation.effect_name()
                    ),
                    format!("write `{}`", misspelled.repair),
                    self.span,
                ));
            }
            return Some(self.unknown_member(
                format!("`{package}.{written}` is not how TypeScript spells a package member"),
                format!("write `{owner}`"),
            ));
        }
        let (NamespaceKind::Type(ty), [_, member]) = (&export.kind, rest) else {
            return Some(self.unknown_member(
                format!("`{owner}` has no member `{}`", rest[1..].join(".")),
                format!("call `{owner}` itself"),
            ));
        };
        if let Some((name, found)) = ty
            .members
            .iter()
            .find(|(name, _)| loosely(name.as_str(), member))
        {
            let repair = SPELLING.spell(SpellKind::Member, name);
            return Some(match found {
                TypeMember::Static(_) => self.unknown_member(
                    format!("`{owner}.{member}` is not how TypeScript spells a package member"),
                    format!("write `{owner}.{repair}`"),
                ),
                TypeMember::Method(_) | TypeMember::Property(_) => self.unknown_member(
                    format!("`{repair}` is a member of `{owner}` values, not of `{owner}` itself"),
                    format!("call it on a value: `value.{repair}(...)`"),
                ),
            });
        }
        Some(self.unknown_member(
            format!("`{owner}` has no static `{member}`"),
            format!("call one of the statics the prompt lists under `{owner}`"),
        ))
    }

    fn unknown_member(&self, message: String, repair: String) -> Diagnostic {
        Diagnostic::with_repair(
            DiagnosticCode::PackageMemberUnknown,
            message,
            repair,
            self.span,
        )
    }

    /// The refusal of a namespace, a type or an error used as a value.
    fn namespace_value(&self, spelled: &str, span: Option<SourceSpan>) -> Diagnostic {
        Diagnostic::with_repair(
            DiagnosticCode::PackageNamespaceValue,
            format!("`{spelled}` is a package namespace, not a value"),
            format!(
                "write the member's full path, `{spelled}.<member>(...)`, or bind a code member by name: `const {{ <member> }} = {spelled}`"
            ),
            span.or(self.span),
        )
    }

    /// A read of a package path as a value: a code member's function
    /// value, or a refusal. `None` when `expr` is no package path.
    pub(super) fn read_package(
        &mut self,
        expr: &ast::Expr,
        span: Option<SourceSpan>,
    ) -> Option<Lowering<Operand>> {
        if let Some(path) = self.package_segments(expr)
            && let Resolution::Operation { .. } = self.namespaces.resolve(&path, SPELLING)
        {
            let spelled = path.join(".");
            return Some(Err(Diagnostic::with_repair(
                DiagnosticCode::OperationValue,
                format!("`{spelled}` is an operation, which is called, never passed as a value"),
                format!("call it where its result is needed: `await {spelled}(...)`"),
                span.or(self.span),
            )));
        }
        let found = match self.package_ref(expr) {
            Ok(found) => found?,
            Err(refused) => return Some(Err(refused)),
        };
        Some(match found {
            PackageRef::Callable { member, spelled } => self.member_value(member, &spelled),
            PackageRef::Namespace(spelled)
            | PackageRef::Type { spelled, .. }
            | PackageRef::Error { spelled, .. } => Err(self.namespace_value(&spelled, span)),
        })
    }

    /// A call whose callee is a package path: `None` when it is none.
    pub(super) fn call_package(
        &mut self,
        callee: &ast::Expr,
        args: &[ast::CallArg],
        awaited: bool,
    ) -> Option<Lowering<Operand>> {
        let found = match self.package_ref(callee) {
            Ok(found) => found?,
            Err(refused) => return Some(Err(refused)),
        };
        Some(match found {
            PackageRef::Callable { member, spelled } => {
                self.call_member(None, member, &spelled, args, awaited)
            }
            PackageRef::Namespace(spelled)
            | PackageRef::Type { spelled, .. }
            | PackageRef::Error { spelled, .. } => Err(Diagnostic::with_repair(
                DiagnosticCode::PackageNamespaceValue,
                format!("`{spelled}` is a package namespace, not a function"),
                format!("call one of its members, `{spelled}.<member>(...)`"),
                self.span,
            )),
        })
    }

    /// `value instanceof <package>.<Error>`: whether the value is a kernel
    /// error of the error's kind. `None` when the class is no package
    /// error.
    pub(super) fn instance_of_package(
        &mut self,
        value: &ast::Expr,
        class: &ast::Expr,
    ) -> Option<Lowering<Operand>> {
        let found = match self.package_ref(class) {
            Ok(found) => found?,
            Err(refused) => return Some(Err(refused)),
        };
        let PackageRef::Error { kind, .. } = found else {
            return None;
        };
        Some((|| {
            let value = self.lower_expr(value)?;
            let value = self.pin(value);
            let is_error = self.native("kind", vec![value.expr()])?;
            let is_error = self.same(is_error, Operand::text("error").expr())?;
            let kind_of = Expr::Member(Box::new(lash_kernel_doc::Member::Field {
                target: value.expr(),
                field: "kind".to_owned(),
            }));
            let same_kind = self.same(kind_of, Operand::text(kind).expr())?;
            Ok(self.inline(
                Expr::If {
                    cond: Box::new(is_error),
                    then: Box::new(same_kind),
                    r#else: Box::new(Expr::Literal(Literal::Bool(false))),
                },
                Ty::Bool,
            ))
        })())
    }

    /// `object.name(args)` where the object's package type is known: the
    /// type's member, or a refusal when the type declares none by that
    /// name.
    pub(super) fn call_typed_member(
        &mut self,
        object: &Operand,
        brand: &Brand,
        name: &str,
        args: &[ast::CallArg],
        awaited: bool,
    ) -> Lowering<Operand> {
        match self.declared_member(brand, name)? {
            TypeMember::Method(member) => {
                self.call_member(Some(object.clone()), member, name, args, awaited)
            }
            TypeMember::Property(member) => {
                let value = self.invoke_member(member, vec![object.clone()], true)?;
                let value = self.pin(value);
                let passed = self.arguments(args)?;
                self.apply_method(value, object.clone(), Operand::text(name), passed)
            }
            TypeMember::Static(_) => Err(self.static_on_value(brand, name)),
        }
    }

    /// `object.name` read where the object's package type is known: a
    /// property's getter, or a refusal.
    pub(super) fn read_typed_member(
        &mut self,
        object: &Operand,
        brand: &Brand,
        name: &str,
    ) -> Lowering<Operand> {
        match self.declared_member(brand, name)? {
            TypeMember::Property(member) => self.invoke_member(member, vec![object.clone()], true),
            TypeMember::Method(_) => Err(self.unknown_member(
                format!(
                    "`{name}` is a method of `{}` values, which is called, never read",
                    self.type_spelling(brand)
                ),
                format!("call it: `value.{name}(...)`"),
            )),
            TypeMember::Static(_) => Err(self.static_on_value(brand, name)),
        }
    }

    fn static_on_value(&self, brand: &Brand, name: &str) -> Diagnostic {
        let owner = self.type_spelling(brand);
        self.unknown_member(
            format!("`{name}` is a static of `{owner}`, not a member of its values"),
            format!("call `{owner}.{name}(...)`"),
        )
    }

    /// The TypeScript spelling of a value type's path.
    fn type_spelling(&self, brand: &Brand) -> String {
        format!(
            "{}.{}",
            brand.package_text(),
            SPELLING.spell(SpellKind::TypeName, &brand.ty)
        )
    }

    /// The member `name` that values of `brand` declare: a package value
    /// is sealed, so any other name is refused.
    fn declared_member(&self, brand: &Brand, name: &str) -> Lowering<&'a TypeMember> {
        let owner = self.type_spelling(brand);
        let Some(ty) = brand.value_type(self.namespaces) else {
            return Err(self.unknown_member(
                format!("`{owner}` is no value type of the packages in view"),
                "rebuild the value with the package's current members".to_owned(),
            ));
        };
        if let Some(member) = member_named(ty, name) {
            return Ok(member);
        }
        let repair = match Segment::new(name) {
            Ok(canonical) if ty.members.contains_key(&canonical) => {
                format!("write `{}`", SPELLING.spell(SpellKind::Member, &canonical))
            }
            _ => format!(
                "use one of the members the prompt lists for `{owner}`; a package value has no other fields"
            ),
        };
        Err(self.unknown_member(
            format!("values of `{owner}` have no member `{name}`"),
            repair,
        ))
    }

    /// Whether a package value may reach a receiver of unknown type: some
    /// package in view exports a value type, so a member read or call on
    /// such a receiver selects, and raises on a package value without the
    /// member (sealed values).
    pub(super) fn seals(&self) -> bool {
        !dispatch::typed_packages(self.namespaces).is_empty()
    }

    /// The package members a member call or read on a receiver of unknown
    /// type may select: methods for a call, properties for a read.
    pub(super) fn selectable(&self, name: &str, call: bool) -> Vec<Target<'a>> {
        let Some(segment) = SPELLING.unspell(SpellKind::Member, name) else {
            return Vec::new();
        };
        dispatch::targets(self.namespaces, &segment)
            .into_iter()
            .filter(|target| match target.member {
                TypeMember::Method(_) => call,
                TypeMember::Property(_) => !call,
                TypeMember::Static(_) => false,
            })
            .collect()
    }

    /// Selects, before anything else runs, what `object.name` is on a
    /// receiver of unknown type: a package target by the receiver's brand,
    /// the member read in the dialect's own way, or the attribute error
    /// for a package value with no such member, raised at once. Then runs
    /// `before` once, whatever was selected (a call's arguments), and
    /// `then` in each branch: with the target's member, or with `None` and
    /// the ordinary read, pinned before `before` ran.
    fn select<P, F>(
        &mut self,
        object: &Operand,
        name: &str,
        targets: &[Target<'a>],
        before: impl FnOnce(&mut Self) -> Lowering<P>,
        mut then: F,
    ) -> Lowering<Operand>
    where
        F: FnMut(&mut Self, Option<&'a FunctionMember>, &Operand, &P) -> Lowering<Operand>,
    {
        let brand = self.invoke(
            dispatch::BRAND_OF,
            std::slice::from_ref(object),
            Ty::Unknown,
        )?;
        let brand = self.pin(brand);
        let same = self.function("same")?;
        let packages = dispatch::typed_packages(self.namespaces);
        let selected = Selection::select(&brand.expr(), targets, &packages, same);
        let selected = self.let_expr(selected, Ty::Unknown);
        let method = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let read = self.block(|this| {
            let value = this.get_member(object, &Key::Static(name.to_owned()))?;
            this.store(
                Place::Variable(super::statements::variable_of(&method)),
                value,
            );
            Ok(())
        })?;
        self.emit_if(
            Selection::Ordinary.test(selected.expr(), same),
            read,
            super::Buf::default(),
        );
        let raise = self.block(|this| {
            let message = Operand::text(format!("this package value has no member `{name}`"));
            let args = this.inline(Expr::List(vec![message.expr()]), Ty::Unknown);
            let error = this.invoke(
                "ts.error.new_TypeError",
                &[Operand::undefined(), args],
                Ty::Unknown,
            )?;
            this.emit(Stmt::Throw {
                value: error.expr(),
            });
            Ok(())
        })?;
        self.emit_if(
            Selection::Raise.test(selected.expr(), same),
            raise,
            super::Buf::default(),
        );
        let passed = before(self)?;
        let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
        let place = Place::Variable(super::statements::variable_of(&result));
        let mut otherwise = self.block(|this| {
            let value = then(this, None, &method, &passed)?;
            this.store(place.clone(), value);
            Ok(())
        })?;
        for (at, target) in targets.iter().enumerate().rev() {
            let member = target.member.member();
            let chosen = self.block(|this| {
                let value = then(this, Some(member), &method, &passed)?;
                this.store(place.clone(), value);
                Ok(())
            })?;
            let test = Selection::Package(at).test(selected.expr(), same);
            otherwise = self.block(|this| {
                this.emit_if(test, chosen, otherwise);
                Ok(())
            })?;
        }
        self.append(otherwise);
        Ok(result)
    }

    /// `object.name(args)` on a receiver of unknown type that a package
    /// method may answer: selection, then the arguments once, in source
    /// order, then the selected call.
    pub(super) fn select_call(
        &mut self,
        object: &Operand,
        name: &str,
        targets: &[Target<'a>],
        args: &[ast::CallArg],
        awaited: bool,
    ) -> Lowering<Operand> {
        self.select(
            object,
            name,
            targets,
            |this| {
                let list = this.arguments(args)?;
                Ok(this.pin(list))
            },
            |this: &mut Self,
             member: Option<&'a FunctionMember>,
             method: &Operand,
             list: &Operand| {
                match member {
                    Some(member) => {
                        let slots = this.dynamic_slots(member, Some(object.clone()), list)?;
                        this.invoke_member(member, slots, awaited)
                    }
                    None => this.apply_method(
                        method.clone(),
                        object.clone(),
                        Operand::text(name),
                        list.clone(),
                    ),
                }
            },
        )
    }

    /// `object.name` on a receiver of unknown type that a package property
    /// may answer: selection, then the getter or the ordinary read.
    pub(super) fn select_read(
        &mut self,
        object: &Operand,
        name: &str,
        targets: &[Target<'a>],
    ) -> Lowering<Operand> {
        self.select(
            object,
            name,
            targets,
            |_| Ok(()),
            |this: &mut Self, member: Option<&'a FunctionMember>, method: &Operand, (): &()| {
                match member {
                    Some(member) => this.invoke_member(member, vec![object.clone()], true),
                    None => Ok(method.clone()),
                }
            },
        )
    }

    /// The function value of a code member: a TypeScript function whose
    /// closure invokes the member's identity with its slots filled from the
    /// call's arguments.
    pub(super) fn member_value(
        &mut self,
        member: &'a FunctionMember,
        spelled: &str,
    ) -> Lowering<Operand> {
        let this = self.fresh("this");
        let supplied = self.fresh("args");
        let list = Operand::variable(supplied.clone(), Ty::Unknown);
        let block = self.block(|lowerer| {
            let slots = lowerer.dynamic_slots(member, None, &list)?;
            let result = lowerer.invoke_member(member, slots, false)?;
            lowerer.emit(Stmt::Return {
                value: result.expr(),
            });
            Ok(())
        })?;
        let closure = self.emit_closure(vec![this, supplied], block, None);
        let (closure, note) = match self.take_binding_of(&closure) {
            Some((Rhs::Expr(expr), note)) => (expr, Some(note)),
            _ => (closure.expr(), None),
        };
        let name = spelled.rsplit('.').next().unwrap_or(spelled);
        let arity = dispatch::slots(member, false)
            .iter()
            .take_while(|slot| !dispatch::admits_absent(slot.ty))
            .count();
        #[expect(clippy::cast_precision_loss, reason = "a member's slot count")]
        let arity = lash_kernel_doc::Float::new(arity as f64);
        let text = |value: &str| Expr::Literal(Literal::Text(value.to_owned()));
        let token = Expr::Tuple(vec![
            text(crate::FUNCTION_TAG),
            text(""),
            text(name),
            Expr::Literal(Literal::Float(arity)),
            closure,
            Expr::Record(Vec::new()),
        ]);
        let returned = Ty::Function(Box::new(crate::types::Gives::of(member).called(false)));
        let value = self.let_expr(token, returned);
        if let Some(note) = note
            && let Some(last) = self.buf.last_note_mut()
        {
            last.blocks = note.blocks;
            last.nested_written = note.nested_written;
        }
        Ok(value)
    }

    /// A call of a member, its arguments evaluated once in source order and
    /// its slots filled by the canonical convention: the receiver first,
    /// then each argument adapted to its slot, `absent` for each trailing
    /// slot the call leaves out, and a rest slot's arguments as one list.
    fn call_member(
        &mut self,
        receiver: Option<Operand>,
        member: &'a FunctionMember,
        spelled: &str,
        args: &[ast::CallArg],
        awaited: bool,
    ) -> Lowering<Operand> {
        let slots = match self.pass(args)? {
            Passed::List(list) => self.dynamic_slots(member, receiver, &list)?,
            Passed::Each(values) => {
                let filled = dispatch::fill(member, receiver.is_some(), spelled, values).map_err(
                    |refused| {
                        Diagnostic::defect(
                            DiagnosticCode::UnsupportedExpression,
                            refused.to_string(),
                            self.span,
                        )
                    },
                )?;
                let kinds = dispatch::slots(member, receiver.is_some());
                let mut slots: Vec<Operand> = receiver.into_iter().collect();
                for (filled, slot) in filled.into_iter().zip(&kinds) {
                    slots.push(match filled {
                        Filled::One(value) => {
                            self.adapt(value, &slot.adapt, dispatch::admits_absent(slot.ty))?
                        }
                        Filled::Absent => Operand::undefined(),
                        Filled::Rest(values) => {
                            let mut items = Vec::with_capacity(values.len());
                            for value in values {
                                items.push(self.adapt(value, &slot.adapt, false)?.expr());
                            }
                            self.inline(Expr::List(items), Ty::Array)
                        }
                    });
                }
                slots
            }
        };
        self.invoke_member(member, slots, awaited)
    }

    /// A call's arguments, each by itself unless one is spread.
    fn pass(&mut self, args: &[ast::CallArg]) -> Lowering<Passed> {
        let values: Option<Vec<&ast::Expr>> = args
            .iter()
            .map(|arg| match arg {
                ast::CallArg::Value(value) => Some(value),
                ast::CallArg::Spread(_) => None,
            })
            .collect();
        Ok(match values {
            Some(values) => {
                let values = self.operands(&values)?;
                Passed::Each(values.into_iter().map(|value| self.pin(value)).collect())
            }
            None => {
                let list = self.arguments(args)?;
                Passed::List(self.pin(list))
            }
        })
    }

    /// A member's slots read from an argument list whose length is known
    /// only when it runs: a missing argument is `undefined`, which is
    /// `absent`, and a rest slot takes the list from its position on.
    fn dynamic_slots(
        &mut self,
        member: &'a FunctionMember,
        receiver: Option<Operand>,
        list: &Operand,
    ) -> Lowering<Vec<Operand>> {
        let kinds = dispatch::slots(member, receiver.is_some());
        let fixed = kinds.len() - usize::from(member.rest && !kinds.is_empty());
        let mut slots: Vec<Operand> = receiver.into_iter().collect();
        if fixed > 0 {
            #[expect(clippy::cast_precision_loss, reason = "a member's slot count")]
            let count = Operand::number(fixed as f64);
            let padded = self.invoke("ts.pad", &[list.clone(), count], Ty::Unknown)?;
            let padded = self.pin(padded);
            for (index, slot) in kinds[..fixed].iter().enumerate() {
                let value = self.inline(Self::element(&padded, index), Ty::Unknown);
                let value = self.adapt(value, &slot.adapt, true)?;
                slots.push(value);
            }
        }
        if fixed < kinds.len() {
            #[expect(clippy::cast_precision_loss, reason = "a member's slot count")]
            let start = Operand::number(fixed as f64);
            let rest = self.invoke("ts.rest", &[list.clone(), start], Ty::Array)?;
            slots.push(rest);
        }
        Ok(slots)
    }

    /// A value adapted to a slot: an integer for an `Int` slot, and a bare
    /// kernel closure for a callback slot, which calls the TypeScript
    /// function with no receiver and its parameters as the argument list.
    /// `optional` passes `undefined` as it is.
    fn adapt(&mut self, value: Operand, adapt: &Adapt, optional: bool) -> Lowering<Operand> {
        match adapt {
            Adapt::Plain => Ok(value),
            Adapt::Int => {
                if let Expr::Literal(Literal::Float(number)) = &value.expr
                    && let Some(integer) = integral(number.get())
                {
                    return Ok(Operand {
                        expr: Expr::Literal(Literal::Int(integer)),
                        ..value
                    });
                }
                let value = self.pin(value);
                let kind = self.native("kind", vec![value.expr()])?;
                let float = self.same(kind, Operand::text("float").expr())?;
                let converted = self.native("float.to_int", vec![value.expr()])?;
                Ok(self.inline(
                    Expr::If {
                        cond: Box::new(float),
                        then: Box::new(converted),
                        r#else: Box::new(value.expr()),
                    },
                    Ty::BigInt,
                ))
            }
            Adapt::Function(signature) => {
                let declared = matches!(value.ty, Ty::Function(_));
                let value = self.pin(value);
                let callable = if declared {
                    Self::element(&value, crate::FUNCTION_CALLABLE as usize)
                } else {
                    self.invoke("ts.callable", std::slice::from_ref(&value), Ty::Unknown)?
                        .expr()
                };
                let params: Vec<lash_vocabulary::Name> =
                    signature.params.iter().map(|_| self.fresh("arg")).collect();
                let wrapped = |this: &mut Self| -> Lowering<Operand> {
                    let callable = this.let_expr(callable.clone(), Ty::Unknown);
                    let target = this.temp();
                    let passed = Expr::List(params.iter().cloned().map(Expr::Variable).collect());
                    let block = this.block(|this| {
                        let call = this.action(
                            ActKind::Call(Callee::Value(target.clone())),
                            vec![Operand::undefined().expr(), passed],
                            Ty::Unknown,
                        );
                        this.emit(Stmt::Return {
                            value: Expr::Let {
                                name: target.clone(),
                                value: Box::new(callable.expr()),
                                body: Box::new(call.expr()),
                            },
                        });
                        Ok(())
                    })?;
                    // The closure states the slot's signature, so that it
                    // may cross to a runner as the member's callback.
                    let start = lash_kernel_doc::CallablePresentation {
                        signature: signature.clone(),
                        representation: lash_kernel_doc::FunctionRepresentation::Bare,
                        convention: lash_kernel_doc::StartConvention::Direct,
                    };
                    Ok(this.emit_closure(params.clone(), block, Some(start)))
                };
                if !optional || declared {
                    return wrapped(self);
                }
                let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
                let place = Place::Variable(super::statements::variable_of(&result));
                let absent = self.same(value.expr(), Expr::Literal(Literal::Absent))?;
                let wrap = self.block(|this| {
                    let closure = wrapped(this)?;
                    this.store(place, closure);
                    Ok(())
                })?;
                self.emit_if(absent, super::Buf::default(), wrap);
                Ok(result)
            }
        }
    }

    /// Invokes a member's identity over its filled slots. An asynchronous
    /// member called where its call is not awaited starts a promise whose
    /// task makes the call, as an unawaited tool call does.
    fn invoke_member(
        &mut self,
        member: &'a FunctionMember,
        slots: Vec<Operand>,
        awaited: bool,
    ) -> Lowering<Operand> {
        let result = Ty::decoded(&member.signature.result);
        if awaited || !member.asynchronous {
            let slot = self.imports.code(
                member.name.clone(),
                member.signature.clone(),
                member.convention,
            );
            return Ok(self.action(
                ActKind::Call(Callee::Import(slot)),
                slots.iter().map(Operand::expr).collect(),
                result,
            ));
        }
        let this = self.fresh("this");
        let supplied = self.fresh("args");
        let list = Operand::variable(supplied.clone(), Ty::Unknown);
        let count = slots.len();
        let block = self.block(|lowerer| {
            let values: Vec<Operand> = (0..count)
                .map(|index| lowerer.inline(Self::element(&list, index), Ty::Unknown))
                .collect();
            let slot = lowerer.imports.code(
                member.name.clone(),
                member.signature.clone(),
                member.convention,
            );
            let value = lowerer.action(
                ActKind::Call(Callee::Import(slot)),
                values.iter().map(Operand::expr).collect(),
                Ty::Unknown,
            );
            lowerer.emit(Stmt::Return {
                value: value.expr(),
            });
            Ok(())
        })?;
        let body = self.emit_closure(vec![this, supplied], block, None);
        let Some((Rhs::Expr(body), note)) = self.take_binding_of(&body) else {
            unreachable!("a member call's body is a fresh closure");
        };
        let body = self.inline(body, Ty::Unknown);
        let args = self.inline(
            Expr::List(slots.iter().map(Operand::expr).collect()),
            Ty::Unknown,
        );
        let promise = self.invoke(
            "ts.promise.start",
            &[body, Operand::undefined(), args],
            Ty::Unknown,
        )?;
        let promise = self.let_expr(promise.expr(), Ty::Unknown);
        if let Some(last) = self.buf.last_note_mut() {
            last.blocks = note.blocks;
        }
        Ok(promise)
    }

    /// `const { col, lit: literal } = dataframe`: each property a code
    /// member of the namespace, bound by its pattern to the member's
    /// function value. `None` when the initializer is no package path.
    pub(super) fn destructure_package(
        &mut self,
        pattern: &ast::Pattern,
        init: &ast::Expr,
        mode: super::patterns::Mode,
    ) -> Option<Lowering<()>> {
        let found = match self.package_ref(init) {
            Ok(found) => found?,
            Err(refused) => return Some(Err(refused)),
        };
        let PackageRef::Namespace(spelled) = found else {
            return None;
        };
        let ast::Pattern::Object {
            properties,
            rest: None,
        } = pattern
        else {
            return Some(Err(self.namespace_value(&spelled, None)));
        };
        Some((|| {
            for property in properties {
                let ast::PropertyKey::Static(name) = &property.key else {
                    return Err(self.namespace_value(&spelled, None));
                };
                let path = ast::Expr::Member {
                    object: Box::new(init.clone()),
                    property: ast::MemberProperty::Field(name.clone()),
                    span: SourceSpan { start: 0, end: 0 },
                };
                let value = match self.read_package(&path, None) {
                    Some(value) => value?,
                    None => {
                        return Err(self.unknown_member(
                            format!("`{spelled}` exports nothing named `{name}`"),
                            format!(
                                "bind one of the code members the prompt lists under `{spelled}`"
                            ),
                        ));
                    }
                };
                self.destructure(&property.value, value, mode)?;
            }
            Ok(())
        })())
    }
}

/// The member a value type declares under the TypeScript spelling `name`.
fn member_named<'v>(ty: &'v ValueType, name: &str) -> Option<&'v TypeMember> {
    let segment = SPELLING.unspell(SpellKind::Member, name)?;
    ty.members.get(&segment)
}

/// The integer a float literal is, when it is one exactly.
fn integral(number: f64) -> Option<lash_kernel_doc::Integer> {
    #[expect(clippy::cast_possible_truncation, reason = "checked to be exact")]
    let whole = number as i64;
    #[expect(clippy::cast_precision_loss, reason = "checked to be exact")]
    (number.fract() == 0.0 && whole as f64 == number && number.abs() < 9.0e15)
        .then(|| lash_kernel_doc::Integer::from(whole))
}

/// How TypeScript spells an export of this kind.
fn export_kind(export: &NamespaceKind) -> SpellKind {
    match export {
        NamespaceKind::Type(_) | NamespaceKind::Error => SpellKind::TypeName,
        NamespaceKind::Operation(_) | NamespaceKind::Function(_) => SpellKind::Member,
    }
}

/// Whether `written` is the canonical `segment` in some spelling: the
/// same letters, cased or separated otherwise (`read_text`, `readtext`).
fn loosely(segment: &str, written: &str) -> bool {
    let letters = |text: &str| {
        text.chars()
            .filter(|c| *c != '_')
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    letters(segment) == letters(written)
}
