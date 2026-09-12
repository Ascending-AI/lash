use std::collections::BTreeSet;

use serde_json::Value;

use super::{Path, is_null_schema};

/// An argument-instance path whose final property may be removed when its
/// value is `null` because strict projection introduced null as an omission
/// sentinel at exactly that location.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OmissionNullPath {
    segments: Vec<OmissionNullPathSegment>,
}

impl OmissionNullPath {
    pub fn segments(&self) -> &[OmissionNullPathSegment] {
        &self.segments
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OmissionNullPathSegment {
    Property(String),
    ArrayItem,
}

pub(super) fn materialize_omission_null_paths(
    schema: &Value,
    introduced: &BTreeSet<Path>,
    diagnostics: &mut Vec<String>,
) -> Vec<OmissionNullPath> {
    let mut paths = BTreeSet::new();
    let mut active_refs = Vec::new();
    collect_omission_null_paths(
        schema,
        schema,
        &Path::root(),
        &[],
        introduced,
        diagnostics,
        &mut active_refs,
        &mut paths,
    );
    paths.into_iter().collect()
}

#[allow(clippy::too_many_arguments)]
fn collect_omission_null_paths(
    root: &Value,
    schema: &Value,
    schema_path: &Path,
    instance_path: &[OmissionNullPathSegment],
    introduced: &BTreeSet<Path>,
    diagnostics: &mut Vec<String>,
    active_refs: &mut Vec<Path>,
    paths: &mut BTreeSet<OmissionNullPath>,
) {
    if introduced.contains(schema_path) {
        paths.insert(OmissionNullPath {
            segments: instance_path.to_vec(),
        });
    }

    let Some(object) = schema.as_object() else {
        return;
    };

    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let Some((target, target_path)) = resolve_local_schema_ref(root, reference) else {
            diagnostics.push(format!(
                "{schema_path}: omission-null mapping left untouched because `{reference}` is not an explicit resolvable local reference"
            ));
            return;
        };
        if active_refs.contains(&target_path) {
            diagnostics.push(format!(
                "{schema_path}: omission-null mapping left recursive reference `{reference}` untouched"
            ));
            return;
        }
        active_refs.push(target_path.clone());
        collect_omission_null_paths(
            root,
            target,
            &target_path,
            instance_path,
            introduced,
            diagnostics,
            active_refs,
            paths,
        );
        active_refs.pop();
        return;
    }

    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let mut property_instance_path = instance_path.to_vec();
            property_instance_path.push(OmissionNullPathSegment::Property(name.clone()));
            collect_omission_null_paths(
                root,
                property,
                &schema_path.child("properties").child(name),
                &property_instance_path,
                introduced,
                diagnostics,
                active_refs,
                paths,
            );
        }
    }

    if let Some(items) = object.get("items") {
        let mut item_instance_path = instance_path.to_vec();
        item_instance_path.push(OmissionNullPathSegment::ArrayItem);
        collect_omission_null_paths(
            root,
            items,
            &schema_path.child("items"),
            &item_instance_path,
            introduced,
            diagnostics,
            active_refs,
            paths,
        );
    }

    let Some(any_of) = object.get("anyOf").and_then(Value::as_array) else {
        return;
    };
    let branch_paths = any_of
        .iter()
        .enumerate()
        .map(|(index, _)| schema_path.child("anyOf").index(index))
        .collect::<Vec<_>>();
    let mapped_branches = branch_paths
        .iter()
        .filter(|branch_path| {
            introduced
                .iter()
                .any(|path| path.is_at_or_below(branch_path))
        })
        .count();
    let non_null_branches = any_of
        .iter()
        .filter(|branch| !is_null_schema(branch))
        .count();
    if mapped_branches > 0 && non_null_branches > 1 {
        diagnostics.push(format!(
            "{schema_path}: omission-null mapping inside multi-branch anyOf left untouched"
        ));
        return;
    }
    for (index, branch) in any_of.iter().enumerate() {
        if is_null_schema(branch) {
            continue;
        }
        collect_omission_null_paths(
            root,
            branch,
            &branch_paths[index],
            instance_path,
            introduced,
            diagnostics,
            active_refs,
            paths,
        );
    }
}

fn resolve_local_schema_ref<'a>(root: &'a Value, reference: &str) -> Option<(&'a Value, Path)> {
    if reference == "#" {
        return Some((root, Path::root()));
    }
    let pointer = reference.strip_prefix("#/")?;
    let mut value = root;
    let mut path = Path::root();
    for encoded_segment in pointer.split('/') {
        let segment = encoded_segment.replace("~1", "/").replace("~0", "~");
        match value {
            Value::Object(object) => {
                value = object.get(&segment)?;
                path = path.child(&segment);
            }
            Value::Array(array) => {
                let index = segment.parse::<usize>().ok()?;
                value = array.get(index)?;
                path = path.index(index);
            }
            _ => return None,
        }
    }
    Some((value, path))
}
