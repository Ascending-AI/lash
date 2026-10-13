//! An entry's linked-effect set (P2): the effects its code calls or spawns.
//!
//! It is separate from each unit's [`crate::EffectSet`], which bounds what a
//! unit may perform through every function it reaches, references included.
//! A process env links exactly an entry's linked effects, so this set counts
//! only what the entry runs: its own performs and those of its closures,
//! and those of every declared function it calls or spawns, to a fixed
//! point. A declared function the entry only passes as a value is a template
//! its receiver links, and is not counted.
//!
//! A call through a value runs a closure, a library function or a declared
//! function the code holds as a value. A closure's performs and calls are
//! already its unit's own, and a closure exists only once its unit ran; a
//! native library function performs nothing. So a call through a value is
//! certified unless the entry holds a declared function as a value that it
//! does not also run: that template's effects are not counted, and the call
//! might run it, so it is refused. A call through a variable that only ever
//! holds a closure of its own unit runs that closure, and is never refused.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Document, EffectName, FunctionCatalog, Literal, Name, Signature, Site, Unit,
};

use crate::graph::{
    ActionNode, AtomNode, BindingId, CalleeNode, ExprNode, Graph, NodeKind, PlaceNode, Reference,
    StmtNode,
};

/// Why an entry's linked-effect set cannot be certified.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EntryEffectsRefusal {
    #[error("the document has no entry `{entry}`")]
    UnknownEntry { entry: Name },
    /// The entry, or a function it calls, calls a function held in a value,
    /// and holds `template`, a declared function it does not run, as a
    /// value: the call might run it, and what it performs is not counted.
    #[error(
        "the call at {site:?} goes through a value, which might run `{template}`, a function the entry holds but does not run"
    )]
    ValueCall { site: Site, template: Name },
    #[error("effect `{effect}` is performed but the document's manifest does not declare it")]
    UndeclaredEffect { effect: EffectName },
}

/// What one unit's own code runs, before following its calls.
#[derive(Default)]
struct Runs {
    effects: BTreeSet<EffectName>,
    calls: BTreeSet<Name>,
    /// The declared functions the unit holds as values.
    held: BTreeSet<Name>,
    value_call: Option<Site>,
}

/// The effects `entry` of `document` calls or spawns, each with the
/// signature the document's manifest declares for it. `graph` is the
/// document's derived graph against `catalog`.
///
/// # Errors
///
/// `entry` is not an entry of the document, a function it runs calls
/// through a value, or a performed effect is undeclared.
pub fn entry_effects(
    document: &Document,
    graph: &Graph,
    catalog: &dyn FunctionCatalog,
    entry: &Name,
) -> Result<BTreeMap<EffectName, Signature>, EntryEffectsRefusal> {
    if !document.entries.contains_key(entry) {
        return Err(EntryEffectsRefusal::UnknownEntry {
            entry: entry.clone(),
        });
    }
    let closures = closure_bindings(graph);
    let mut runs: BTreeMap<Unit, Runs> = BTreeMap::new();
    for node in &graph.nodes {
        let unit = runs.entry(node.site.unit.clone()).or_default();
        let action = match &node.kind {
            NodeKind::Action(action) => action,
            NodeKind::Expr(ExprNode::Literal(Literal::Function(name))) => {
                unit.held.insert(name.clone());
                continue;
            }
            _ => continue,
        };
        unit.held
            .extend(atoms_of(action).filter_map(|atom| match atom {
                AtomNode::Literal(Literal::Function(name)) => Some(name.clone()),
                _ => None,
            }));
        match action {
            ActionNode::Perform { effect, .. } => {
                unit.effects.insert(effect.clone());
            }
            ActionNode::Call { callee, .. } | ActionNode::Spawn { callee, .. } => match callee {
                CalleeNode::Declared(name) => {
                    unit.calls.insert(name.clone());
                }
                CalleeNode::Value(Reference::Binding(binding)) if closures.contains(binding) => {}
                CalleeNode::Value(_) => {
                    unit.value_call.get_or_insert_with(|| node.site.clone());
                }
                // A library function with a kernel-code body calls the
                // functions it is passed: a declared function passed to it
                // by name is called.
                CalleeNode::Library(function) => {
                    if catalog
                        .definition(function)
                        .is_none_or(|definition| !definition.has_native())
                    {
                        unit.calls
                            .extend(atoms_of(action).filter_map(|atom| match atom {
                                AtomNode::Literal(Literal::Function(name)) => Some(name.clone()),
                                _ => None,
                            }));
                    }
                }
            },
            _ => {}
        }
    }
    let mut reached = BTreeSet::from([entry.clone()]);
    let mut pending = vec![entry.clone()];
    let mut effects = BTreeSet::new();
    let mut held = BTreeSet::new();
    let mut value_call = None;
    while let Some(name) = pending.pop() {
        let Some(unit) = runs.get(&Unit::Function(name)) else {
            continue;
        };
        if value_call.is_none() {
            value_call.clone_from(&unit.value_call);
        }
        held.extend(unit.held.iter().cloned());
        effects.extend(unit.effects.iter().cloned());
        for callee in &unit.calls {
            if reached.insert(callee.clone()) {
                pending.push(callee.clone());
            }
        }
    }
    if let Some(site) = value_call
        && let Some(template) = held.into_iter().find(|name| !reached.contains(name))
    {
        return Err(EntryEffectsRefusal::ValueCall { site, template });
    }
    effects
        .into_iter()
        .map(|effect| match document.manifest.effects.get(&effect) {
            Some(signature) => Ok((effect, signature.clone())),
            None => Err(EntryEffectsRefusal::UndeclaredEffect { effect }),
        })
        .collect()
}

/// The variables every write of which is a closure literal: a call through
/// one runs a closure of its own unit.
fn closure_bindings(graph: &Graph) -> BTreeSet<BindingId> {
    let mut closures = BTreeSet::new();
    let mut other = BTreeSet::new();
    for node in &graph.nodes {
        let (binding, value) = match &node.kind {
            NodeKind::Stmt(StmtNode::Let { binding, value }) => (*binding, *value),
            NodeKind::Stmt(StmtNode::Assign {
                place: PlaceNode::Variable(Reference::Binding(binding)),
                value,
            }) => (*binding, *value),
            _ => continue,
        };
        if matches!(
            graph.nodes[value.index()].kind,
            NodeKind::Expr(ExprNode::Closure { .. })
        ) {
            closures.insert(binding);
        } else {
            other.insert(binding);
        }
    }
    closures.retain(|binding| !other.contains(binding));
    closures
}

fn atoms_of(action: &ActionNode) -> impl Iterator<Item = &AtomNode> {
    crate::flow::atoms(action)
}
