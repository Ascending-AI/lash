//! Manifest derivation (`K-DOC-002`, `K-ADM-007`): what a document's code
//! requires, read from the code, against what its manifest says.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    ActKind, Action, Atom, Block, Callee, Document, EffectName, Expr, FunctionCatalog, FunctionId,
    FunctionName, InvalidReason, Literal, Name, Node,
};

use crate::refusal::{RefusalReason, Refused, refused};

/// What a document's code requires of an environment, derived from the code
/// alone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    /// Every effect some `perform` names.
    pub effects: BTreeSet<EffectName>,
    /// Every library function the code reaches, directly or through another
    /// function's body, with the name its definition carries. A function
    /// the catalog does not hold is listed under the name its caller gives
    /// it, and its own body is not followed.
    pub functions: BTreeMap<FunctionId, FunctionName>,
}

/// The references in a block, shared by manifest and entry-slice derivation.
/// Children come from the kernel's exhaustive structural walk and library
/// functions from its slot inventory ([`Node::functions`]); actions have
/// atomic arguments rather than children, so every action and literal kind
/// is accounted for here as well.
fn scan(
    block: &Block,
    effects: &mut BTreeSet<EffectName>,
    functions: &mut BTreeSet<FunctionId>,
    declared: &mut BTreeSet<Name>,
) {
    let mut pending = vec![Node::Block(block)];
    while let Some(node) = pending.pop() {
        functions.extend(node.functions());
        match node {
            Node::Block(_) | Node::Stmt(_) => {}
            Node::Expr(expr) => match expr {
                Expr::Literal(literal) => reference(literal, declared),
                Expr::Call { .. }
                | Expr::Closure(_)
                | Expr::Variable(_)
                | Expr::Tuple(_)
                | Expr::List(_)
                | Expr::Map(_)
                | Expr::Set(_)
                | Expr::Record(_)
                | Expr::Member(_)
                | Expr::If { .. }
                | Expr::Let { .. }
                | Expr::Clock
                | Expr::Random
                | Expr::Read(_) => {}
                Expr::Act(act) => match &act.kind {
                    ActKind::Call(callee) | ActKind::Spawn(callee) => {
                        callee_reference(callee, declared);
                    }
                    ActKind::Perform { effect, .. } => {
                        effects.insert(effect.clone());
                    }
                    ActKind::Await(_)
                    | ActKind::Sleep { .. }
                    | ActKind::Join
                    | ActKind::JoinMany(_)
                    | ActKind::Cancel
                    | ActKind::Yield => {}
                },
            },
            Node::Action(action) => {
                let args: &[Atom] = match action {
                    Action::Call { callee, args } | Action::Spawn { callee, args } => {
                        callee_reference(callee, declared);
                        args
                    }
                    Action::Perform { effect, args, .. } => {
                        effects.insert(effect.clone());
                        args
                    }
                    Action::Await { value, .. } => std::slice::from_ref(value),
                    Action::Sleep { duration, .. } => std::slice::from_ref(duration),
                    Action::Join { task } | Action::Cancel { task } => std::slice::from_ref(task),
                    Action::JoinMany { tasks, .. } => std::slice::from_ref(tasks),
                    Action::Yield => &[],
                };
                for atom in args {
                    match atom {
                        Atom::Literal(literal) => reference(literal, declared),
                        Atom::Variable(_) => {}
                    }
                }
            }
        }
        pending.extend(node.children());
    }
}

fn callee_reference(callee: &Callee, declared: &mut BTreeSet<Name>) {
    match callee {
        Callee::Declared(name) => {
            declared.insert(name.clone());
        }
        Callee::Library(_) | Callee::Value(_) => {}
    }
}

fn reference(literal: &Literal, declared: &mut BTreeSet<Name>) {
    match literal {
        Literal::Function(name) => {
            declared.insert(name.clone());
        }
        Literal::Null
        | Literal::Absent
        | Literal::Bool(_)
        | Literal::Int(_)
        | Literal::Float(_)
        | Literal::Text(_)
        | Literal::Bytes(_) => {}
    }
}

/// A declared entry's structural reachability, derived in one reference walk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryReachability {
    /// The root and every declared function it reaches.
    pub functions: BTreeSet<Name>,
    /// The effects performed by those functions, including their closures.
    pub effects: BTreeSet<EffectName>,
}

/// The declared functions `root` reaches, including itself, through calls,
/// spawns and function-reference literals in any expression or atomic slot.
/// Closures are traversed as part of their enclosing function. Cycles are
/// followed once. This uses the same reference walk as manifest derivation.
///
/// No library catalog is needed: library code cannot name the document's
/// declared functions. This is structural reachability, not admission;
/// scope, type and environment rules remain on [`crate::admit`].
///
/// # Errors
///
/// The root or a function it references is not declared.
pub fn entry_reachability(
    document: &Document,
    root: &Name,
) -> Result<EntryReachability, crate::Refusal> {
    let mut reached = EntryReachability::default();
    let mut pending = vec![root.clone()];
    while let Some(name) = pending.pop() {
        if !reached.functions.insert(name.clone()) {
            continue;
        }
        let function = document
            .functions
            .get(&name)
            .ok_or_else(|| crate::Refusal {
                errors: vec![refused(
                    None,
                    RefusalReason::Invalid(InvalidReason::FunctionNotDeclared { name }),
                )],
            })?;
        let mut references = BTreeSet::new();
        scan(
            &function.body,
            &mut reached.effects,
            &mut BTreeSet::new(),
            &mut references,
        );
        pending.extend(references);
    }
    Ok(reached)
}

/// A function reached, and the function whose body reached it; `None` when
/// the document's own code calls it.
pub(crate) type Reached = BTreeMap<FunctionId, Option<FunctionId>>;

pub(crate) fn reach(
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> (BTreeSet<EffectName>, Reached) {
    // A declared function's start adapter calls its functions when the
    // function is started, as an entry's convention does: they are reached,
    // with what they call.
    let started = document
        .header_functions()
        .into_iter()
        .map(|(_, function)| *function);
    reach_from(
        std::iter::once(&document.main)
            .chain(document.functions.values().map(|function| &function.body)),
        started,
        catalog,
    )
}

pub(crate) fn reach_blocks<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    catalog: &dyn FunctionCatalog,
) -> (BTreeSet<EffectName>, Reached) {
    reach_from(blocks, std::iter::empty(), catalog)
}

/// What `blocks` reach, and `roots`, functions reached besides their code.
fn reach_from<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    roots: impl IntoIterator<Item = FunctionId>,
    catalog: &dyn FunctionCatalog,
) -> (BTreeSet<EffectName>, Reached) {
    let mut effects = BTreeSet::new();
    let mut direct: BTreeSet<FunctionId> = roots.into_iter().collect();
    for block in blocks {
        scan(block, &mut effects, &mut direct, &mut BTreeSet::new());
    }
    let mut reached: Reached = direct.iter().map(|function| (*function, None)).collect();
    let mut pending: Vec<FunctionId> = direct.into_iter().collect();
    while let Some(function) = pending.pop() {
        if let Some(registered) = catalog.registered(&function) {
            for callee in registered.callees() {
                if let std::collections::btree_map::Entry::Vacant(entry) = reached.entry(*callee) {
                    entry.insert(Some(function));
                    pending.push(*callee);
                }
            }
            continue;
        }
        let Some(body) = catalog
            .definition(&function)
            .and_then(|definition| definition.body())
        else {
            continue;
        };
        let mut called = BTreeSet::new();
        scan(
            &body.block,
            &mut BTreeSet::new(),
            &mut called,
            &mut BTreeSet::new(),
        );
        for callee in called {
            if let std::collections::btree_map::Entry::Vacant(entry) = reached.entry(callee) {
                entry.insert(Some(function));
                pending.push(callee);
            }
        }
    }
    (effects, reached)
}

/// The name `function` goes by: its definition's, or the one the body that
/// reached it lists, or the manifest's.
fn name_of(
    function: &FunctionId,
    through: Option<&FunctionId>,
    document: &Document,
    catalog: &dyn FunctionCatalog,
) -> Option<FunctionName> {
    if let Some(definition) = catalog.definition(function) {
        return Some(definition.name.clone());
    }
    through
        .and_then(|through| catalog.definition(through))
        .and_then(|definition| definition.body())
        .and_then(|body| body.functions.get(function))
        .or_else(|| document.manifest.functions.get(function))
        .cloned()
}

/// Derives a document's requirements from its code and the bodies of the
/// library functions it reaches.
pub fn requirements(document: &Document, catalog: &dyn FunctionCatalog) -> Requirements {
    let (effects, reached) = reach(document, catalog);
    let functions = reached
        .iter()
        .filter_map(|(function, through)| {
            name_of(function, through.as_ref(), document, catalog).map(|name| (*function, name))
        })
        .collect();
    Requirements { effects, functions }
}

/// Every function the document needs that `catalog` does not hold: those
/// its manifest lists and those its code reaches.
pub(crate) fn missing_functions(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    reached: &Reached,
) -> Vec<Refused> {
    let mut missing = BTreeMap::new();
    for (function, through) in reached {
        if catalog.definition(function).is_none()
            && let Some(name) = name_of(function, through.as_ref(), document, catalog)
        {
            missing.insert(*function, name);
        }
    }
    // A manifest's spelling wins when both it and a caller name an absent
    // function, as in the combined requirements/manifest map.
    for (function, name) in &document.manifest.functions {
        if catalog.definition(function).is_none() {
            missing.insert(*function, name.clone());
        }
    }
    missing
        .into_iter()
        .map(|(function, name)| refused(None, RefusalReason::MissingFunction { function, name }))
        .collect()
}

/// Checks the manifest the document carries against the one its code
/// derives. A function the document's own code calls without listing is
/// structural validation's to refuse; this names the rest.
pub(crate) fn check(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    effects: &BTreeSet<EffectName>,
    reached: &Reached,
    errors: &mut Vec<Refused>,
) {
    for effect in document.manifest.effects.keys() {
        if !effects.contains(effect) {
            errors.push(refused(
                None,
                RefusalReason::EffectNotPerformed {
                    effect: effect.clone(),
                },
            ));
        }
    }
    for (function, listed) in &document.manifest.functions {
        if !reached.contains_key(function) {
            errors.push(refused(
                None,
                RefusalReason::FunctionNotReached {
                    function: *function,
                    name: listed.clone(),
                },
            ));
        }
        if let Some(definition) = catalog.definition(function)
            && &definition.name != listed
        {
            errors.push(refused(
                None,
                RefusalReason::FunctionName {
                    function: *function,
                    listed: listed.clone(),
                    defined: definition.name.clone(),
                },
            ));
        }
    }
    for (function, through) in reached {
        let Some(through) = through else {
            continue;
        };
        if document.manifest.functions.contains_key(function) {
            continue;
        }
        if let Some(name) = name_of(function, Some(through), document, catalog) {
            errors.push(refused(
                None,
                RefusalReason::FunctionNotListed {
                    function: *function,
                    name,
                    through: *through,
                },
            ));
        }
    }
}
