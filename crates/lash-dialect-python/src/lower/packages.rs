//! Package code in Python source (§2.3 and §2.4 of the host-package
//! design). A package is in scope by its path and imports nothing: every
//! segment after the path is the Python spelling of a canonical name, as
//! it is for a function or a member and PascalCase for a type or an error.
//!
//! - `dataframe.col(x)` invokes the function's identity with its slots
//!   filled by the canonical convention ([`dispatch::fill_placed`]); a
//!   keyword names one field of one options slot. `dataframe.DataQuanta.
//!   read_text(t)` invokes a static the same way.
//! - A method call on a value whose package type is known (a member's
//!   declared result) invokes that type's member with the receiver first;
//!   a member it lacks is refused here (sealed values). A property such as
//!   `expr.name` reads through its getter.
//! - A method call or attribute read on a receiver of unknown type selects,
//!   before any argument, a package member by the receiver's brand, the
//!   dialect's own method, or the `AttributeError` ([`Selection`]). A
//!   branded receiver takes the package member before `py.method.<m>`.
//! - A code member read as a value is a function value that invokes the
//!   member's identity: `col = dataframe.col`. A namespace, a value type,
//!   an error and an operation are never values.
//! - `except dataframe.ExecutionError` catches the error's kernel kind.

use lash_kernel_doc::{ActKind, Callee};

use std::collections::BTreeMap;

use lash_kernel_dialect::dispatch::{self, Brand, Filled, Selection, Target};
use lash_kernel_dialect::{
    FunctionMember, NamespaceKind, NamespaceView, Namespaces, Resolution, SpellKind, Spelling,
    TypeMember,
};
use lash_kernel_doc::{
    CallablePresentation, Expr, FunctionRepresentation, Literal, Place, RecordEntry, Rhs,
    StartConvention,
};

use lash_kernel_value::PackagePath;
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::{Ranged, TextRange};
use {lash_kernel_value::Name, lash_kernel_value::Param, lash_kernel_value::Signature};

use super::calls::arguments_error;
use super::{Buf, Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::exceptions::Classes;
use crate::scope::Ty;
use lash_ext_py_helpers::methods::Method;

const SPELLING: Spelling = Spelling::Python;

/// The exception classes a program names: the built-ins, and each package
/// error in view under its kernel kind, an `Exception`.
pub(super) fn classes(namespaces: &Namespaces) -> Classes {
    let mut classes = Classes::builtin();
    for package in namespaces.paths() {
        for (segment, export) in namespaces.exports(&package).into_iter().flatten() {
            if matches!(export.kind, NamespaceKind::Error) {
                classes.declare(&format!("{package}.{segment}"), "Exception");
            }
        }
    }
    classes
}

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

/// What a call passes, each value evaluated once in source order: the
/// receiver, the positional arguments and the keywords with their values.
pub(super) struct Passed {
    pub(super) receiver: Option<Operand>,
    pub(super) positional: Vec<Operand>,
    pub(super) keywords: Vec<(String, Operand)>,
}

impl<'a> Lowerer<'a> {
    /// The segments of a static path whose root is no variable and that a
    /// package path starts with. `getattr(dataframe, "lambda")` is the
    /// qualified escape: a segment as it is written.
    fn package_segments(&self, expr: &PyExpr) -> Option<Vec<String>> {
        let (path, bound) = self.call_path(expr)?;
        (!bound && self.namespaces.is_root(&path[0])).then_some(path)
    }

    /// Whether `expr` is a dotted path whose root is no variable and is a
    /// package root, so that a read of a member of it may be package code.
    pub(super) fn package_rooted(&self, expr: &PyExpr) -> bool {
        match expr {
            PyExpr::Name(name) => {
                let id = name.id.as_str();
                self.namespaces.is_root(id) && self.variable(id).is_none()
            }
            PyExpr::Attribute(attribute) => self.package_rooted(&attribute.value),
            _ => false,
        }
    }

    /// What `expr` names among the packages in view: `None` when it is no
    /// package path, or when it names an operation, which is a tool call's.
    ///
    /// # Errors
    ///
    /// A path under a code package that names nothing the package exports,
    /// or a segment written in another dialect's spelling, with the repair.
    pub(super) fn package_ref(&self, expr: &PyExpr) -> Lowering<Option<PackageRef<'a>>> {
        let Some(path) = self.package_segments(expr) else {
            return Ok(None);
        };
        let written: Vec<&str> = path.iter().map(String::as_str).collect();
        let spelled = path.join(".");
        let range = expr.range();
        let namespaces: &'a Namespaces = self.namespaces;
        let found = match namespaces.resolve(&written, SPELLING) {
            Resolution::Operation { .. } | Resolution::Deferred { .. } => return Ok(None),
            Resolution::Namespace => PackageRef::Namespace(spelled),
            Resolution::Export {
                package,
                segment,
                export,
                member,
            } => {
                let name = lash_kernel_value::QualifiedName {
                    package: package.clone(),
                    member: segment.clone(),
                    type_member: None,
                };
                let brand = lash_kernel_value::Brand {
                    package: package.clone(),
                    name: segment.clone(),
                };
                match export {
                    NamespaceKind::Type(_) => {
                        self.imports.insert(lash_kernel_value::Import {
                            name,
                            kind: lash_kernel_value::ImportKind::Type { brand },
                        });
                    }
                    NamespaceKind::Error => {
                        self.imports.insert(lash_kernel_value::Import {
                            name,
                            kind: lash_kernel_value::ImportKind::Error { kind: brand },
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
                        return Err(unknown_member(
                            range,
                            format!("`{spelled}` is an operation this session does not offer"),
                            "call an operation the prompt lists".to_owned(),
                        ));
                    }
                    (_, Some((_, TypeMember::Static(member)))) => {
                        PackageRef::Callable { member, spelled }
                    }
                    (_, Some((_, TypeMember::Method(_) | TypeMember::Property(_)))) => {
                        let (owner, name) = spelled.rsplit_once('.').unwrap_or((&spelled, ""));
                        return Err(unknown_member(
                            range,
                            format!(
                                "`{name}` is a member of `{owner}` values, not of `{owner}` itself"
                            ),
                            format!("call it on a value: `value.{name}(...)`"),
                        ));
                    }
                }
            }
            Resolution::Unknown => {
                return self.unknown_path(&written, range).map_or(Ok(None), Err);
            }
        };
        Ok(Some(found))
    }

    /// The refusal of `path`, which `Namespaces::resolve` found nothing at:
    /// `None` when no code package is above it, so that a path only
    /// operations are under stays a tool call's.
    fn unknown_path(
        &self,
        path: &[&str],
        range: TextRange,
    ) -> Option<lash_kernel_dialect::Diagnostic> {
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
            SPELLING.spell(export_kind(&export.kind), segment) == written
                || loosely(segment.as_str(), written)
        });
        let Some((segment, export)) = export else {
            return Some(unknown_member(
                range,
                format!("`{package}` exports nothing named `{written}`"),
                format!("call one of the exports the prompt lists under `{package}`"),
            ));
        };
        let owner = format!(
            "{package}.{}",
            SPELLING.spell(export_kind(&export.kind), segment)
        );
        if SPELLING.spell(export_kind(&export.kind), segment) != written {
            return Some(unknown_member(
                range,
                format!("`{package}.{written}` is not how Python spells a package member"),
                format!("write `{owner}`"),
            ));
        }
        let (NamespaceKind::Type(ty), [_, member]) = (&export.kind, rest) else {
            return Some(unknown_member(
                range,
                format!("`{owner}` has no member `{}`", rest[1..].join(".")),
                format!("call `{owner}` itself"),
            ));
        };
        if let Some((name, found)) = ty
            .members
            .iter()
            .find(|(name, _)| loosely(name.as_str(), member))
        {
            return Some(match found {
                TypeMember::Static(_) => unknown_member(
                    range,
                    format!("`{owner}.{member}` is not how Python spells a package member"),
                    format!("write `{owner}.{name}`"),
                ),
                TypeMember::Method(_) | TypeMember::Property(_) => unknown_member(
                    range,
                    format!("`{name}` is a member of `{owner}` values, not of `{owner}` itself"),
                    format!("call it on a value: `value.{name}(...)`"),
                ),
            });
        }
        Some(unknown_member(
            range,
            format!("`{owner}` has no static `{member}`"),
            format!("call one of the statics the prompt lists under `{owner}`"),
        ))
    }

    /// The refusal of a namespace, a type or an error used as a value.
    pub(super) fn namespace_value(
        &self,
        spelled: &str,
        range: TextRange,
    ) -> lash_kernel_dialect::Diagnostic {
        diagnostics::with_repair(
            Code::PackageNamespaceValue,
            format!("`{spelled}` is a package namespace, not a value"),
            range,
            format!(
                "write the member's full path, `{spelled}.<member>(...)`, or bind a code member by name: `<member> = {spelled}.<member>`"
            ),
        )
    }

    /// A read of a package path as a value: a code member's function
    /// value, or a refusal. `None` when `expr` is no package path.
    pub(super) fn read_package(&mut self, expr: &PyExpr) -> Option<Lowering<Operand>> {
        if let Some(path) = self.package_segments(expr) {
            let written: Vec<&str> = path.iter().map(String::as_str).collect();
            if let Resolution::Operation { .. } = self.namespaces.resolve(&written, SPELLING) {
                let spelled = path.join(".");
                return Some(Err(diagnostics::with_repair(
                    Code::OperationValue,
                    format!(
                        "`{spelled}` is an operation, which is called, never passed as a value"
                    ),
                    expr.range(),
                    format!("call it where its result is needed: `await {spelled}(...)`"),
                )));
            }
        }
        let found = match self.package_ref(expr) {
            Ok(found) => found?,
            Err(refused) => return Some(Err(refused)),
        };
        Some(match found {
            PackageRef::Callable { member, spelled } => {
                self.member_value(member, &spelled, expr.range())
            }
            PackageRef::Namespace(spelled)
            | PackageRef::Type { spelled }
            | PackageRef::Error { spelled, .. } => {
                Err(self.namespace_value(&spelled, expr.range()))
            }
        })
    }

    /// A call whose callee is a package path: `None` when it is none.
    pub(super) fn call_package(
        &mut self,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Option<Lowering<Operand>> {
        let found = match self.package_ref(&call.func) {
            Ok(found) => found?,
            Err(refused) => return Some(Err(refused)),
        };
        Some(match found {
            PackageRef::Callable { member, spelled } => {
                self.call_member(None, member, &spelled, call, awaited)
            }
            PackageRef::Namespace(spelled)
            | PackageRef::Type { spelled }
            | PackageRef::Error { spelled, .. } => Err(diagnostics::with_repair(
                Code::PackageNamespaceValue,
                format!("`{spelled}` is a package namespace, not a function"),
                call.func.range(),
                format!("call one of its members, `{spelled}.<member>(...)`"),
            )),
        })
    }

    /// The kernel kind of the package error `expr` names, if it names one.
    pub(super) fn package_error(&self, expr: &PyExpr) -> Option<String> {
        match self.package_ref(expr) {
            Ok(Some(PackageRef::Error { kind, .. })) => Some(kind),
            _ => None,
        }
    }

    /// The type an operand of a package value of `brand` has.
    fn package_ty(&mut self, brand: Brand) -> Ty {
        let at = match self.brands.iter().position(|known| *known == brand) {
            Some(at) => at,
            None => {
                self.brands.push(brand);
                self.brands.len() - 1
            }
        };
        Ty::Package(at)
    }

    /// The package value type an operand is known to hold.
    pub(super) fn brand_of(&self, ty: Ty) -> Option<Brand> {
        match ty {
            Ty::Package(at) => self.brands.get(at).cloned(),
            _ => None,
        }
    }

    /// What a call of `member` gives, as far as the front end uses it: a
    /// package value of the type its declared result names.
    fn member_ty(&mut self, member: &FunctionMember) -> Ty {
        match Brand::of_type(&member.signature.result) {
            Some(brand) => self.package_ty(brand),
            None => Ty::Unknown,
        }
    }

    /// `receiver.name(...)` where the receiver's package type is known:
    /// the type's method, or a refusal when the type declares none.
    pub(super) fn call_typed_member(
        &mut self,
        receiver: Operand,
        brand: &Brand,
        name: &str,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        let owner = type_spelling(brand);
        match self.declared_member(brand, name, call.func.range())? {
            TypeMember::Method(member) => {
                self.call_member(Some(receiver), member, name, call, awaited)
            }
            TypeMember::Property(_) => Err(unknown_member(
                call.func.range(),
                format!("`{name}` is a property of `{owner}` values, which is read, never called"),
                format!("read it: `value.{name}`"),
            )),
            TypeMember::Static(_) => Err(static_on_value(&owner, name, call.func.range())),
        }
    }

    /// `receiver.name` read where the receiver's package type is known: a
    /// property's getter, or a refusal.
    pub(super) fn read_typed_member(
        &mut self,
        receiver: Operand,
        brand: &Brand,
        name: &str,
        range: TextRange,
    ) -> Lowering<Operand> {
        let owner = type_spelling(brand);
        match self.declared_member(brand, name, range)? {
            TypeMember::Property(member) => self.invoke_member(member, &[receiver], false),
            TypeMember::Method(_) => Err(unknown_member(
                range,
                format!("`{name}` is a method of `{owner}` values, which is called, never read"),
                format!("call it: `value.{name}(...)`"),
            )),
            TypeMember::Static(_) => Err(static_on_value(&owner, name, range)),
        }
    }

    /// The member `name` that values of `brand` declare: a package value
    /// is sealed, so any other name is refused.
    fn declared_member(
        &self,
        brand: &Brand,
        name: &str,
        range: TextRange,
    ) -> Lowering<&'a TypeMember> {
        let owner = type_spelling(brand);
        let namespaces: &'a Namespaces = self.namespaces;
        let Some(ty) = brand.value_type(namespaces) else {
            return Err(unknown_member(
                range,
                format!("`{owner}` is no value type of the packages in view"),
                "rebuild the value with the package's current members".to_owned(),
            ));
        };
        if let Some(member) = SPELLING
            .unspell(SpellKind::Member, name)
            .and_then(|segment| ty.members.get(&segment))
        {
            return Ok(member);
        }
        let repair = match ty
            .members
            .keys()
            .find(|known| loosely(known.as_str(), name))
        {
            Some(known) => format!("write `{known}`"),
            None => format!(
                "use one of the members the prompt lists for `{owner}`; a package value has no other fields"
            ),
        };
        Err(unknown_member(
            range,
            format!("values of `{owner}` have no member `{name}`"),
            repair,
        ))
    }

    /// Whether a package value may reach a receiver of unknown type: some
    /// package in view exports a value type, so a method call or attribute
    /// read on such a receiver selects, and raises on a package value
    /// without the member (sealed values).
    pub(super) fn seals(&self) -> bool {
        !dispatch::typed_packages(self.namespaces).is_empty()
    }

    /// The package members a method call or attribute read on a receiver
    /// of unknown type may select: methods for a call, properties for a
    /// read.
    pub(super) fn selectable(&self, name: &str, call: bool) -> Vec<Target<'a>> {
        let Some(segment) = SPELLING.unspell(SpellKind::Member, name) else {
            return Vec::new();
        };
        let namespaces: &'a Namespaces = self.namespaces;
        dispatch::targets(namespaces, &segment)
            .into_iter()
            .filter(|target| match target.member {
                TypeMember::Method(_) => call,
                TypeMember::Property(_) => !call,
                TypeMember::Static(_) => false,
            })
            .collect()
    }

    /// Selects, before anything else runs, what `receiver.name` is on a
    /// receiver of unknown type: a package target by the receiver's brand,
    /// the dialect's own reading, or the `AttributeError` for a package
    /// value with no such member, raised at once. Gives the selection's
    /// temporary and the `same` it tests with.
    fn select(
        &mut self,
        receiver: &Operand,
        name: &str,
        targets: &[Target<'a>],
    ) -> Lowering<(Operand, lash_kernel_value::FunctionId)> {
        let brand = self.invoke(
            dispatch::BRAND_OF,
            std::slice::from_ref(receiver),
            Ty::Unknown,
        )?;
        let brand = self.pin(brand);
        let same = self.function("same")?;
        let packages = dispatch::typed_packages(self.namespaces);
        let selected = Selection::select(&brand.expr, targets, &packages, same);
        let selected = self.let_rhs(Rhs::Expr(selected), Ty::Unknown);
        let raise = self.block(|this| {
            this.invoke_do("py.no_attribute", &[receiver.clone(), Operand::text(name)])
        })?;
        self.emit_if(
            Selection::Raise.test(selected.expr.clone(), same),
            raise,
            Buf::default(),
        );
        Ok((selected, same))
    }

    /// Runs `branch` once per outcome of a selection, each in a block of
    /// its own that stores what it gives into one result: `None` for the
    /// dialect's own reading, then each target's member.
    fn branches(
        &mut self,
        selected: &Operand,
        same: lash_kernel_value::FunctionId,
        targets: &[Target<'a>],
        mut branch: impl FnMut(&mut Self, Option<usize>) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        let result = self.let_rhs(Rhs::Expr(Expr::Literal(Literal::Absent)), Ty::Unknown);
        let Expr::Variable(variable) = result.expr.clone() else {
            unreachable!("a temporary is a variable");
        };
        let place = Place::Variable(variable);
        let mut otherwise = self.block(|this| {
            let value = branch(this, None)?;
            this.store(place.clone(), value);
            Ok(())
        })?;
        for at in (0..targets.len()).rev() {
            let chosen = self.block(|this| {
                let value = branch(this, Some(at))?;
                this.store(place.clone(), value);
                Ok(())
            })?;
            let test = Selection::Package(at).test(selected.expr.clone(), same);
            otherwise = self.block(|this| {
                this.emit_if(test, chosen, otherwise);
                Ok(())
            })?;
        }
        self.append(otherwise);
        Ok(result)
    }

    /// `receiver.name(...)` on a receiver of unknown type that a package
    /// method may answer: selection, then the arguments once, in source
    /// order, then the selected call: the target's member, or `method`,
    /// the dialect's own, or the `AttributeError` when it has none. A
    /// branch whose arguments do not fit it raises `TypeError`; when none
    /// fits, the call is refused here.
    pub(super) fn select_call(
        &mut self,
        receiver: Operand,
        name: &str,
        method: Option<&'static Method>,
        targets: &[Target<'a>],
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        self.no_stars(&call.arguments)?;
        if !awaited
            && let Some(target) = targets
                .iter()
                .find(|target| target.member.member().asynchronous)
        {
            return Err(not_awaited(self, &target.brand, name, call));
        }
        let receiver = self.pin(receiver);
        let (selected, same) = self.select(&receiver, name, targets)?;
        let passed = self.passed(None, &call.arguments)?;
        let positional: Vec<Operand> = passed
            .positional
            .into_iter()
            .map(|value| self.pin(value))
            .collect();
        let keywords: Vec<(String, Operand)> = passed
            .keywords
            .into_iter()
            .map(|(keyword, value)| (keyword, self.pin(value)))
            .collect();
        let ordinary = method.map(|method| {
            self.method_slots(
                method,
                receiver.clone(),
                positional.clone(),
                &keywords,
                call,
            )
        });
        let plans: Vec<Result<Vec<Operand>, String>> = targets
            .iter()
            .map(|target| {
                member_slots(
                    target.member.member(),
                    Some(receiver.clone()),
                    name,
                    positional.clone(),
                    keywords.clone(),
                )
            })
            .collect();
        if !matches!(ordinary, Some(Ok(_))) && plans.iter().all(Result::is_err) {
            return Err(match (ordinary, plans.into_iter().find_map(Result::err)) {
                (Some(Err(refused)), _) => refused,
                (_, Some(message)) => arguments_error(message, call.range()),
                _ => self.method_refusal(name, call),
            });
        }
        self.branches(&selected, same, targets, |this, at| match at {
            Some(at) => match &plans[at] {
                Ok(slots) => this.invoke_member(targets[at].member.member(), slots, awaited),
                Err(message) => this.fail_type(message),
            },
            None => match &ordinary {
                Some(Ok(slots)) => {
                    let value = this.invoke(
                        &format!("py.method.{name}"),
                        slots,
                        method.map_or(Ty::Unknown, |method| method.result.into()),
                    )?;
                    if awaited {
                        return this.await_via("py.await", value);
                    }
                    Ok(value)
                }
                Some(Err(refused)) => this.fail_type(&refused.message),
                None => {
                    this.invoke_do("py.no_attribute", &[receiver.clone(), Operand::text(name)])?;
                    Ok(Operand::none())
                }
            },
        })
    }

    /// `receiver.name` on a receiver of unknown type that a package
    /// property may answer: selection, then its declared getter or the
    /// dialect's shared safe attribute lookup.
    pub(super) fn select_read(
        &mut self,
        receiver: Operand,
        name: &str,
        targets: &[Target<'a>],
    ) -> Lowering<Operand> {
        let receiver = self.pin(receiver);
        let (selected, same) = self.select(&receiver, name, targets)?;
        self.branches(&selected, same, targets, |this, at| match at {
            Some(at) => this.invoke_member(
                targets[at].member.member(),
                std::slice::from_ref(&receiver),
                false,
            ),
            None => this.invoke(
                "py.getattr",
                &[receiver.clone(), Operand::text(name)],
                Ty::Unknown,
            ),
        })
    }

    /// Raises the `TypeError` a call whose arguments do not fit raises.
    fn fail_type(&mut self, message: &str) -> Lowering<Operand> {
        self.invoke_do(
            "py.fail",
            &[Operand::text("TypeError"), Operand::text(message)],
        )?;
        Ok(Operand::none())
    }

    /// The function value of a code member: a closure whose parameters are
    /// the member's slots and whose body invokes its identity. A call that
    /// leaves out a slot that does not admit `absent` raises `TypeError`.
    pub(super) fn member_value(
        &mut self,
        member: &'a FunctionMember,
        spelled: &str,
        range: TextRange,
    ) -> Lowering<Operand> {
        let name = spelled.rsplit('.').next().unwrap_or(spelled);
        if member.rest {
            return Err(unknown_member(
                range,
                format!(
                    "`{spelled}` takes any number of arguments, which a Python function value cannot"
                ),
                format!("call it by its path: `{spelled}(...)`"),
            ));
        }
        let kinds = dispatch::slots(member, false);
        let params: Vec<Name> = kinds.iter().map(|_| self.fresh("arg")).collect();
        let block = self.block(|this| {
            for (param, slot) in params.iter().zip(&kinds) {
                if dispatch::admits_absent(slot.ty) {
                    continue;
                }
                let missing = this.native(
                    "same",
                    vec![
                        Expr::Variable(param.clone()),
                        Expr::Literal(Literal::Absent),
                    ],
                )?;
                let message = format!("{name}() missing required argument: '{}'", slot.name);
                let raise = this.block(|this| this.fail_type(&message).map(drop))?;
                this.emit_if(missing, raise, Buf::default());
            }
            let slots: Vec<Operand> = params
                .iter()
                .map(|param| Operand::temp(param.clone(), Ty::Unknown))
                .collect();
            let value = this.invoke_member(member, &slots, false)?;
            this.return_value(value);
            Ok(())
        })?;
        // The value states the member's slots, so that it may cross to a
        // runner as a callback.
        let start = CallablePresentation {
            signature: Signature {
                params: kinds
                    .iter()
                    .map(|slot| Param {
                        name: Name::new(slot.name),
                        ty: slot.ty.clone(),
                        optional: dispatch::admits_absent(slot.ty),
                    })
                    .collect(),
                result: member.signature.result.clone(),
            },
            representation: FunctionRepresentation::Bare,
            convention: StartConvention::Direct,
        };
        let metadata = kinds
            .iter()
            .map(|slot| {
                Expr::Tuple(vec![
                    Expr::Literal(Literal::Text(slot.name.to_string())),
                    Expr::Literal(Literal::Int(1.into())),
                    Expr::Literal(Literal::Bool(dispatch::admits_absent(slot.ty))),
                ])
            })
            .collect();
        self.wrap_python_callable(params, block, metadata, Some(start), name, false)
    }

    /// The arguments of a call, evaluated once in source order after
    /// `receiver`, which was evaluated before them: pinned when a later
    /// argument's statements could change it.
    pub(super) fn passed(
        &mut self,
        receiver: Option<Operand>,
        arguments: &ast::Arguments,
    ) -> Lowering<Passed> {
        let (buf, given) = self.aside(|this| this.given(None, arguments))?;
        let receiver = receiver.map(|receiver| {
            if buf.is_empty() && arguments.keywords.is_empty() {
                receiver
            } else {
                self.pin(receiver)
            }
        });
        self.append(buf);
        let keywords = super::calls::keyword_names(arguments)
            .zip(given.keywords)
            .collect();
        Ok(Passed {
            receiver,
            positional: given.positional,
            keywords,
        })
    }

    /// A call of a member, its receiver and arguments evaluated once in
    /// source order and its slots filled by the canonical convention.
    fn call_member(
        &mut self,
        receiver: Option<Operand>,
        member: &'a FunctionMember,
        spelled: &str,
        call: &ast::ExprCall,
        awaited: bool,
    ) -> Lowering<Operand> {
        self.no_stars(&call.arguments)?;
        if member.asynchronous && !awaited {
            return Err(diagnostics::with_repair(
                Code::CoroutineNotAwaited,
                format!("`{spelled}` is a coroutine method called without `await`"),
                call.range(),
                format!("write `await {}`", self.text(call.range())),
            ));
        }
        let passed = self.passed(receiver, &call.arguments)?;
        let slots = member_slots(
            member,
            passed.receiver,
            spelled,
            passed.positional,
            passed.keywords,
        )
        .map_err(|message| arguments_error(message, call.range()))?;
        self.invoke_member(member, &slots, awaited)
    }

    /// Invokes a member's identity over its filled slots. Awaiting a
    /// member that does not wait raises Python's `TypeError`, as awaiting
    /// any other value that is no awaitable does.
    fn invoke_member(
        &mut self,
        member: &FunctionMember,
        slots: &[Operand],
        awaited: bool,
    ) -> Lowering<Operand> {
        let ty = self.member_ty(member);
        let slot = self.imports.code(
            member.name.clone(),
            member.signature.clone(),
            member.convention,
        );
        let value = Operand::inline(
            Expr::Act(Box::new(lash_kernel_doc::ActExpr {
                kind: ActKind::Call(Callee::Import(slot)),
                args: slots.iter().map(|value| value.expr.clone()).collect(),
            })),
            ty,
        );
        if awaited && !member.asynchronous {
            return self.await_via("py.await", value);
        }
        Ok(value)
    }
}

/// A member's slots from what a call passes: the receiver first, each
/// positional argument to its slot, each keyword to its field of the one
/// options slot that has it, `absent` for a slot the call leaves out, and
/// a rest slot's arguments as one list. The error is the message of the
/// `TypeError` the call raises, or is refused with, when they do not fit.
fn member_slots(
    member: &FunctionMember,
    receiver: Option<Operand>,
    spelled: &str,
    positional: Vec<Operand>,
    keywords: Vec<(String, Operand)>,
) -> Result<Vec<Operand>, String> {
    let names: Vec<&str> = keywords.iter().map(|(name, _)| name.as_str()).collect();
    let placed = dispatch::keyword_slots(member, receiver.is_some(), spelled, &names)
        .map_err(|refused| refused.to_string())?;
    let mut records: BTreeMap<usize, Vec<RecordEntry>> = BTreeMap::new();
    for (slot, (field, value)) in placed.into_iter().zip(keywords) {
        records.entry(slot).or_default().push(RecordEntry {
            field,
            value: value.expr,
        });
    }
    let records = records
        .into_iter()
        .map(|(slot, fields)| (slot, Operand::inline(Expr::Record(fields), Ty::Unknown)))
        .collect();
    let filled = dispatch::fill_placed(member, receiver.is_some(), spelled, positional, records)
        .map_err(|refused| refused.to_string())?;
    let mut slots: Vec<Operand> = receiver.into_iter().collect();
    for filled in filled {
        slots.push(match filled {
            Filled::One(value) => value,
            Filled::Absent => Operand::literal(Literal::Absent, Ty::Unknown),
            Filled::Rest(values) => Operand::inline(
                Expr::List(values.into_iter().map(|value| value.expr).collect()),
                Ty::List,
            ),
        });
    }
    Ok(slots)
}

fn unknown_member(
    range: TextRange,
    message: String,
    repair: String,
) -> lash_kernel_dialect::Diagnostic {
    diagnostics::with_repair(Code::PackageMemberUnknown, message, range, repair)
}

fn static_on_value(owner: &str, name: &str, range: TextRange) -> lash_kernel_dialect::Diagnostic {
    unknown_member(
        range,
        format!("`{name}` is a static of `{owner}`, not a member of its values"),
        format!("call `{owner}.{name}(...)`"),
    )
}

/// The refusal of a call of an asynchronous member that is not awaited.
fn not_awaited(
    lowerer: &Lowerer<'_>,
    brand: &Brand,
    name: &str,
    call: &ast::ExprCall,
) -> lash_kernel_dialect::Diagnostic {
    diagnostics::with_repair(
        Code::CoroutineNotAwaited,
        format!(
            "`{name}` is a coroutine method of `{}` values called without `await`",
            type_spelling(brand)
        ),
        call.range(),
        format!("write `await {}`", lowerer.text(call.range())),
    )
}

/// The Python spelling of a value type's path.
fn type_spelling(brand: &Brand) -> String {
    format!(
        "{}.{}",
        brand.package_text(),
        SPELLING.spell(SpellKind::TypeName, &brand.ty)
    )
}

/// How Python spells an export of this kind.
fn export_kind(export: &NamespaceKind) -> SpellKind {
    match export {
        NamespaceKind::Type(_) | NamespaceKind::Error => SpellKind::TypeName,
        NamespaceKind::Operation(_) | NamespaceKind::Function(_) => SpellKind::Member,
    }
}

/// Whether `written` is the canonical `segment` in some spelling: the
/// same letters, cased or separated otherwise (`readText`, `readtext`).
fn loosely(segment: &str, written: &str) -> bool {
    let letters = |text: &str| {
        text.chars()
            .filter(|c| *c != '_')
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    letters(segment) == letters(written)
}

impl<'a> Lowerer<'a> {
    /// Bind a context protocol member through the same declared package
    /// identities as a method call, or through a guest dict's explicit table.
    pub(super) fn context_member(
        &mut self,
        receiver: Operand,
        name: &str,
        asynchronous: bool,
    ) -> Lowering<Operand> {
        // Package segments have no leading/trailing underscores. The protocol
        // projects Python's dunder spelling to the canonical member name.
        let targets = self.selectable(name.trim_matches('_'), true);
        let receiver = self.pin(receiver);
        let (selected, same) = self.select(&receiver, name, &targets)?;
        self.branches(&selected, same, &targets, |this, at| {
            let Some(at) = at else {
                return this.invoke(
                    "py.context_member",
                    &[receiver.clone(), Operand::text(name)],
                    Ty::Unknown,
                );
            };
            let member = targets[at].member.member();
            let params: Vec<Name> = member
                .signature
                .params
                .iter()
                .skip(1)
                .map(|_| this.temp())
                .collect();
            let body = this.block(|this| {
                let mut args = vec![receiver.clone()];
                args.extend(
                    params
                        .iter()
                        .map(|name| Operand::temp(name.clone(), Ty::Unknown)),
                );
                let value = this.invoke_member(member, &args, asynchronous)?;
                this.return_value(value);
                Ok(())
            })?;
            let metadata = member
                .signature
                .params
                .iter()
                .skip(1)
                .map(|param| {
                    Expr::Tuple(vec![
                        Expr::Literal(Literal::Text(param.name.to_string())),
                        Expr::Literal(Literal::Int(1.into())),
                        Expr::Literal(Literal::Bool(param.optional)),
                    ])
                })
                .collect();
            this.wrap_python_callable(params, body, metadata, None, name, false)
        })
    }
}
