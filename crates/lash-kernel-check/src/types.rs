//! Relations between types, as sets of kernel values.
//!
//! Both relations err toward silence: [`disjoint`] holds only when no value
//! is of both types and [`subtype`] only when every value of the first is of
//! the second, so a refusal built on either is never a false one.

use lash_kernel_doc::{MapType, Param, RecordType, Signature, Type};

/// A union of `members`, in the shape validation accepts: flat, without
/// repeats, and not a union at all when one type says it.
pub(crate) fn union(members: impl IntoIterator<Item = Type>) -> Type {
    let mut flat: Vec<Type> = Vec::new();
    for member in members {
        let parts = match member {
            Type::Union(parts) => parts,
            other => vec![other],
        };
        for part in parts {
            if part == Type::Any {
                return Type::Any;
            }
            if !flat.contains(&part) {
                flat.push(part);
            }
        }
    }
    match flat.len() {
        0 => Type::Any,
        1 => flat.remove(0),
        _ => Type::Union(flat),
    }
}

/// The type every member of a collection literal has: the members' common
/// type when they agree, `Any` otherwise.
pub(crate) fn common(members: impl IntoIterator<Item = Type>) -> Type {
    let mut members = members.into_iter();
    let Some(first) = members.next() else {
        return Type::Any;
    };
    if members.all(|member| member == first) {
        first
    } else {
        Type::Any
    }
}

/// What a variable that held a value of type `ty` still holds later: the
/// value's kind, with the contents of every mutable object left open, since
/// any statement in between may have changed them.
pub(crate) fn stable(ty: &Type) -> Type {
    match ty {
        Type::List(_) => Type::List(Box::new(Type::Any)),
        Type::Set(_) => Type::Set(Box::new(Type::Any)),
        Type::Map(_) => Type::Map(Box::new(MapType {
            key: Type::Any,
            value: Type::Any,
        })),
        Type::Record(_) => Type::Record(RecordType {
            fields: Vec::new(),
            rest: Some(Box::new(Type::Any)),
        }),
        Type::Tuple(members) => Type::Tuple(members.iter().map(stable).collect()),
        Type::Union(members) => union(members.iter().map(stable)),
        Type::Task(result) => Type::Task(Box::new(stable(result))),
        other => other.clone(),
    }
}

/// A function type that says nothing but how many arguments it names.
pub(crate) fn function_of(params: &[lash_kernel_doc::Name]) -> Type {
    Type::Function(Box::new(Signature {
        params: params
            .iter()
            .map(|name| Param {
                name: name.clone(),
                ty: Type::Any,
                optional: true,
            })
            .collect(),
        result: Type::Any,
    }))
}

/// No value is of both types.
pub(crate) fn disjoint(a: &Type, b: &Type) -> bool {
    match (a, b) {
        (Type::Any, _) | (_, Type::Any) => false,
        (Type::Union(members), other) | (other, Type::Union(members)) => {
            members.iter().all(|member| disjoint(member, other))
        }
        (Type::Number, Type::Int | Type::Float | Type::Number)
        | (Type::Int | Type::Float, Type::Number)
        | (Type::Text, Type::Enum(_))
        | (Type::Enum(_), Type::Text) => false,
        (Type::Enum(a), Type::Enum(b)) => !a.iter().any(|member| b.contains(member)),
        (Type::Tuple(a), Type::Tuple(b)) => {
            a.len() != b.len() || a.iter().zip(b).any(|(a, b)| disjoint(a, b))
        }
        (Type::Handle(a), Type::Handle(b)) => a != b,
        (Type::Record(a), Type::Record(b)) => records_disjoint(a, b) || records_disjoint(b, a),
        // An empty collection is of every element type, and nothing is
        // inferred about what a function or a task gives.
        (Type::List(_), Type::List(_))
        | (Type::Set(_), Type::Set(_))
        | (Type::Map(_), Type::Map(_))
        | (Type::Function(_), Type::Function(_))
        | (Type::Task(_), Type::Task(_)) => false,
        (a, b) => a != b,
    }
}

/// `a` requires a field `b` cannot have, or cannot have with that type.
fn records_disjoint(a: &RecordType, b: &RecordType) -> bool {
    a.fields
        .iter()
        .filter(|field| !field.optional)
        .any(
            |field| match b.fields.iter().find(|other| other.name == field.name) {
                Some(other) => disjoint(&field.ty, &other.ty),
                None => b
                    .rest
                    .as_deref()
                    .is_none_or(|rest| disjoint(&field.ty, rest)),
            },
        )
}

/// Every value of `sub` is a value of `sup`.
pub(crate) fn subtype(sub: &Type, sup: &Type) -> bool {
    match (sub, sup) {
        (_, Type::Any) => true,
        (Type::Union(members), _) => members.iter().all(|member| subtype(member, sup)),
        (_, Type::Union(members)) => members.iter().any(|member| subtype(sub, member)),
        (Type::Int | Type::Float, Type::Number) | (Type::Enum(_), Type::Text) => true,
        (Type::Enum(sub), Type::Enum(sup)) => sub.iter().all(|member| sup.contains(member)),
        (Type::Tuple(sub), Type::Tuple(sup)) => {
            sub.len() == sup.len() && sub.iter().zip(sup).all(|(sub, sup)| subtype(sub, sup))
        }
        (Type::List(sub), Type::List(sup))
        | (Type::Set(sub), Type::Set(sup))
        | (Type::Task(sub), Type::Task(sup)) => subtype(sub, sup),
        (Type::Map(sub), Type::Map(sup)) => {
            subtype(&sub.key, &sup.key) && subtype(&sub.value, &sup.value)
        }
        (Type::Record(sub), Type::Record(sup)) => record_subtype(sub, sup),
        (Type::Function(sub), Type::Function(sup)) => serves(sub, sup),
        (sub, sup) => sub == sup,
    }
}

fn record_subtype(sub: &RecordType, sup: &RecordType) -> bool {
    let named = sup.fields.iter().all(|field| {
        match sub.fields.iter().find(|other| other.name == field.name) {
            Some(other) => subtype(&other.ty, &field.ty) && (field.optional || !other.optional),
            // `sub` may hold the field under its rest type, or not at all.
            None => {
                field.optional
                    && sub
                        .rest
                        .as_deref()
                        .is_none_or(|rest| subtype(rest, &field.ty))
            }
        }
    });
    let extra = sub
        .fields
        .iter()
        .filter(|field| !sup.fields.iter().any(|other| other.name == field.name))
        .map(|field| &field.ty)
        .chain(sub.rest.as_deref())
        .all(|ty| sup.rest.as_deref().is_some_and(|rest| subtype(ty, rest)));
    named && extra
}

/// A function of signature `provided` can stand wherever one of signature
/// `expected` is called: it takes every argument list `expected` allows, and
/// returns only what `expected` promises.
pub(crate) fn serves(provided: &Signature, expected: &Signature) -> bool {
    provided.params.len() >= expected.params.len()
        && provided.required() <= expected.required()
        && expected
            .params
            .iter()
            .zip(&provided.params)
            .all(|(expected, provided)| subtype(&expected.ty, &provided.ty))
        && subtype(&provided.result, &expected.result)
}
