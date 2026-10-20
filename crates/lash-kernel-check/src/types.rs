//! Relations between types, as sets of kernel values.
//!
//! Both relations err toward silence: [`disjoint`] holds only when no value
//! is of both types and subtyping succeeds only when every value of the
//! first is of the second, so a refusal built on either is never a false one.

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
        // A list or a record read as data may be given a function later.
        Type::Json => Type::Any,
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
        (Type::Json, other) | (other, Type::Json) => !admits_data(other),
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

/// Some value of `ty` holds no function, task or handle.
fn admits_data(ty: &Type) -> bool {
    !matches!(ty, Type::Function(_) | Type::Task(_) | Type::Handle(_))
}

/// Every value of `ty` is data ([`Type::Json`]): it holds no function,
/// task or handle anywhere. An error's data may hold one.
pub(crate) fn data_only(ty: &Type) -> bool {
    match ty {
        Type::Json
        | Type::Null
        | Type::Absent
        | Type::Bool
        | Type::Int
        | Type::Float
        | Type::Number
        | Type::Text
        | Type::Bytes
        | Type::Timestamp
        | Type::Enum(_) => true,
        Type::Tuple(members) | Type::Union(members) => members.iter().all(data_only),
        Type::List(item) | Type::Set(item) => data_only(item),
        Type::Map(map) => data_only(&map.key) && data_only(&map.value),
        Type::Record(record) => {
            record.fields.iter().all(|field| data_only(&field.ty))
                && record.rest.as_deref().is_none_or(data_only)
        }
        Type::Any | Type::Function(_) | Type::Task(_) | Type::Error | Type::Handle(_) => false,
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

/// A failed containment, oriented as the sub- and supertype being compared.
struct TypeMismatch {
    path: String,
    sub: Type,
    sup: Type,
}

fn differs(path: &str, sub: &Type, sup: &Type) -> TypeMismatch {
    TypeMismatch {
        path: path.into(),
        sub: sub.clone(),
        sup: sup.clone(),
    }
}

fn property(path: &str, name: &str) -> String {
    format!(
        "{path}/properties/{}",
        name.replace('~', "~0").replace('/', "~1")
    )
}

fn subtype_at(sub: &Type, sup: &Type, path: &str) -> Option<TypeMismatch> {
    match (sub, sup) {
        (_, Type::Any) => None,
        (Type::Union(members), _) => members
            .iter()
            .find_map(|member| subtype_at(member, sup, path)),
        (_, Type::Union(members)) => {
            if members
                .iter()
                .any(|member| subtype_at(sub, member, path).is_none())
            {
                None
            } else {
                Some(differs(path, sub, sup))
            }
        }
        (sub, Type::Json) if data_only(sub) => None,
        (Type::Int | Type::Float, Type::Number) | (Type::Enum(_), Type::Text) => None,
        (Type::Enum(subs), Type::Enum(sups)) if subs.iter().all(|member| sups.contains(member)) => {
            None
        }
        (Type::Tuple(subs), Type::Tuple(sups)) if subs.len() == sups.len() => subs
            .iter()
            .zip(sups)
            .enumerate()
            .find_map(|(index, (sub, sup))| subtype_at(sub, sup, &format!("{path}/items/{index}"))),
        (Type::List(sub), Type::List(sup))
        | (Type::Set(sub), Type::Set(sup))
        | (Type::Task(sub), Type::Task(sup)) => subtype_at(sub, sup, &format!("{path}/items")),
        (Type::Map(sub), Type::Map(sup)) => subtype_at(&sub.key, &sup.key, &format!("{path}/keys"))
            .or_else(|| subtype_at(&sub.value, &sup.value, &format!("{path}/values"))),
        (Type::Record(sub), Type::Record(sup)) => record_subtype_at(sub, sup, path),
        (Type::Function(sub), Type::Function(sup)) => {
            serves_at(sub, sup, path).map(|mismatch| TypeMismatch {
                path: mismatch.path,
                sub: mismatch.provided,
                sup: mismatch.expected,
            })
        }
        (sub, sup) if sub == sup => None,
        (sub, sup) => Some(differs(path, sub, sup)),
    }
}

fn record_subtype_at(sub: &RecordType, sup: &RecordType, path: &str) -> Option<TypeMismatch> {
    for field in &sup.fields {
        let at = property(path, &field.name);
        match sub.fields.iter().find(|other| other.name == field.name) {
            Some(other) => {
                if let Some(mismatch) = subtype_at(&other.ty, &field.ty, &at) {
                    return Some(mismatch);
                }
                if !field.optional && other.optional {
                    return Some(differs(
                        &at,
                        &Type::Union(vec![other.ty.clone(), Type::Absent]),
                        &field.ty,
                    ));
                }
            }
            None if !field.optional => return Some(differs(&at, &Type::Absent, &field.ty)),
            None => {
                if let Some(rest) = &sub.rest
                    && let Some(mismatch) = subtype_at(rest, &field.ty, &at)
                {
                    return Some(mismatch);
                }
            }
        }
    }
    for field in &sub.fields {
        if sup.fields.iter().any(|other| other.name == field.name) {
            continue;
        }
        let at = property(path, &field.name);
        match &sup.rest {
            Some(rest) => {
                if let Some(mismatch) = subtype_at(&field.ty, rest, &at) {
                    return Some(mismatch);
                }
            }
            None => return Some(differs(&at, &field.ty, &Type::Absent)),
        }
    }
    match (&sub.rest, &sup.rest) {
        (Some(sub), Some(sup)) => subtype_at(sub, sup, &format!("{path}/additionalProperties")),
        (Some(_), None) => Some(differs(
            &format!("{path}/additionalProperties"),
            &Type::Record(sub.clone()),
            &Type::Record(sup.clone()),
        )),
        _ => None,
    }
}

/// The same contravariant-input, covariant-result comparison produces the
/// diagnostic at its first failure; callers never diff the types afterward.
pub(crate) fn serves(
    provided: &Signature,
    expected: &Signature,
) -> Option<crate::SignatureMismatch> {
    serves_at(provided, expected, "")
}

fn serves_at(
    provided: &Signature,
    expected: &Signature,
    path: &str,
) -> Option<crate::SignatureMismatch> {
    use crate::{SignatureMismatch, SignatureSide};
    let input = format!("{path}/input");
    if provided.params.len() < expected.params.len() || provided.required() > expected.required() {
        return Some(SignatureMismatch {
            side: SignatureSide::Input,
            path: input,
            expected: Type::Function(Box::new(expected.clone())),
            provided: Type::Function(Box::new(provided.clone())),
        });
    }
    for (expected, provided) in expected.params.iter().zip(&provided.params) {
        let at = if provided.name.as_str() == "input" && expected.name.as_str() == "input" {
            input.clone()
        } else {
            property(&input, expected.name.as_str())
        };
        if let Some(mismatch) = subtype_at(&expected.ty, &provided.ty, &at) {
            return Some(SignatureMismatch {
                side: SignatureSide::Input,
                path: mismatch.path,
                expected: mismatch.sub,
                provided: mismatch.sup,
            });
        }
    }
    subtype_at(
        &provided.result,
        &expected.result,
        &format!("{path}/output"),
    )
    .map(|mismatch| SignatureMismatch {
        side: SignatureSide::Output,
        path: mismatch.path,
        expected: mismatch.sup,
        provided: mismatch.sub,
    })
}
