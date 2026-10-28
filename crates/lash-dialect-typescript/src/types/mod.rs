//! The type analysis: what the lowerer may believe a value to be.
//!
//! It is not a TypeScript checker. It reads the annotations the source
//! wrote, the literals and operators whose result has one type whatever
//! they are given, and the tests that narrow a value, and answers one
//! question per operand: is its type known? Where the answer is yes the
//! lowerer emits the kernel function for that type; where it cannot tell,
//! the answer is [`Ty::Unknown`] and the lowerer emits the helper that
//! carries JavaScript's meaning.
//!
//! A type comes from proof or from trust. Proof is a literal, an operator's
//! result, a `typeof` test, or a `let` whose every assignment gives one
//! type: such a type is what JavaScript would hold, and using it changes
//! nothing. Trust is an annotation, or the result type a tool declares:
//! `n: number` is believed. The kernel function chosen for a believed type
//! takes exactly that type, so a wrong belief raises a typed error where
//! JavaScript would have coerced. Each such place is a `TS_TYPED_*` row of
//! the crate's deviation register, `deviations.md`.
//!
//! A representation is only ever proof: [`Ty::Object`] and [`Ty::Array`]
//! say which kernel value the source built, never what it holds, and no
//! annotation gives either. A kernel value keeps its kind for its whole
//! life, so the proof survives every write through every alias; what the
//! value holds does not, and a read of it is still checked where it runs.

use std::collections::BTreeMap;

use lash_kernel_value::Type;

use crate::adapter::{BinaryOp, TypeAnnotation, TypeShape, UnaryOp};

mod entry;
mod facts;
mod narrow;
mod packages;
mod plain;

pub(crate) use facts::Facts;
pub(crate) use narrow::{Narrowing, narrow};
pub(crate) use packages::{Gives, Packages};
pub(crate) use plain::PlainRecord;

/// The array methods whose result is a new kernel list whatever array they
/// are called on: each returns a list it builds, from `[]` or by
/// `list.slice` (`helpers/array.kernel`).
pub(crate) const FRESH_LIST_METHODS: &[&str] = &["concat", "filter", "map", "slice"];

/// What a value is known or believed to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ty {
    /// Nothing is known: `any`, an unannotated parameter, a type the
    /// analysis does not model.
    Unknown,
    /// An immutable builtin token: a tuple, never a record or a list.
    Builtin,
    Undefined,
    Null,
    Bool,
    /// A number the kernel holds as a float: a literal, or the result of
    /// arithmetic.
    Float,
    /// An explicit TypeScript BigInt carries a tag around an exact kernel Integer.
    BigInt,
    /// A trusted Number annotation, without proof of the runtime kind.
    Number,
    Text,
    /// An array, with what its elements are.
    List(Box<Ty>),
    /// An object, with the properties its type names.
    Record(BTreeMap<String, Ty>),
    /// An object the source built: a kernel record, whatever fields it
    /// later gains or loses. Guest properties cannot supply the kernel tuple marker
    /// that identifies an internal branded receiver.
    Object,
    /// An array the source built, or one a built-in array method made: a
    /// kernel list, of any elements, holes among them.
    Array,
    Map,
    Set,
    /// A function, with what a call of it gives.
    Function(Box<Ty>),
    /// A function annotation, without proof that the value is a function token.
    Callable(Box<Ty>),
    /// One of several types, none of them unknown and none a union.
    Union(Vec<Ty>),
    /// A package value of this brand, as a package member's declared
    /// result or an annotation names it: a static receiver, whose members
    /// are its type's. Boxed: every operand holds a type, and lowering
    /// recurses through frames full of operands.
    Package(Box<lash_kernel_dialect::dispatch::Brand>),
    /// No value yet: where the inference of a binding's type starts. It is
    /// never the type of an operand.
    Never,
}

impl Ty {
    pub(crate) fn is_number(&self) -> bool {
        matches!(self, Self::Float | Self::Number)
    }

    /// An arithmetic result can be a Number or BigInt, but never text.
    pub(crate) fn numeric() -> Self {
        Self::Union(vec![Self::Float, Self::BigInt])
    }

    pub(crate) fn is_numeric(&self) -> bool {
        self.is_number()
            || matches!(self, Self::BigInt)
            || matches!(self, Self::Union(parts) if !parts.is_empty() && parts.iter().all(Self::is_numeric))
    }

    /// The type of a value that is one of two.
    pub(crate) fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Never, ty) | (ty, Self::Never) => ty.clone(),
            (left, right) if left == right => left.clone(),
            (left, right) if left.is_number() && right.is_number() => Self::Number,
            (left, right) if left.is_numeric() && right.is_numeric() => Self::numeric(),
            _ => Self::Unknown,
        }
    }

    /// What an element of this array is.
    pub(crate) fn element(&self) -> Self {
        match self {
            Self::List(element) => (**element).clone(),
            Self::Never => Self::Never,
            _ => Self::Unknown,
        }
    }

    /// What the property `name` of this object is.
    pub(crate) fn property(&self, name: &str) -> Self {
        match self {
            Self::Record(fields) => fields.get(name).cloned().unwrap_or(Self::Unknown),
            Self::List(_) | Self::Text if name == "length" => Self::Number,
            Self::Array if name == "length" => Self::Float,
            Self::Never => Self::Never,
            _ => Self::Unknown,
        }
    }

    /// What a call of this function gives.
    pub(crate) fn returned(&self) -> Self {
        match self {
            Self::Function(result) | Self::Callable(result) => (**result).clone(),
            Self::Never => Self::Never,
            _ => Self::Unknown,
        }
    }

    /// One of `members`: a union when they differ.
    fn one_of(members: Vec<Self>) -> Self {
        let mut flat: Vec<Self> = Vec::new();
        for member in members {
            let parts = match member {
                Self::Union(parts) => parts,
                Self::Unknown | Self::Never => return Self::Unknown,
                other => vec![other],
            };
            for part in parts {
                if !flat.contains(&part) {
                    flat.push(part);
                }
            }
        }
        match flat.len() {
            0 => Self::Unknown,
            1 => flat.remove(0),
            _ => Self::Union(flat),
        }
    }

    /// What a tool's result is, decoded as the type its `perform` states
    /// (`K-EFF-005`). Incoming integers are Numbers; only the Float boundary
    /// proves a binary64 representation before a checked Number operation.
    pub(crate) fn decoded(stated: &Type) -> Self {
        if let Some(brand) = lash_kernel_dialect::dispatch::Brand::of_type(stated) {
            return Self::Package(Box::new(brand));
        }
        match stated {
            Type::Null => Self::Null,
            Type::Absent => Self::Undefined,
            Type::Bool => Self::Bool,
            Type::Int | Type::Number => Self::Number,
            Type::Float => Self::Float,
            Type::Text | Type::Enum(_) => Self::Text,
            Type::List(element) => Self::List(Box::new(Self::decoded(element))),
            Type::Record(record) => Self::Record(
                record
                    .fields
                    .iter()
                    .map(|field| {
                        let ty = Self::decoded(&field.ty);
                        let ty = if field.optional {
                            Self::one_of(vec![ty, Self::Undefined])
                        } else {
                            ty
                        };
                        (field.name.clone(), ty)
                    })
                    .collect(),
            ),
            Type::Union(members) => Self::one_of(members.iter().map(Self::decoded).collect()),
            _ => Self::Unknown,
        }
    }

    /// Source annotations do not prove a runtime float or normalize undefined.
    pub(crate) fn annotated(ty: &Type) -> Self {
        if let Some(brand) = lash_kernel_dialect::dispatch::Brand::of_type(ty) {
            return Self::Package(Box::new(brand));
        }
        match ty {
            Type::Function(signature) => {
                Self::Callable(Box::new(Self::annotated(&signature.result)))
            }
            Type::Number | Type::Float => Self::Number,
            Type::Int => Self::BigInt,
            Type::List(element) => Self::List(Box::new(Self::annotated(element))),
            Type::Record(record) => Self::Record(
                record
                    .fields
                    .iter()
                    .map(|field| {
                        let ty = Self::annotated(&field.ty);
                        (
                            field.name.clone(),
                            if field.optional {
                                Self::one_of(vec![ty, Self::Undefined])
                            } else {
                                ty
                            },
                        )
                    })
                    .collect(),
            ),
            Type::Union(members) => Self::one_of(members.iter().map(Self::annotated).collect()),
            _ => Self::decoded(ty),
        }
    }

    /// This type with the members of a union that `excluded` names taken
    /// out. A type that is not a union is unchanged: nothing says what else
    /// it could be.
    pub(crate) fn without(&self, excluded: &[Self]) -> Self {
        let Self::Union(members) = self else {
            return self.clone();
        };
        let kept: Vec<Self> = members
            .iter()
            .filter(|member| {
                !excluded
                    .iter()
                    .any(|gone| gone == *member || (gone.is_number() && member.is_number()))
            })
            .cloned()
            .collect();
        if kept.is_empty() {
            // The annotation said this could not happen; believe nothing.
            return Self::Unknown;
        }
        Self::one_of(kept)
    }

    /// This type, now that a test has shown the value to be a `shown`.
    pub(crate) fn shown(&self, shown: &Self) -> Self {
        if self.is_number() && shown.is_number() {
            return self.clone();
        }
        shown.clone()
    }
}

/// What a written type lets the lowerer believe. `aliases` names the
/// source's own `type` and `interface` declarations.
pub(crate) fn believed(annotation: &TypeAnnotation, aliases: &BTreeMap<String, Ty>) -> Ty {
    match &annotation.shape {
        TypeShape::Function(result) => Ty::Callable(Box::new(believed(result, aliases))),
        TypeShape::OptionalTupleElement(_)
        | TypeShape::Tuple(_)
        | TypeShape::Intersection(_)
        | TypeShape::GenericAlias { .. }
        | TypeShape::Any
        | TypeShape::Unknown
        | TypeShape::Never
        | TypeShape::Unsupported(_) => Ty::Unknown,
        TypeShape::String | TypeShape::StringLiteral(_) => Ty::Text,
        TypeShape::Number => Ty::Number,
        TypeShape::BigInt => Ty::BigInt,
        TypeShape::Boolean => Ty::Bool,
        TypeShape::Null => Ty::Null,
        TypeShape::Undefined => Ty::Undefined,
        TypeShape::Generic(name, args) => match (name.as_str(), args.as_slice()) {
            ("Array" | "ReadonlyArray", [element]) => {
                Ty::List(Box::new(believed(element, aliases)))
            }
            _ => Ty::Unknown,
        },
        TypeShape::Array(element) => Ty::List(Box::new(believed(element, aliases))),
        TypeShape::Object(fields, _) => Ty::Record(
            fields
                .iter()
                .map(|field| {
                    let ty = believed(&field.ty, aliases);
                    let ty = if field.optional {
                        Ty::one_of(vec![ty, Ty::Undefined])
                    } else {
                        ty
                    };
                    (field.name.clone(), ty)
                })
                .collect(),
        ),
        TypeShape::Union(members) => Ty::one_of(
            members
                .iter()
                .map(|member| believed(member, aliases))
                .collect(),
        ),
        TypeShape::Reference(name) => aliases.get(name).cloned().unwrap_or(Ty::Unknown),
    }
}

pub(crate) use crate::constructs::operators::Direct;

/// What a binary operator gives, whichever way it is lowered.
pub(crate) fn binary_result(op: BinaryOp, left: &Ty, right: &Ty) -> Ty {
    match op {
        BinaryOp::Add => {
            if *left == Ty::Never || *right == Ty::Never {
                Ty::Never
            } else if *left == Ty::BigInt && *right == Ty::BigInt {
                Ty::BigInt
            } else if left.is_number() && right.is_number() {
                Ty::Float
            } else if *left == Ty::Text || *right == Ty::Text {
                Ty::Text
            } else {
                Ty::Unknown
            }
        }
        BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Remainder
        | BinaryOp::Exponent
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::ShiftLeft
        | BinaryOp::ShiftRight
        | BinaryOp::ShiftRightUnsigned => {
            // An unfinished recurrence is still bottom, not evidence that a
            // BigInt or an unknown value can reach this binding.
            if *left == Ty::Never || *right == Ty::Never {
                Ty::Never
            } else if *left == Ty::BigInt && *right == Ty::BigInt {
                Ty::BigInt
            } else if left.is_number() && right.is_number() {
                Ty::Float
            } else {
                Ty::numeric()
            }
        }
        BinaryOp::StrictEqual
        | BinaryOp::StrictNotEqual
        | BinaryOp::LooseEqual
        | BinaryOp::LooseNotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual
        | BinaryOp::In
        | BinaryOp::InstanceOf => Ty::Bool,
    }
}

/// What a unary operator gives, whichever way it is lowered.
pub(crate) fn unary_result(op: UnaryOp, operand: &Ty) -> Ty {
    match op {
        UnaryOp::Plus => Ty::Float,
        UnaryOp::Minus | UnaryOp::BitNot => {
            if *operand == Ty::Never {
                Ty::Never
            } else if *operand == Ty::BigInt {
                Ty::BigInt
            } else if operand.is_number() {
                Ty::Float
            } else {
                Ty::Unknown
            }
        }
        UnaryOp::Not => Ty::Bool,
        UnaryOp::TypeOf => Ty::Text,
        UnaryOp::Void => Ty::Undefined,
    }
}
