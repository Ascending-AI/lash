//! An entry's linked-effect set (P2): the effects its code calls or spawns.
//!
//! It is separate from each unit's [`crate::EffectSet`], which bounds what a
//! unit may perform through every function it reaches, references included.
//! A process env links exactly an entry's linked effects, so this set counts
//! only what the entry runs: its own performs and those of its closures,
//! those of the library bodies it calls (`K-DOC-002`), and those of every
//! declared function it calls or spawns, to a fixed point. A declared function the entry only passes as a value is a template
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
    Document, EffectName, FunctionCatalog, FunctionId, Literal, Name, OperationRequirement, Site,
    Type, Unit,
};

use crate::graph::{
    ActNode, ActionNode, AtomNode, BindingId, CalleeNode, ExprNode, Graph, NodeId, NodeKind,
    PlaceNode, Reference, StmtNode,
};
use crate::manifest::performs_through;

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
) -> Result<BTreeMap<EffectName, OperationRequirement>, EntryEffectsRefusal> {
    if !document.entries.contains_key(entry) {
        return Err(EntryEffectsRefusal::UnknownEntry {
            entry: entry.clone(),
        });
    }
    linked_effects(document, graph, catalog, Unit::Function(entry.clone()))
}

/// The effects a session cell's `main` calls or spawns, as
/// [`entry_effects`] counts an entry's: its own performs, its closures',
/// and those of every declared function it calls or spawns. A function the
/// cell calls through a session binding is not followed: what it performs
/// is not counted, and a host that plans by this set treats it as it treats
/// any effect outside it.
///
/// # Errors
///
/// A function it runs calls through a value while it holds a declared
/// function it does not run, or a performed effect is undeclared.
pub fn main_effects(
    document: &Document,
    graph: &Graph,
    catalog: &dyn FunctionCatalog,
) -> Result<BTreeMap<EffectName, OperationRequirement>, EntryEffectsRefusal> {
    linked_effects(document, graph, catalog, Unit::Main)
}

/// The effects `root`'s code calls or spawns, to a fixed point over the
/// declared functions it calls or spawns.
fn linked_effects(
    document: &Document,
    graph: &Graph,
    catalog: &dyn FunctionCatalog,
    root: Unit,
) -> Result<BTreeMap<EffectName, OperationRequirement>, EntryEffectsRefusal> {
    let Linked {
        reached,
        effects,
        held,
        value_call,
    } = linked_units(graph, catalog, root);
    if let Some(site) = value_call
        && let Some(template) = held.into_iter().find(|name| !reached.contains(name))
    {
        return Err(EntryEffectsRefusal::ValueCall { site, template });
    }
    effects
        .into_iter()
        .map(|effect| match document.manifest.effects.get(&effect) {
            Some(required) => Ok((effect, required.clone())),
            None => Err(EntryEffectsRefusal::UndeclaredEffect { effect }),
        })
        .collect()
}

/// The declared units an entry executes, using the same call/template
/// distinction as its linked effects. Closures belong to their owning unit.
pub(crate) fn executed_units(
    graph: &Graph,
    catalog: &dyn FunctionCatalog,
    entry: &Name,
) -> BTreeSet<Unit> {
    linked_units(graph, catalog, Unit::Function(entry.clone()))
        .reached
        .into_iter()
        .map(Unit::Function)
        .collect()
}

struct Linked {
    reached: BTreeSet<Name>,
    effects: BTreeSet<EffectName>,
    held: BTreeSet<Name>,
    value_call: Option<Site>,
}

fn linked_units(graph: &Graph, catalog: &dyn FunctionCatalog, root: Unit) -> Linked {
    let closures = closure_bindings(graph);
    let targets = function_targets(graph);
    let mut runs: BTreeMap<Unit, Runs> = BTreeMap::new();
    let mut callback_helpers = BTreeMap::new();
    let mut performs = BTreeMap::new();
    for node in &graph.nodes {
        let unit = runs.entry(node.site.unit.clone()).or_default();
        // A library function's body, and the bodies it reaches, perform
        // what they list. One with a kernel-code body calls the functions
        // it is passed: what the expressions passed to it can hold.
        let mut library_call = |unit: &mut Runs, function: &FunctionId, args: &[NodeId]| {
            unit.effects
                .extend(performs_through(catalog, function, &mut performs));
            if !*callback_helpers
                .entry(*function)
                .or_insert_with(|| library_calls_values(catalog, function))
            {
                return;
            }
            for argument in args {
                let argument = graph.node(*argument);
                let known = targets_of(graph, argument.id, &targets);
                if !known.is_empty() {
                    unit.calls.extend(known);
                    continue;
                }
                match &argument.kind {
                    NodeKind::Expr(ExprNode::Literal(Literal::Function(name))) => {
                        unit.calls.insert(name.clone());
                    }
                    NodeKind::Expr(ExprNode::Variable(Reference::Binding(binding)))
                        if closures.contains(binding) => {}
                    _ if argument.facet.as_ref().is_none_or(may_hold_function) => {
                        unit.value_call.get_or_insert_with(|| node.site.clone());
                    }
                    _ => {}
                }
            }
        };
        let action = match &node.kind {
            NodeKind::Action(action) => action,
            NodeKind::Stmt(StmtNode::ReturnVia { value, policy }) => {
                library_call(unit, policy, &[*value]);
                continue;
            }
            NodeKind::Expr(ExprNode::Call { function, args }) => {
                library_call(unit, function, args);
                continue;
            }
            NodeKind::Expr(ExprNode::Act { kind, args }) => {
                match kind {
                    ActNode::Perform { effect, .. } => {
                        unit.effects.insert(effect.clone());
                    }
                    ActNode::Call(callee) | ActNode::Spawn(callee) => match callee {
                        CalleeNode::Declared(name) => {
                            unit.calls.insert(name.clone());
                        }
                        CalleeNode::Value(Reference::Binding(binding))
                            if closures.contains(binding) => {}
                        CalleeNode::Value(_) => {
                            unit.value_call.get_or_insert_with(|| node.site.clone());
                        }
                        CalleeNode::Library(function) => library_call(unit, function, args),
                    },
                    ActNode::Await(policy) => library_call(unit, policy, args),
                    ActNode::Sleep
                    | ActNode::Join
                    | ActNode::JoinMany(_)
                    | ActNode::Yield
                    | ActNode::Cancel => {}
                }
                continue;
            }
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
        let awaited;
        let callee = match action {
            ActionNode::Perform { effect, .. } => {
                unit.effects.insert(effect.clone());
                None
            }
            ActionNode::Call { callee, .. } | ActionNode::Spawn { callee, .. } => Some(callee),
            // The await policy is a library call of the value.
            ActionNode::Await { policy, .. } => {
                awaited = CalleeNode::Library(*policy);
                Some(&awaited)
            }
            _ => None,
        };
        if let Some(callee) = callee {
            match callee {
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
                    unit.effects
                        .extend(performs_through(catalog, function, &mut performs));
                    if *callback_helpers
                        .entry(*function)
                        .or_insert_with(|| library_calls_values(catalog, function))
                    {
                        for argument in atoms_of(action) {
                            let known = targets_of_atom(argument, &targets);
                            if !known.is_empty() {
                                unit.calls.extend(known);
                                continue;
                            }
                            match argument {
                                AtomNode::Literal(Literal::Function(name)) => {
                                    unit.calls.insert(name.clone());
                                }
                                AtomNode::Variable(Reference::Binding(binding))
                                    if closures.contains(binding) => {}
                                AtomNode::Variable(reference) => {
                                    let callable = match reference {
                                        Reference::Binding(binding) => {
                                            may_hold_function(&graph.binding(*binding).facet)
                                        }
                                        Reference::Session(_) => true,
                                    };
                                    if callable {
                                        unit.value_call.get_or_insert_with(|| node.site.clone());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
    let mut reached = BTreeSet::new();
    if let Unit::Function(entry) = &root {
        reached.insert(entry.clone());
    }
    let mut pending = vec![root];
    let mut effects = BTreeSet::new();
    let mut held = BTreeSet::new();
    let mut value_call = None;
    while let Some(next) = pending.pop() {
        let Some(unit) = runs.get(&next) else {
            continue;
        };
        if value_call.is_none() {
            value_call.clone_from(&unit.value_call);
        }
        held.extend(unit.held.iter().cloned());
        effects.extend(unit.effects.iter().cloned());
        for callee in &unit.calls {
            if reached.insert(callee.clone()) {
                pending.push(Unit::Function(callee.clone()));
            }
        }
    }
    Linked {
        reached,
        effects,
        held,
        value_call,
    }
}

/// The variables every write of which is a closure literal: a call through
/// one runs a closure of its own unit.
fn closure_bindings(graph: &Graph) -> BTreeSet<BindingId> {
    let mut closures = BTreeSet::new();
    let mut other = BTreeSet::new();
    for node in &graph.nodes {
        let (binding, value) = match &node.kind {
            NodeKind::Stmt(StmtNode::Let { binding, value })
            | NodeKind::Expr(ExprNode::Let { binding, value, .. }) => (*binding, *value),
            NodeKind::Stmt(StmtNode::Assign {
                place: PlaceNode::Variable(Reference::Binding(binding)),
                value,
                ..
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

/// A helper may receive a callback directly or inside a collection. Unknown
/// facets cannot certify that a held template stays uncalled.
fn may_hold_function(ty: &Type) -> bool {
    match ty {
        Type::Function(_) => true,
        Type::Tuple(items) | Type::Union(items) => items.iter().any(may_hold_function),
        Type::List(item) | Type::Set(item) | Type::Task(item) => may_hold_function(item),
        Type::Map(map) => may_hold_function(&map.key) || may_hold_function(&map.value),
        Type::Record(record) => {
            record
                .fields
                .iter()
                .any(|field| may_hold_function(&field.ty))
                || record.rest.as_deref().is_some_and(may_hold_function)
        }
        _ => false,
    }
}

/// Pure helpers that only carry a reference do not run its template (P2).
/// The derived body's value-call flag also follows body-backed helper calls.
fn library_calls_values(
    catalog: &dyn FunctionCatalog,
    function: &lash_kernel_doc::FunctionId,
) -> bool {
    let Some(definition) = catalog.definition(function) else {
        return true;
    };
    if definition.body().is_none() {
        return false;
    }
    crate::derive_definition(definition, catalog).map_or(true, |graph| {
        graph.units().iter().any(|unit| unit.effects.through_values)
    })
}

/// Possible declared targets carried through values. A tool result does not
/// carry the templates passed to that tool: a child handle stays a handle.
fn function_targets(graph: &Graph) -> BTreeMap<BindingId, BTreeSet<Name>> {
    let mut targets = BTreeMap::<BindingId, BTreeSet<Name>>::new();
    loop {
        let mut changed = false;
        for node in graph.nodes() {
            let (binding, value) = match &node.kind {
                NodeKind::Expr(ExprNode::Let { binding, value, .. })
                | NodeKind::Stmt(StmtNode::Let { binding, value })
                | NodeKind::Stmt(StmtNode::Assign {
                    place: PlaceNode::Variable(Reference::Binding(binding)),
                    value,
                    ..
                }) => (*binding, *value),
                NodeKind::Stmt(StmtNode::For {
                    binding, iterable, ..
                }) => (*binding, *iterable),
                _ => continue,
            };
            let carried = targets_of(graph, value, &targets);
            let entry = targets.entry(binding).or_default();
            let previous = entry.len();
            entry.extend(carried);
            changed |= entry.len() != previous;
        }
        if !changed {
            return targets;
        }
    }
}

fn targets_of_atom(
    atom: &AtomNode,
    targets: &BTreeMap<BindingId, BTreeSet<Name>>,
) -> BTreeSet<Name> {
    match atom {
        AtomNode::Literal(Literal::Function(name)) => BTreeSet::from([name.clone()]),
        AtomNode::Variable(Reference::Binding(binding)) => {
            targets.get(binding).cloned().unwrap_or_default()
        }
        _ => BTreeSet::new(),
    }
}

fn targets_of(
    graph: &Graph,
    value: crate::graph::NodeId,
    targets: &BTreeMap<BindingId, BTreeSet<Name>>,
) -> BTreeSet<Name> {
    let union = |children: &[crate::graph::NodeId]| {
        children
            .iter()
            .flat_map(|child| targets_of(graph, *child, targets))
            .collect()
    };
    match &graph.node(value).kind {
        NodeKind::Expr(ExprNode::Literal(Literal::Function(name))) => {
            BTreeSet::from([name.clone()])
        }
        NodeKind::Expr(ExprNode::Variable(Reference::Binding(binding))) => {
            targets.get(binding).cloned().unwrap_or_default()
        }
        NodeKind::Expr(ExprNode::Tuple(items) | ExprNode::List(items) | ExprNode::Set(items)) => {
            union(items)
        }
        NodeKind::Expr(ExprNode::Record(fields)) => fields
            .iter()
            .flat_map(|field| targets_of(graph, field.value, targets))
            .collect(),
        NodeKind::Expr(ExprNode::Map(entries)) => entries
            .iter()
            .flat_map(|entry| [entry.key, entry.value])
            .flat_map(|value| targets_of(graph, value, targets))
            .collect(),
        NodeKind::Expr(ExprNode::Member(member)) => {
            let target = match member {
                crate::graph::MemberNode::Field { target, .. }
                | crate::graph::MemberNode::Index { target, .. } => target,
            };
            targets_of(graph, *target, targets)
        }
        NodeKind::Expr(ExprNode::If { then, r#else, .. }) => union(&[*then, *r#else]),
        NodeKind::Expr(ExprNode::Let { body, .. }) => targets_of(graph, *body, targets),
        NodeKind::Expr(
            ExprNode::Call { args, .. }
            | ExprNode::Act {
                kind: ActNode::Call(_) | ActNode::Spawn(_),
                args,
            },
        ) => union(args),
        NodeKind::Action(ActionNode::Call { args, .. } | ActionNode::Spawn { args, .. }) => args
            .iter()
            .flat_map(|arg| targets_of_atom(arg, targets))
            .collect(),
        _ => BTreeSet::new(),
    }
}
