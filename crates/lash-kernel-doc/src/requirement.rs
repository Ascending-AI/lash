//! What code requires of an operation it performs (`K-DOC-002`,
//! `K-LIB-004`): the signature it calls the operation under and the
//! contract that pins what the operation means.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::Node;
use crate::function::FunctionDefinition;
use crate::graph_tokens::{encode_body, logical_digest};
use crate::name::{EffectName, FunctionId};
use crate::native::FunctionCatalog;
use crate::types::Signature;

/// What pins an operation's meaning under its signature: opaque text,
/// compared for equality and never read for anything else.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct ContractId(String);

impl ContractId {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The contract a requirement states while operations neither declare
    /// nor derive one: the digest of the signature's canonical bytes, so two
    /// requirements agree exactly when their signatures do.
    pub fn of_signature(signature: &Signature) -> Self {
        Self(
            blake3::Hash::from(logical_digest(&encode_body(signature)))
                .to_hex()
                .to_string(),
        )
    }
}

impl fmt::Display for ContractId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An operation as code requires it: the signature it performs the
/// operation under and the contract it was written against. A manifest
/// states one for each operation the document performs or reaches, and a
/// library body one for each operation it performs itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRequirement {
    pub signature: Signature,
    pub contract: ContractId,
}

impl OperationRequirement {
    /// The requirement of an operation performed under `signature`, its
    /// contract [`ContractId::of_signature`].
    pub fn of_signature(signature: Signature) -> Self {
        Self {
            contract: ContractId::of_signature(&signature),
            signature,
        }
    }
}

impl fmt::Display for OperationRequirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&crate::text::requirement_text(self))
    }
}

/// For each operation, what each body among `functions` that performs it
/// requires, by that body's function (`K-DOC-002`). `functions` is a
/// closed set, as a manifest lists one: every function a body in it calls
/// is in it too. A function `catalog` does not hold performs nothing here.
pub fn performed_by(
    catalog: &dyn FunctionCatalog,
    functions: impl IntoIterator<Item = FunctionId>,
) -> BTreeMap<EffectName, BTreeMap<FunctionId, &OperationRequirement>> {
    let mut performed: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
    for function in functions {
        let effects = match catalog.registered(&function) {
            Some(registered) => Some(registered.performs()),
            None => catalog
                .definition(&function)
                .and_then(FunctionDefinition::body)
                .map(|body| &body.effects),
        };
        for (effect, requirement) in effects.into_iter().flatten() {
            performed
                .entry(effect.clone())
                .or_default()
                .insert(function, requirement);
        }
    }
    performed
}

/// What the bodies of `roots`, and of every function they reach, perform:
/// [`performed_by`] over the functions `roots` reach.
pub fn performed_through(
    catalog: &dyn FunctionCatalog,
    roots: impl IntoIterator<Item = FunctionId>,
) -> BTreeMap<EffectName, BTreeMap<FunctionId, &OperationRequirement>> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<FunctionId> = roots.into_iter().collect();
    while let Some(function) = pending.pop() {
        if !reached.insert(function) {
            continue;
        }
        match catalog.registered(&function) {
            Some(registered) => pending.extend(registered.callees()),
            None => pending.extend(
                catalog
                    .definition(&function)
                    .and_then(FunctionDefinition::body)
                    .map(|body| Node::Block(&body.block).functions_within())
                    .unwrap_or_default(),
            ),
        }
    }
    performed_by(catalog, reached)
}
