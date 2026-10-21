//! The callable a view opens as its caller sees it: the signature it
//! declares and what a call of it can end with.

use std::collections::BTreeSet;

use lash_kernel_doc::Document;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use lash_vocabulary::types::Param;
use lash_vocabulary::types::Type;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{ScopeKind, WorkflowView};

/// The callable a view opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryView {
    /// The declared function; absent for the document's `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    /// Its parameters in order, as its signature declares them.
    pub params: Vec<EntryParamView>,
    /// The result type its signature declares; `any` when it declares none.
    pub result: Type,
    /// What a call can end with: the union of the facets of the `return`s
    /// and `finish`es that end this callable, except those that only follow
    /// a statement that cannot complete, with `null` when the body can
    /// complete and fall off its end. A lowering that spells falling off as an
    /// explicit `return` (TypeScript's `return absent`) contributes that
    /// return's facet instead.
    pub result_facet: Type,
}

/// One declared parameter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntryParamView {
    pub name: Name,
    /// The type the signature declares, not the binding's facet: `any` for
    /// a function the document does not list as an entry.
    #[serde(rename = "type")]
    pub ty: Type,
    /// A caller may leave it out.
    pub optional: bool,
    /// Its binding in `dataflow`.
    pub binding: usize,
}

/// The entry of `view`, opened at `unit`, whose parameters are bound to
/// `bindings` in order.
pub(crate) fn entry(
    document: &Document,
    unit: &Unit,
    bindings: &[usize],
    view: &WorkflowView,
) -> EntryView {
    let (name, params, result) = match unit {
        Unit::Function(name) => match document.entries.get(name) {
            Some(entry) => (
                Some(name.clone()),
                entry.signature.params.clone(),
                entry.signature.result.clone(),
            ),
            None => (
                Some(name.clone()),
                document
                    .functions
                    .get(name)
                    .map(|function| {
                        function
                            .params
                            .iter()
                            .map(|name| Param {
                                name: name.clone(),
                                ty: Type::Any,
                                optional: false,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                Type::Any,
            ),
        },
        Unit::Main | Unit::Library(_) => (None, Vec::new(), Type::Any),
    };
    let body = Site::new(unit.clone(), Vec::new());
    let unreached = unreached(view, &body);
    let facets = view
        .dataflow
        .returns
        .iter()
        .filter(|ending| ending.callable == body && !unreached.contains(&ending.statement))
        .map(|ending| ending.ty.clone());
    let falls_off = completes(view, &body);
    EntryView {
        name,
        params: params
            .into_iter()
            .zip(bindings)
            .map(|(param, binding)| EntryParamView {
                name: param.name,
                ty: param.ty,
                optional: param.optional,
                binding: *binding,
            })
            .collect(),
        result,
        result_facet: union(facets.chain(falls_off.then_some(Type::Null))),
    }
}

/// The statements of the callable whose body is `body` that only follow a
/// statement that cannot complete, in its own blocks: dead code a
/// lowering may leave, such as TypeScript's closing `return absent` after
/// an `if` whose arms both return. A closure's body is its own callable's.
fn unreached(view: &WorkflowView, body: &Site) -> BTreeSet<Site> {
    let mut unreached = BTreeSet::new();
    let mut blocks = vec![(body.clone(), true)];
    while let Some((block, mut live)) = blocks.pop() {
        for statement in view
            .statements
            .iter()
            .filter(|statement| statement.block == block)
        {
            if !live {
                unreached.insert(statement.site.clone());
            }
            blocks.extend(
                view.scopes
                    .iter()
                    .filter(|scope| {
                        scope.statement.as_ref() == Some(&statement.site)
                            && scope.kind != ScopeKind::Closure
                    })
                    .map(|scope| (scope.block.clone(), live)),
            );
            live &= crate::coarse::falls_through(&statement.statement);
        }
    }
    unreached
}

/// Every statement of `block` can complete, so the block can end by
/// falling off its end.
fn completes(view: &WorkflowView, block: &Site) -> bool {
    view.statements
        .iter()
        .filter(|statement| statement.block == *block)
        .all(|statement| crate::coarse::falls_through(&statement.statement))
}

/// A union in the shape validation accepts, as the checker writes one:
/// flat, without repeats, `any` when a member is, and no union at all for
/// one member. No member says nothing, so `any`.
fn union(members: impl IntoIterator<Item = Type>) -> Type {
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
