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

use std::collections::BTreeMap;

use lash_kernel_doc::Type;

use crate::adapter::{BinaryOp, TypeAnnotation, TypeShape, UnaryOp};

mod facts;
mod narrow;

pub(crate) use facts::Facts;
pub(crate) use narrow::{Narrowing, narrow};

/// What a value is known or believed to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ty {
    /// Nothing is known: `any`, an unannotated parameter, a type the
    /// analysis does not model.
    Unknown,
    Undefined,
    Null,
    Bool,
    /// A number the kernel holds as a float: a literal, or the result of
    /// arithmetic.
    Float,
    /// A JavaScript number, which the kernel may hold as an integer or as a
    /// float: an annotation says no more than this.
    Number,
    Text,
    /// An array, with what its elements are.
    List(Box<Ty>),
    /// An object, with the properties its type names.
    Record(BTreeMap<String, Ty>),
    /// A function, with what a call of it gives.
    Function(Box<Ty>),
    /// One of several types, none of them unknown and none a union.
    Union(Vec<Ty>),
    /// No value yet: where the inference of a binding's type starts. It is
    /// never the type of an operand.
    Never,
}

impl Ty {
    pub(crate) fn is_number(&self) -> bool {
        matches!(self, Self::Float | Self::Number)
    }

    /// The type of a value that is one of two.
    pub(crate) fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Never, ty) | (ty, Self::Never) => ty.clone(),
            (left, right) if left == right => left.clone(),
            (left, right) if left.is_number() && right.is_number() => Self::Number,
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
            Self::Never => Self::Never,
            _ => Self::Unknown,
        }
    }

    /// What a call of this function gives.
    pub(crate) fn returned(&self) -> Self {
        match self {
            Self::Function(result) => (**result).clone(),
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
    /// (`K-EFF-005`). An `Int` is a number that may be an integer; a bare
    /// number is a float, by the policy of every TypeScript document.
    pub(crate) fn decoded(stated: &Type) -> Self {
        match stated {
            Type::Null => Self::Null,
            Type::Absent => Self::Undefined,
            Type::Bool => Self::Bool,
            Type::Int => Self::Number,
            Type::Float | Type::Number => Self::Float,
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
        TypeShape::Unknown | TypeShape::Unsupported(_) => Ty::Unknown,
        TypeShape::String | TypeShape::StringLiteral => Ty::Text,
        TypeShape::Number => Ty::Number,
        TypeShape::Boolean => Ty::Bool,
        TypeShape::Null => Ty::Null,
        TypeShape::Undefined => Ty::Undefined,
        TypeShape::Array(element) => Ty::List(Box::new(believed(element, aliases))),
        TypeShape::Object(fields) => Ty::Record(
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

/// The kernel function a binary operator is, for operands of known types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direct {
    /// `function(left, right)` over two numbers, each as a float.
    Numeric(&'static str),
    /// `function(right, left)` over two numbers, each as a float.
    NumericSwapped(&'static str),
    /// `eq(left, right)` over two numbers, each as a float; negated for
    /// `!==` and `!=`.
    NumericEqual { negated: bool },
    /// `text.concat(left, right)` over two texts.
    Concat,
}

/// The kernel function `op` is when both operands' types are known, or
/// `None` when a helper has to decide at run time.
///
/// Every function named here takes exactly the operand types it is chosen
/// for (`K-NUM-008`), so a believed type that is wrong raises `type_error`.
/// `**` stays a helper: JavaScript's `1 ** NaN` is not the kernel's.
pub(crate) fn direct_binary(op: BinaryOp, left: &Ty, right: &Ty) -> Option<Direct> {
    if left.is_number() && right.is_number() {
        return Some(match op {
            BinaryOp::Add => Direct::Numeric("num.add"),
            BinaryOp::Subtract => Direct::Numeric("num.sub"),
            BinaryOp::Multiply => Direct::Numeric("num.mul"),
            BinaryOp::Divide => Direct::Numeric("num.div"),
            BinaryOp::Remainder => Direct::Numeric("num.rem_trunc"),
            BinaryOp::Less => Direct::Numeric("num.lt"),
            BinaryOp::LessEqual => Direct::Numeric("num.le"),
            BinaryOp::Greater => Direct::NumericSwapped("num.lt"),
            BinaryOp::GreaterEqual => Direct::NumericSwapped("num.le"),
            BinaryOp::StrictEqual | BinaryOp::LooseEqual => Direct::NumericEqual { negated: false },
            BinaryOp::StrictNotEqual | BinaryOp::LooseNotEqual => {
                Direct::NumericEqual { negated: true }
            }
            _ => return None,
        });
    }
    (op == BinaryOp::Add && *left == Ty::Text && *right == Ty::Text).then_some(Direct::Concat)
}

/// What a binary operator gives, whichever way it is lowered.
pub(crate) fn binary_result(op: BinaryOp, left: &Ty, right: &Ty) -> Ty {
    match op {
        BinaryOp::Add => {
            if *left == Ty::Never || *right == Ty::Never {
                Ty::Never
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
        | BinaryOp::ShiftRightUnsigned => Ty::Float,
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
pub(crate) fn unary_result(op: UnaryOp) -> Ty {
    match op {
        UnaryOp::Plus | UnaryOp::Minus | UnaryOp::BitNot => Ty::Float,
        UnaryOp::Not => Ty::Bool,
        UnaryOp::TypeOf => Ty::Text,
        UnaryOp::Void => Ty::Undefined,
    }
}
