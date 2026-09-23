//! `Program::spans` serializes as a list of entries: a `BTreeMap`'s struct
//! key is not a JSON object key, and `ModuleArtifact` encodes `Program` as
//! JSON. Iteration order is already key order, so the form stays canonical.

use super::{AstPath, Span};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
struct SpanEntry {
    path: AstPath,
    span: Span,
}

pub(super) fn serialize<S: Serializer>(
    spans: &BTreeMap<AstPath, Span>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(spans.iter().map(|(path, span)| SpanEntry {
        path: path.clone(),
        span: *span,
    }))
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<AstPath, Span>, D::Error> {
    Ok(Vec::<SpanEntry>::deserialize(deserializer)?
        .into_iter()
        .map(|entry| (entry.path, entry.span))
        .collect())
}
