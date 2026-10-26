//! How a front end calls a package's code members (§2.4 of the host-package
//! design): member selection on a package value, and the canonical calling
//! convention that fills a member's slots. Every dialect lowers through
//! these, so a TypeScript and a Python call of one member reach one
//! identity with one slot list.
//!
//! **Selection.** A member call on a receiver whose type the front end does
//! not know selects, before any argument is evaluated, exactly one of three
//! outcomes ([`Selection`]), held in a hoisted temporary that survives a
//! wait like any variable (`K-EVAL-008`):
//! - [`Selection::Package`]: the receiver's brand names a value type of a
//!   package in view, and that type declares the member;
//! - [`Selection::Ordinary`]: the receiver is no package value, and the
//!   dialect's own member read stands;
//! - [`Selection::Raise`]: the receiver is a package value whose type lacks
//!   the member; the dialect's attribute error is raised before any
//!   argument's side effect.
//!
//! Selection is keyed by `(type, member segment)` into
//! [`ValueType::members`], so two types' members of one name keep their own
//! slot lists.
//!
//! **Slots.** A member's kernel signature is its slot list: the receiver
//! first for a method or property, then every declared slot. Every slot is
//! required; an optional one admits `absent`, which fills it when the call
//! leaves it out. A rest slot is one `List` slot that gathers every
//! argument from its position on.

use lash_kernel_doc::{Expr, Integer, Literal, Member as Place};

use {lash_kernel_value::FunctionId, lash_kernel_value::Signature, lash_kernel_value::Type};

use crate::exports::{
    BRAND_FIELD, FunctionMember, NamespaceKind, NamespaceView, TypeMember, ValueType,
};
use {lash_kernel_value::PackagePath, lash_kernel_value::Segment};

/// The library function a front end reads a value's brand with: a text
/// pair, or `absent` for a value of no package type.
pub const BRAND_OF: &str = "brand.of";

/// What a package value is: the package and the value type that built it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Brand {
    pub package: PackagePath,
    pub ty: Segment,
}

impl Brand {
    /// The package's path as one dotted text, as a brand holds it.
    pub fn package_text(&self) -> String {
        self.package.to_string()
    }

    /// The value [`BRAND_OF`] gives for a value of this brand.
    pub fn value(&self) -> Expr {
        Expr::Tuple(vec![
            Expr::Literal(Literal::Text(self.package_text())),
            Expr::Literal(Literal::Text(self.ty.as_str().to_owned())),
        ])
    }

    /// The brand a kernel type states: a record whose brand field is
    /// `Tuple(Enum[<path>], Enum[<type>])`, which a member's slot or result
    /// of a package type is.
    pub fn of_type(ty: &Type) -> Option<Self> {
        let Type::Record(record) = ty else {
            return None;
        };
        let field = record
            .fields
            .iter()
            .find(|field| field.name == BRAND_FIELD && !field.optional)?;
        let Type::Tuple(parts) = &field.ty else {
            return None;
        };
        let [Type::Enum(package), Type::Enum(name)] = parts.as_slice() else {
            return None;
        };
        let ([package], [name]) = (package.as_slice(), name.as_slice()) else {
            return None;
        };
        Some(Self {
            package: PackagePath::parse(package).ok()?,
            ty: Segment::new(name.as_str()).ok()?,
        })
    }

    /// The value type this brand names among the view's exports.
    pub fn value_type<'v>(&self, view: &'v dyn NamespaceView) -> Option<&'v ValueType> {
        match &view.exports(&self.package)?.get(&self.ty)?.kind {
            NamespaceKind::Type(ty) => Some(ty),
            _ => None,
        }
    }
}

/// One way a member call on an unknown receiver may resolve: a value type
/// that declares the member, and the member it declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target<'v> {
    pub brand: Brand,
    pub member: &'v TypeMember,
}

/// The value types of `view` whose values have `member`, in brand order:
/// a method or a property. A static is the type's, never its values'.
pub fn targets<'v>(view: &'v dyn NamespaceView, member: &Segment) -> Vec<Target<'v>> {
    let mut targets = Vec::new();
    for package in view.paths() {
        let Some(exports) = view.exports(&package) else {
            continue;
        };
        for (name, export) in exports {
            let NamespaceKind::Type(ty) = &export.kind else {
                continue;
            };
            match ty.members.get(member) {
                Some(found @ (TypeMember::Method(_) | TypeMember::Property(_))) => {
                    targets.push(Target {
                        brand: Brand {
                            package: package.clone(),
                            ty: name.clone(),
                        },
                        member: found,
                    });
                }
                Some(TypeMember::Static(_)) | None => {}
            }
        }
    }
    targets.sort_by(|left, right| left.brand.cmp(&right.brand));
    targets
}

/// The packages of `view` that export a value type: a value whose brand
/// names one of them is a package value.
pub fn typed_packages(view: &dyn NamespaceView) -> Vec<PackagePath> {
    view.paths()
        .into_iter()
        .filter(|package| {
            view.exports(package).is_some_and(|exports| {
                exports
                    .values()
                    .any(|export| matches!(export.kind, NamespaceKind::Type(_)))
            })
        })
        .collect()
}

/// The outcome of selecting a member on an unknown receiver, before its
/// arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The target at this position of the [`targets`] the selection tested.
    Package(usize),
    /// No package value: the dialect's own member read.
    Ordinary,
    /// A package value without the member.
    Raise,
}

impl Selection {
    /// The value a selection's temporary holds.
    pub fn literal(self) -> Literal {
        let tag: usize = match self {
            Self::Ordinary => 0,
            Self::Raise => 1,
            Self::Package(at) => 2 + at,
        };
        Literal::Int(Integer::new(tag))
    }

    /// The test that a selection's temporary holds this outcome, by the
    /// kernel's `same`.
    pub fn test(self, selected: Expr, same: FunctionId) -> Expr {
        Expr::Call {
            function: same,
            args: vec![selected, Expr::Literal(self.literal())],
        }
    }

    /// The selection, as an expression over a receiver's brand that the
    /// front end has already read with [`BRAND_OF`] into a variable. It
    /// reads nothing else, calls only `same`, and so cannot raise or wait:
    /// a front end binds it to a temporary before any argument.
    pub fn select(
        brand: &Expr,
        targets: &[Target<'_>],
        packages: &[PackagePath],
        same: FunctionId,
    ) -> Expr {
        let is = |value: Expr| Expr::Call {
            function: same,
            args: vec![brand.clone(), value],
        };
        let tag = |selection: Self| Expr::Literal(selection.literal());
        let mut chain = tag(Self::Ordinary);
        for package in packages.iter().rev() {
            let named = Expr::Call {
                function: same,
                args: vec![
                    Expr::Member(Box::new(Place::Index {
                        target: brand.clone(),
                        index: Expr::Literal(Literal::Int(Integer::from(0))),
                    })),
                    Expr::Literal(Literal::Text(package.to_string())),
                ],
            };
            chain = Expr::If {
                cond: Box::new(named),
                then: Box::new(tag(Self::Raise)),
                r#else: Box::new(chain),
            };
        }
        for (at, target) in targets.iter().enumerate().rev() {
            chain = Expr::If {
                cond: Box::new(is(target.brand.value())),
                then: Box::new(tag(Self::Package(at))),
                r#else: Box::new(chain),
            };
        }
        Expr::If {
            cond: Box::new(is(Expr::Literal(Literal::Absent))),
            then: Box::new(tag(Self::Ordinary)),
            r#else: Box::new(chain),
        }
    }
}

/// How a front end adapts a value it passes to a slot, read off the slot's
/// declared type. Kinds never convert implicitly (`K-VAL-001`), and a
/// callback is a bare kernel closure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adapt {
    /// As it is.
    Plain,
    /// An integer: a dialect whose numbers may be floats adapts them by a
    /// checked integral conversion that raises `type_error`.
    Int,
    /// A bare closure of this signature.
    Function(Signature),
}

impl Adapt {
    pub fn of(ty: &Type) -> Self {
        match ty {
            Type::Int => Self::Int,
            Type::Function(signature) => Self::Function((**signature).clone()),
            Type::Union(members) => {
                // An optional slot of one kind adapts as that kind; absent
                // passes as it is.
                let mut kinds = members.iter().filter(|ty| **ty != Type::Absent);
                match (kinds.next(), kinds.next()) {
                    (Some(only), None) => Self::of(only),
                    _ => Self::Plain,
                }
            }
            _ => Self::Plain,
        }
    }
}

/// One slot a call fills: its position in the member's signature, its
/// declared type and how a front end adapts what it passes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot<'m> {
    pub position: usize,
    pub name: &'m str,
    pub ty: &'m Type,
    pub adapt: Adapt,
}

/// What a call passes to each of a member's slots.
#[derive(Clone, Debug, PartialEq)]
pub enum Filled<T> {
    /// One argument.
    One(T),
    /// The slot was left out: it admits `absent`, which fills it.
    Absent,
    /// The rest slot's arguments, gathered into one list, each adapted as
    /// the list's element type says.
    Rest(Vec<T>),
}

/// Why a call cannot fill a member's slots.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SlotRefusal {
    #[error("`{member}` takes {most} argument(s) and is called with {given}")]
    TooMany {
        member: String,
        most: usize,
        given: usize,
    },
    #[error("`{member}` needs its argument `{slot}`, which the call leaves out")]
    Missing { member: String, slot: String },
    #[error("`{member}` is given its argument `{slot}` both by position and by keyword")]
    Twice { member: String, slot: String },
}

/// Why a call's keywords cannot fill a member's options slots.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeywordRefusal {
    #[error("`{member}` has no option `{keyword}`")]
    Unknown { member: String, keyword: String },
    #[error(
        "`{keyword}` names a field of more than one options slot of `{member}`: {}",
        slots.iter().map(|slot| format!("`{slot}`")).collect::<Vec<_>>().join(" and ")
    )]
    Ambiguous {
        member: String,
        keyword: String,
        slots: Vec<String>,
    },
    #[error("`{member}` is given the option `{keyword}` twice")]
    Repeated { member: String, keyword: String },
}

/// The fields of a slot that takes options: a record of declared fields,
/// or such a record or `absent`. A package value's slot takes none.
pub fn options_fields(ty: &Type) -> Option<&[lash_kernel_value::RecordTypeField]> {
    match ty {
        Type::Record(record) if Brand::of_type(ty).is_none() && !record.fields.is_empty() => {
            Some(&record.fields)
        }
        Type::Union(members) => {
            let mut kinds = members.iter().filter(|ty| **ty != Type::Absent);
            match (kinds.next(), kinds.next()) {
                (Some(only), None) => options_fields(only),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The slot each keyword of a call fills a field of: its position among
/// [`slots`]. Keywords are sugar for an options record, so each keyword
/// names exactly one field of exactly one options slot. `name` is the
/// member as the source spells it, for a refusal.
///
/// # Errors
///
/// A keyword that names no options field, one that names a field of more
/// than one options slot, naming each, or one given twice.
pub fn keyword_slots(
    member: &FunctionMember,
    receiver: bool,
    name: &str,
    keywords: &[&str],
) -> Result<Vec<usize>, KeywordRefusal> {
    let slots = slots(member, receiver);
    let mut placed = Vec::with_capacity(keywords.len());
    for (at, keyword) in keywords.iter().enumerate() {
        if keywords[..at].contains(keyword) {
            return Err(KeywordRefusal::Repeated {
                member: name.to_owned(),
                keyword: (*keyword).to_owned(),
            });
        }
        let named: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                options_fields(slot.ty)
                    .is_some_and(|fields| fields.iter().any(|field| field.name == *keyword))
            })
            .map(|(index, _)| index)
            .collect();
        match named.as_slice() {
            [only] => placed.push(*only),
            [] => {
                return Err(KeywordRefusal::Unknown {
                    member: name.to_owned(),
                    keyword: (*keyword).to_owned(),
                });
            }
            many => {
                return Err(KeywordRefusal::Ambiguous {
                    member: name.to_owned(),
                    keyword: (*keyword).to_owned(),
                    slots: many
                        .iter()
                        .map(|index| slots[*index].name.to_owned())
                        .collect(),
                });
            }
        }
    }
    Ok(placed)
}

/// Whether a slot of this type may be left out.
pub fn admits_absent(ty: &Type) -> bool {
    match ty {
        Type::Absent | Type::Any => true,
        Type::Union(members) => members.iter().any(admits_absent),
        _ => false,
    }
}

/// The slots a call's arguments fill, after the receiver, in signature
/// order.
pub fn slots(member: &FunctionMember, receiver: bool) -> Vec<Slot<'_>> {
    member
        .signature
        .params
        .iter()
        .enumerate()
        .skip(usize::from(receiver))
        .map(|(position, param)| {
            let rest = member.rest && position + 1 == member.signature.params.len();
            let adapt = match (&param.ty, rest) {
                (Type::List(element), true) => Adapt::of(element),
                (ty, _) => Adapt::of(ty),
            };
            Slot {
                position,
                name: param.name.as_str(),
                ty: &param.ty,
                adapt,
            }
        })
        .collect()
}

/// Fills a member's slots from a call's arguments, in source order: each
/// argument to its slot, a rest slot's to one list, and `absent` into each
/// trailing slot the call leaves out. `receiver` is whether the call passes
/// a receiver, which the front end puts first itself. `name` is the
/// member as the source spells it, for a refusal.
///
/// # Errors
///
/// More arguments than slots and no rest slot, or a left-out slot that
/// does not admit `absent`.
pub fn fill<T>(
    member: &FunctionMember,
    receiver: bool,
    name: &str,
    args: Vec<T>,
) -> Result<Vec<Filled<T>>, SlotRefusal> {
    fill_placed(
        member,
        receiver,
        name,
        args,
        std::collections::BTreeMap::new(),
    )
}

/// [`fill`], with `placed` already holding the value of some slots, by
/// their position among [`slots`]: the options records a call's keywords
/// build ([`keyword_slots`]).
///
/// # Errors
///
/// As [`fill`], and a slot given both by position and in `placed`.
pub fn fill_placed<T>(
    member: &FunctionMember,
    receiver: bool,
    name: &str,
    args: Vec<T>,
    mut placed: std::collections::BTreeMap<usize, T>,
) -> Result<Vec<Filled<T>>, SlotRefusal> {
    let slots = slots(member, receiver);
    let given = args.len();
    let fixed = slots.len().saturating_sub(usize::from(member.rest));
    if !member.rest && given > fixed {
        return Err(SlotRefusal::TooMany {
            member: name.to_owned(),
            most: fixed,
            given,
        });
    }
    let mut args = args.into_iter();
    let mut filled = Vec::with_capacity(slots.len());
    for (index, slot) in slots[..fixed].iter().enumerate() {
        let value = placed.remove(&index);
        match (args.next(), value) {
            (Some(_), Some(_)) => {
                return Err(SlotRefusal::Twice {
                    member: name.to_owned(),
                    slot: slot.name.to_owned(),
                });
            }
            (Some(arg), None) | (None, Some(arg)) => filled.push(Filled::One(arg)),
            (None, None) if admits_absent(slot.ty) => filled.push(Filled::Absent),
            (None, None) => {
                return Err(SlotRefusal::Missing {
                    member: name.to_owned(),
                    slot: slot.name.to_owned(),
                });
            }
        }
    }
    if member.rest && !slots.is_empty() {
        filled.push(Filled::Rest(args.collect()));
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use {lash_kernel_value::Name, lash_kernel_value::Param, lash_kernel_value::QualifiedName};

    use super::*;

    /// Exports held in memory, by package path.
    struct HeldExports(BTreeMap<PackagePath, BTreeMap<Segment, crate::NamespaceMember>>);

    impl NamespaceView for HeldExports {
        fn exports(
            &self,
            path: &PackagePath,
        ) -> Option<&BTreeMap<Segment, crate::NamespaceMember>> {
            self.0.get(path)
        }

        fn paths(&self) -> Vec<PackagePath> {
            self.0.keys().cloned().collect()
        }
    }

    fn member(name: &str, params: &[(&str, Type)], rest: bool) -> FunctionMember {
        FunctionMember {
            name: {
                let (package, member) = name.rsplit_once('.').expect("qualified test member");
                QualifiedName {
                    package: PackagePath::parse(package).expect("test package"),
                    member: Segment::new(member).expect("test member"),
                    type_member: None,
                }
            },
            convention: lash_kernel_value::Convention::Direct,
            signature: Signature {
                params: params
                    .iter()
                    .map(|(name, ty)| Param {
                        name: Name::new(*name),
                        ty: ty.clone(),
                        optional: false,
                    })
                    .collect(),
                result: Type::Any,
            },
            asynchronous: false,
            rest,
        }
    }

    fn callback() -> Signature {
        Signature {
            params: Vec::new(),
            result: Type::Any,
        }
    }

    fn segment(text: &str) -> Segment {
        Segment::new(text).expect("canonical")
    }

    /// The canonical convention: every slot is filled, a left-out slot by
    /// `absent` only where it admits it, and a rest slot gathers every
    /// argument from its position on.
    #[test]
    fn slots_fill_by_the_canonical_convention() {
        let optional = Type::Union(vec![
            Type::Record(lash_kernel_value::RecordType {
                fields: Vec::new(),
                rest: lash_kernel_value::RecordRest::Keep(Box::new(Type::Any)),
            }),
            Type::Absent,
        ]);
        let sort = member(
            "p.q.sort",
            &[("q", Type::Any), ("keys", Type::List(Box::new(Type::Any)))],
            true,
        );
        assert_eq!(
            fill(&sort, true, "sort", vec![1, 2, 3]),
            Ok(vec![Filled::Rest(vec![1, 2, 3])])
        );
        assert_eq!(
            fill(&sort, true, "sort", Vec::<i32>::new()),
            Ok(vec![Filled::Rest(Vec::new())])
        );
        let flat_map = member(
            "p.q.flat_map",
            &[
                ("q", Type::Any),
                ("f", Type::Function(Box::new(callback()))),
                ("options", optional),
            ],
            false,
        );
        assert_eq!(
            fill(&flat_map, true, "flatMap", vec![1]),
            Ok(vec![Filled::One(1), Filled::Absent])
        );
        assert_eq!(
            slots(&flat_map, true)
                .iter()
                .map(|slot| slot.adapt.clone())
                .collect::<Vec<_>>(),
            vec![Adapt::Function(callback()), Adapt::Plain]
        );
        assert_eq!(
            fill(&flat_map, true, "flatMap", Vec::<i32>::new()),
            Err(SlotRefusal::Missing {
                member: "flatMap".into(),
                slot: "f".into(),
            })
        );
        assert_eq!(
            fill(&flat_map, true, "flatMap", vec![1, 2, 3]),
            Err(SlotRefusal::TooMany {
                member: "flatMap".into(),
                most: 2,
                given: 3,
            })
        );
    }

    /// Selection is keyed by (type, member): two types' members of one name
    /// are two targets, each its own type's, and a static is no value's.
    #[test]
    fn targets_are_keyed_by_type_and_member() {
        let ty = |members: Vec<(&str, TypeMember)>| crate::NamespaceMember {
            visibility: lash_kernel_value::Visibility::Public,
            kind: NamespaceKind::Type(ValueType {
                representation: Type::Any,
                members: members
                    .into_iter()
                    .map(|(name, member)| (segment(name), member))
                    .collect(),
            }),
        };
        let quanta_sort = TypeMember::Method(member("df.data_quanta.sort", &[], true));
        let expr_sort = TypeMember::Method(member("df.expr.sort", &[], false));
        let view = HeldExports(BTreeMap::from([(
            PackagePath::parse("df").expect("canonical"),
            BTreeMap::from([
                (
                    segment("data_quanta"),
                    ty(vec![("sort", quanta_sort.clone())]),
                ),
                (segment("expr"), ty(vec![("sort", expr_sort.clone())])),
                (
                    segment("context"),
                    ty(vec![(
                        "sort",
                        TypeMember::Static(member("df.context.sort", &[], false)),
                    )]),
                ),
                (
                    segment("error"),
                    crate::NamespaceMember {
                        visibility: lash_kernel_value::Visibility::Public,
                        kind: NamespaceKind::Error,
                    },
                ),
            ]),
        )]));
        let found = targets(&view, &segment("sort"));
        assert_eq!(
            found
                .iter()
                .map(|target| (target.brand.ty.as_str(), target.member))
                .collect::<Vec<_>>(),
            vec![("data_quanta", &quanta_sort), ("expr", &expr_sort)]
        );
        assert_eq!(
            typed_packages(&view),
            vec![PackagePath::parse("df").expect("canonical")]
        );
        let brand = Type::Record(lash_kernel_value::RecordType {
            fields: vec![lash_kernel_value::RecordTypeField {
                name: BRAND_FIELD.into(),
                ty: Type::Tuple(vec![
                    Type::Enum(vec!["df".into()]),
                    Type::Enum(vec!["expr".into()]),
                ]),
                optional: false,
            }],
            rest: lash_kernel_value::RecordRest::Keep(Box::new(Type::Any)),
        });
        assert_eq!(Brand::of_type(&brand), Some(found[1].brand.clone()));
    }
}
