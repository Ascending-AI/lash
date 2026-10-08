use crate::{TypeExpr, TypeField};

pub fn is_resolved_type_assignable(source: &TypeExpr, target: &TypeExpr) -> bool {
    if matches!(source, TypeExpr::Any) || matches!(target, TypeExpr::Any) {
        return true;
    }
    if source == target {
        return true;
    }

    match (source, target) {
        (TypeExpr::Union(sources), _) => sources
            .iter()
            .all(|source| is_resolved_type_assignable(source, target)),
        (_, TypeExpr::Union(targets)) => targets
            .iter()
            .any(|target| is_resolved_type_assignable(source, target)),
        (TypeExpr::Int, TypeExpr::Float) => true,
        (TypeExpr::Str, TypeExpr::Enum(_)) => true,
        (TypeExpr::Enum(_), TypeExpr::Str) => true,
        (TypeExpr::Enum(sources), TypeExpr::Enum(targets)) => {
            sources.iter().all(|source| targets.contains(source))
        }
        // An empty list literal has no elements to type, so `union_type`
        // gives it the empty-list sentinel `list[null]` (see
        // `linker/type_helpers.rs`). `[]` is the most natural spelling of
        // "no items" and must fit a `list[T]` parameter for every `T`;
        // rejecting it turned a documented `edits: []` example into a link
        // error in five of eight live episodes (FIG-1421).
        (TypeExpr::List(source), TypeExpr::List(_)) if **source == TypeExpr::Null => true,
        (TypeExpr::List(source), TypeExpr::List(target)) => {
            is_resolved_type_assignable(source, target)
        }
        (TypeExpr::Dict, TypeExpr::Object(_)) => true,
        (TypeExpr::Object(_), TypeExpr::Dict) => true,
        (TypeExpr::Object(source), TypeExpr::Object(target)) => {
            object_type_assignable(source, target)
        }
        (TypeExpr::Ref(source), TypeExpr::Ref(target)) => source == target,
        // A process value evaluates to its immutable definition record (the
        // compiler's `process_ref_literal`), so it fits wherever a contract
        // names that record, as `processes.start`'s `definition` does.
        (TypeExpr::Process(_), TypeExpr::Object(target)) => is_definition_record_type(target),
        (TypeExpr::Process(source), TypeExpr::Process(target)) => {
            match (source.as_signature(), target.as_signature()) {
                (None, _) | (_, None) => true,
                (Some(source), Some(target)) => {
                    source.arity() == target.arity()
                        && source.params().iter().zip(target.params()).all(
                            |(source_param, target_param)| {
                                source_param.name == target_param.name
                                    && is_resolved_type_assignable(
                                        &target_param.ty,
                                        &source_param.ty,
                                    )
                            },
                        )
                        && is_resolved_type_assignable(source.output(), target.output())
                }
            }
        }

        _ => false,
    }
}

/// Whether `fields` spell the immutable definition record `{ id, signature }`:
/// `id` the tagged `{ $lash_definition_id: str }` and `signature` its variants.
fn is_definition_record_type(fields: &[TypeField]) -> bool {
    let [first, second] = fields else {
        return false;
    };
    let (id, signature) = if first.name.as_str() == "id" {
        (first, second)
    } else {
        (second, first)
    };
    let tagged_id = matches!(
        &id.ty,
        TypeExpr::Object(id_fields)
            if matches!(
                id_fields.as_slice(),
                [tag] if tag.name.as_str() == "$lash_definition_id" && tag.ty == TypeExpr::Str
            )
    );
    id.name.as_str() == "id" && tagged_id && signature.name.as_str() == "signature"
}

fn object_type_assignable(source: &[TypeField], target: &[TypeField]) -> bool {
    target.iter().all(|target_field| {
        let Some(source_field) = source
            .iter()
            .find(|source_field| source_field.name == target_field.name)
        else {
            return target_field.optional;
        };
        if !target_field.optional && source_field.optional {
            return false;
        }
        is_resolved_type_assignable(&source_field.ty, &target_field.ty)
    })
}

#[cfg(test)]
mod tests;
