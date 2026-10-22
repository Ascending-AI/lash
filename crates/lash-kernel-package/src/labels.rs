//! Validate sidecar keys against the exact immutable Package and retained definitions.
use crate::{LinkedContent, MemberKind, Package, Segment};
use lash_helper_labels::{LabelError, Labels};
use lash_kernel_doc::Export;
use std::collections::BTreeSet;

#[cfg(test)]
std::thread_local! {
    pub(super) static DEFINITION_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read strict labels without changing executable content or identity.
/// Member keys are `member` or `type.member`; definition keys name private
/// definitions in documents directly exported by this Package.
pub fn read_labels(
    content: &LinkedContent,
    package: &Package,
    json: &str,
) -> Result<Labels, LabelError> {
    let labels = lash_helper_labels::read_json(json)?;
    let mut definitions = BTreeSet::new();
    let mut public = BTreeSet::new();
    let mut indexed = BTreeSet::new();
    for member in package.members.values() {
        if let MemberKind::Export { link, export } = &member.kind {
            let link = content
                .link(*link)
                .map_err(|e| LabelError::at("/definitions", "", e.to_string()))?;
            let document = content
                .document(link.document)
                .map_err(|e| LabelError::at("/definitions", "", e.to_string()))?;
            if indexed.insert(link.document) {
                definitions.extend(document.functions.values().map(|f| {
                    #[cfg(test)]
                    DEFINITION_VISITS.with(|count| count.set(count.get() + 1));
                    f.name.as_str()
                }));
            }
            if let Some(Export::Function(f)) = document.exports.get(&export.name) {
                public.insert(f.function);
            }
            if let Some(Export::Type(ty)) = document.exports.get(&export.name) {
                public.extend(ty.members.values().map(|m| m.function().function));
            }
        }
    }
    for key in labels.members.keys() {
        let refuse = || LabelError::at("/members", key, "unknown member path");
        let mut parts = key.split('.');
        let name = Segment::new(parts.next().unwrap_or_default()).map_err(|_| refuse())?;
        let member = package.members.get(&name).ok_or_else(refuse)?;
        if let Some(part) = parts.next() {
            if parts.next().is_some() {
                return Err(refuse());
            }
            let name = Segment::new(part).map_err(|_| refuse())?;
            let MemberKind::Export { link, export } = &member.kind else {
                return Err(refuse());
            };
            let Ok(Export::Type(ty)) = content.export(*link, export) else {
                return Err(refuse());
            };
            if !ty.members.contains_key(&name) {
                return Err(refuse());
            }
        }
    }
    for key in labels.definitions.keys() {
        if !definitions.contains(key.as_str()) {
            return Err(LabelError::at(
                "/definitions",
                key,
                "unknown retained definition",
            ));
        }
        if content.documents.values().any(|d| {
            public
                .iter()
                .any(|id| d.functions.get(id).is_some_and(|f| f.name.as_str() == key))
        }) {
            return Err(LabelError::at(
                "/definitions",
                key,
                "definition backs a member; label that member",
            ));
        }
    }
    Ok(labels)
}
