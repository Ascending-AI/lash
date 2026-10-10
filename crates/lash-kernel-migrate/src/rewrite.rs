//! What every document rewrite shares: restating the library functions a
//! document lists, replacing their identities, and the correspondence of a
//! rewrite that moves no node.

use std::collections::BTreeMap;

use lash_kernel_doc::{
    Action, Block, Callee, Document, Expr, FunctionCatalog, FunctionDefinition, FunctionId,
    Implementation, Member, Node, Place, Rhs, Site, Stmt, Unit,
};
use lash_kernel_edit::{Correspondence, Survivor};

use crate::migration::DocumentRefusal;

/// A library function redeclared for the next kernel version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redeclared {
    pub from: FunctionId,
    pub to: FunctionId,
    pub definition: FunctionDefinition,
}

/// Redeclares `functions` and every function their bodies call, each after
/// the functions it calls. `definition` redeclares one function for the next
/// version; the identities its body calls are replaced here.
pub fn redeclare(
    definition: fn(&FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal>,
    functions: impl IntoIterator<Item = FunctionId>,
    catalog: &dyn FunctionCatalog,
) -> Result<Vec<Redeclared>, DocumentRefusal> {
    let mut done = BTreeMap::new();
    let mut ordered = Vec::new();
    for function in functions {
        redeclare_one(definition, function, catalog, &mut done, &mut ordered)?;
    }
    Ok(ordered)
}

/// A body names what it calls by identity, a hash of the callee, so the
/// calls of a library form no cycle and the recursion ends.
fn redeclare_one(
    definition: fn(&FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal>,
    function: FunctionId,
    catalog: &dyn FunctionCatalog,
    done: &mut BTreeMap<FunctionId, FunctionId>,
    ordered: &mut Vec<Redeclared>,
) -> Result<FunctionId, DocumentRefusal> {
    if let Some(to) = done.get(&function) {
        return Ok(*to);
    }
    let Some(written) = catalog.definition(&function) else {
        return Err(DocumentRefusal::FunctionNotHeld { function });
    };
    let called: Vec<FunctionId> = written
        .body()
        .map(|body| body.functions.keys().copied().collect())
        .unwrap_or_default();
    for called in called {
        redeclare_one(definition, called, catalog, done, ordered)?;
    }
    let mut redeclared = definition(written)?;
    if let Implementation::Body(body) | Implementation::Both(body) = &mut redeclared.implementation
    {
        body.functions = std::mem::take(&mut body.functions)
            .into_iter()
            .map(|(id, name)| (done.get(&id).copied().unwrap_or(id), name))
            .collect();
        replace_in_block(&mut body.block, done);
    }
    let to = redeclared
        .identity()
        .map_err(|error| DocumentRefusal::Encode {
            message: error.message,
        })?;
    done.insert(function, to);
    ordered.push(Redeclared {
        from: function,
        to,
        definition: redeclared,
    });
    Ok(to)
}

/// Replaces each library function `document` lists and calls with the one
/// `functions` maps it to. A function the map does not name is kept.
pub fn replace_functions(document: &mut Document, functions: &BTreeMap<FunctionId, FunctionId>) {
    document.manifest.functions = std::mem::take(&mut document.manifest.functions)
        .into_iter()
        .map(|(id, name)| (functions.get(&id).copied().unwrap_or(id), name))
        .collect();
    replace_in_block(&mut document.main, functions);
    for function in document.functions.values_mut() {
        replace_in_block(&mut function.body, functions);
    }
}

pub(crate) fn replace_in_block(block: &mut Block, functions: &BTreeMap<FunctionId, FunctionId>) {
    for stmt in block {
        match stmt {
            Stmt::Let { value, .. } => replace_in_rhs(value, functions),
            Stmt::Assign { place, value } => {
                if let Place::Member(member) = place {
                    replace_in_member(member, functions);
                }
                replace_in_rhs(value, functions);
            }
            Stmt::Remove { member } => replace_in_member(member, functions),
            Stmt::Do { action } => replace_in_action(action, functions),
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                replace_in_expr(condition, functions);
                replace_in_block(then_block, functions);
                replace_in_block(else_block, functions);
            }
            Stmt::For { iterable, body, .. } => {
                replace_in_expr(iterable, functions);
                replace_in_block(body, functions);
            }
            Stmt::While { condition, body } => {
                replace_in_expr(condition, functions);
                replace_in_block(body, functions);
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Return { value }
            | Stmt::Throw { value }
            | Stmt::Print { value }
            | Stmt::Finish { value }
            | Stmt::Fail { value } => replace_in_expr(value, functions),
            Stmt::Try(scope) => {
                replace_in_block(&mut scope.body, functions);
                if let Some(catch) = &mut scope.catch {
                    replace_in_block(&mut catch.body, functions);
                }
                if let Some(finally) = &mut scope.finally {
                    replace_in_block(finally, functions);
                }
            }
        }
    }
}

fn replace_in_rhs(rhs: &mut Rhs, functions: &BTreeMap<FunctionId, FunctionId>) {
    match rhs {
        Rhs::Expr(expr) => replace_in_expr(expr, functions),
        Rhs::Action(action) => replace_in_action(action, functions),
    }
}

fn replace_in_action(action: &mut Action, functions: &BTreeMap<FunctionId, FunctionId>) {
    if let Action::Call { callee, .. } | Action::Spawn { callee, .. } = action
        && let Callee::Library(function) = callee
        && let Some(to) = functions.get(function)
    {
        *function = *to;
    }
}

fn replace_in_member(member: &mut Member, functions: &BTreeMap<FunctionId, FunctionId>) {
    match member {
        Member::Field { target, .. } => replace_in_expr(target, functions),
        Member::Index { target, index } => {
            replace_in_expr(target, functions);
            replace_in_expr(index, functions);
        }
    }
}

fn replace_in_expr(expr: &mut Expr, functions: &BTreeMap<FunctionId, FunctionId>) {
    match expr {
        Expr::Literal(_) | Expr::Variable(_) | Expr::Clock | Expr::Random => {}
        Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => {
            for item in items {
                replace_in_expr(item, functions);
            }
        }
        Expr::Map(entries) => {
            for entry in entries {
                replace_in_expr(&mut entry.key, functions);
                replace_in_expr(&mut entry.value, functions);
            }
        }
        Expr::Record(entries) => {
            for entry in entries {
                replace_in_expr(&mut entry.value, functions);
            }
        }
        Expr::Member(member) => replace_in_member(member, functions),
        Expr::Closure(closure) => replace_in_block(&mut closure.body, functions),
        Expr::Call { function, args } => {
            if let Some(to) = functions.get(function) {
                *function = *to;
            }
            for arg in args {
                replace_in_expr(arg, functions);
            }
        }
        Expr::Read(read) => {
            replace_in_expr(&mut read.handle, functions);
            replace_in_expr(&mut read.request, functions);
        }
    }
}

/// The correspondence of a rewrite that moved no node of `main` or of a
/// declared function: every node of `base` to the same site of `result`.
/// A node of a library function's body is not listed.
pub fn unchanged(base: &Document, result: &Document) -> Result<Correspondence, DocumentRefusal> {
    Correspondence::of(
        document_identity(base)?,
        document_identity(result)?,
        unmoved(base),
    )
    .ok_or(DocumentRefusal::Correspondence)
}

pub(crate) fn document_identity(
    document: &Document,
) -> Result<lash_kernel_doc::DocumentId, DocumentRefusal> {
    document
        .identity()
        .map_err(|error| DocumentRefusal::Encode {
            message: error.message,
        })
}

/// Every node of `base`'s `main` and declared functions, at the same site.
pub(crate) fn unmoved(base: &Document) -> Vec<Survivor> {
    let mut entries = Vec::new();
    same_sites(Unit::Main, Unit::Main, &base.main, &mut entries);
    for (name, function) in &base.functions {
        let unit = Unit::Function(name.clone());
        same_sites(unit.clone(), unit, &function.body, &mut entries);
    }
    entries
}

/// Lists every node of `body`, the code of `from`, as at the same path of
/// `to`.
pub(crate) fn same_sites(from: Unit, to: Unit, body: &Block, entries: &mut Vec<Survivor>) {
    let mut pending = vec![(Vec::new(), Node::Block(body))];
    while let Some((path, node)) = pending.pop() {
        for (index, child) in (0u32..).zip(node.children()) {
            let mut child_path = path.clone();
            child_path.push(index);
            pending.push((child_path, child));
        }
        entries.push(Survivor {
            from: Site::new(from.clone(), path.clone()),
            to: Site::new(to.clone(), path),
            edited: false,
        });
    }
}
