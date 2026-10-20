//! Lowers a parsed Python module to a kernel document.
//!
//! Calls, waits and helper operations nest in their author expressions. An
//! operand is pinned only when a later operand emits statements (§12 of
//! FIG-6402 rev2). Python locals are declared where their first binding
//! dominates every use and capture; forward captures and recursion retain
//! their absent slot at the scope root.

mod calls;
mod captions;
mod entries;
mod expressions;
mod format;
mod lazy;
mod range;
mod repairs;
mod saved;
mod statements;
mod typed_dicts;

use lash_kernel_doc::Span;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use lash_kernel_dialect::{
    CellEnvironment, Diagnostic, Environment, Library, Lowered, Mode, workflow,
};
use lash_kernel_doc::{
    ActExpr, ActKind, AssignOrder, Callee, Catch, Closure, Document, EffectName, Expr,
    FunctionCatalog, FunctionId, FunctionName, Literal, Name, NumberPolicy, Place, Rhs, Stmt,
    TryStmt, validate_document,
};
use ruff_python_ast::{self as ast};
use ruff_text_size::{Ranged, TextRange};

use crate::diagnostics::{self, Code};
use crate::exceptions::Classes;
use crate::scope::{self, Bindings, Ty};

pub(crate) type Lowering<T> = Result<T, Diagnostic>;

pub(crate) use lash_kernel_dialect::Note;

/// Kernel statements with their notes.
#[derive(Debug, Default)]
pub(crate) struct Buf {
    stmts: Vec<Stmt>,
    notes: Vec<Note>,
}

impl Buf {
    fn is_empty(&self) -> bool {
        self.stmts.is_empty()
    }
}

/// A lowered Python expression, what is known of its type, and whether later
/// statements can change what it evaluates to.
#[derive(Clone, Debug)]
pub(crate) struct Operand {
    pub(crate) expr: Expr,
    pub(crate) ty: Ty,
    /// A literal or a temporary: reading it later gives the same value.
    stable: bool,
}

impl Operand {
    pub(crate) fn literal(literal: Literal, ty: Ty) -> Self {
        Self {
            expr: Expr::Literal(literal),
            ty,
            stable: true,
        }
    }

    pub(crate) fn none() -> Self {
        Self::literal(Literal::Null, Ty::None)
    }

    pub(crate) fn text(value: impl Into<String>) -> Self {
        Self::literal(Literal::Text(value.into()), Ty::Str)
    }

    pub(crate) fn inline(expr: Expr, ty: Ty) -> Self {
        Self {
            expr,
            ty,
            stable: false,
        }
    }

    fn temp(name: Name, ty: Ty) -> Self {
        Self {
            expr: Expr::Variable(name),
            ty,
            stable: true,
        }
    }
}

enum ScopeKind {
    Module,
    Function { is_async: bool },
}

/// One Python scope being lowered: the module or a function.
struct Scope {
    kind: ScopeKind,
    bindings: Bindings,
    /// Locals whose every definition is integer-producing. Boundness is
    /// checked separately at each read; these facts are never serialized.
    integer_locals: BTreeSet<String>,
    sequence_locals: BTreeMap<String, Ty>,
    /// Names that hold a value at this point, one frame per kernel block
    /// open in this scope. A read of a name in none of them is checked.
    bound: Vec<BTreeSet<String>>,
    /// The exceptions being handled where the lowering is, innermost last:
    /// what a bare `raise` raises again.
    handlers: Vec<Active>,
    /// How many loops the lowering is inside, in this scope.
    loops: usize,
}

/// The notes of a loop's blocks: its body's, then its `else`'s when the
/// `else` is a child, which an empty one is not.
fn loop_blocks(body: Vec<Note>, orelse: &Buf) -> Vec<Vec<Note>> {
    let mut blocks = vec![body];
    if !orelse.stmts.is_empty() {
        blocks.push(orelse.notes.clone());
    }
    blocks
}

/// The exception a bare `raise` raises again.
#[derive(Clone)]
enum Active {
    /// The value an `except` clause caught.
    Caught(Name),
    /// A variable that holds the exception being handled when the code
    /// runs, or absent when none is: a function's hidden parameter, or what
    /// a `finally` runs under.
    Passed(Name),
}

impl Active {
    fn name(&self) -> &Name {
        match self {
            Active::Caught(name) | Active::Passed(name) => name,
        }
    }
}

/// A Python variable as the kernel names it.
struct Found {
    name: Name,
    /// The scope that owns it; none for a comprehension's variable.
    scope: Option<usize>,
}

pub(crate) struct Lowerer<'a> {
    source: &'a str,
    calls: Option<&'a mut BTreeSet<String>>,
    library: &'a dyn Library,
    /// The packages and offered operations a dotted call resolves through.
    /// An operation that declares controls ends the turn: a control call
    /// ends `main` when it settles, and is written only where it can.
    namespaces: &'a lash_kernel_dialect::Namespaces,
    performed: BTreeMap<EffectName, lash_kernel_doc::OperationRequirement>,
    used: BTreeMap<FunctionId, FunctionName>,
    private: BTreeSet<Name>,
    taken: HashSet<String>,
    scopes: Vec<Scope>,
    /// Comprehension variables, which Python scopes to the comprehension.
    renames: Vec<(String, Name)>,
    /// The comprehension variables a read may meet before their clause
    /// binds them.
    unbound_renames: BTreeSet<Name>,
    classes: Classes,
    rebound: BTreeSet<String>,
    buf: Buf,
    kernel_depth: usize,
    span: Option<Span>,
    nesting: usize,
    max_nesting: usize,
    saved: &'a BTreeMap<Name, lash_kernel_dialect::SavedFunction>,
    saved_used: BTreeSet<Name>,
    declared: BTreeMap<Name, lash_kernel_doc::Function>,
    /// The notes of each declared function's body.
    declared_notes: BTreeMap<Name, Vec<Note>>,
    /// The entries of a workflow module, as they are lowered; `None` for a
    /// cell.
    module: Option<workflow::Module>,
    /// The module's entry declarations: its top-level `async def`s.
    entry_defs: HashSet<TextRange>,
    /// Whether an entry's body is being lowered, where no name of the
    /// module resolves.
    in_entry: bool,
    creates_tasks: bool,
    /// Inline deferred calls retain their body notes without a statement binding.
    inline_closures: Vec<(Closure, Vec<Note>)>,
    /// The first module name an entry's body named, with where, which the
    /// entry is refused for.
    captured: std::cell::RefCell<Option<(String, Option<Span>)>>,
    /// The captions the source spells, by where what each captions starts.
    captions: BTreeMap<usize, lash_kernel_doc::Caption>,
    /// The captions of the module's entries, their parameters and their
    /// types' fields.
    declared_captions: Vec<lash_kernel_doc::CaptionEntry>,
    /// The `TypedDict`s a workflow module declares, by name.
    typed_dicts: typed_dicts::TypedDicts,
}

/// Whether `name` is one of the dialect's built-ins.
pub(crate) fn is_builtin(name: &str) -> bool {
    calls::is_builtin(name) || crate::exceptions::is_builtin(name)
}

/// No module or restored session binding may mask the dialect's built-ins,
/// nor `control`. Package roots obey lexical shadowing.
pub(crate) fn check_binding_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Lowering<()> {
    for name in names {
        let message = if calls::is_builtin(name) || crate::exceptions::is_builtin(name) {
            format!("`{name}` is a built-in; a top-level binding cannot reuse its name")
        } else if name == "control" {
            format!(
                "`{name}` names one of the session's tools; a top-level binding cannot reuse its name"
            )
        } else {
            continue;
        };
        let mut error = diagnostics::unplaced(Code::ShadowsBuiltin, message);
        error.repairs.push(format!(
            "rename `{name}` to `{name}_` and update its references"
        ));
        return Err(error);
    }
    Ok(())
}

/// Lowers a parsed module against `environment`.
pub(crate) fn lower_with_calls(
    module: &ast::ModModule,
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    max_nesting: usize,
    calls: Option<&mut BTreeSet<String>>,
    spelled: crate::captions::Spelled,
) -> Lowering<Lowered> {
    let (cell, workflow) = match mode {
        Mode::Cell(cell) => (cell, None),
        Mode::Module => (CellEnvironment::first(), Some(workflow::Module::new())),
    };
    let entry_defs = if workflow.is_some() {
        entries::declarations(&module.body)
    } else {
        HashSet::new()
    };
    // Fresh names avoid every word of the code, but not of `#:` comments:
    // a caption changes no name (`K-CAP-001`).
    let mut code = Vec::with_capacity(spelled.comments.len() + 1);
    let mut from = 0;
    for comment in &spelled.comments {
        code.push(&source[from..comment.start]);
        from = comment.end;
    }
    code.push(&source[from..]);
    let mut taken: HashSet<String> = code
        .into_iter()
        .flat_map(|code| code.split(|c: char| !(c.is_alphanumeric() || c == '_')))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    taken.extend(cell.bindings.iter().map(|name| name.as_str().to_string()));
    let (globals, nonlocals) = scope::rebound(&module.body);
    let rebound: BTreeSet<String> = globals.union(&nonlocals).cloned().collect();
    let mut bindings = scope::bindings(&module.body, None, &rebound, &BTreeSet::new());
    for (name, saved) in cell.functions {
        if !bindings.locals.contains(name.as_str())
            && let Some(signature) = saved::call_signature(saved)
        {
            bindings.defs.insert(name.to_string(), signature);
        }
    }
    // A `global` statement anywhere makes the name the module's, and an
    // earlier cell's bindings are the module's too.
    bindings.locals.extend(globals);
    check_binding_names(bindings.locals.iter().map(String::as_str))?;
    for statement in &module.body {
        if let ast::Stmt::ClassDef(class) = statement {
            check_binding_names([class.name.id.as_str()])?;
        }
    }
    let declared: Vec<String> = bindings
        .locals
        .iter()
        .filter(|name| !cell.bindings.contains(&Name::new(name.as_str())))
        .cloned()
        .collect();
    bindings
        .locals
        .extend(cell.bindings.iter().map(|name| name.as_str().to_string()));
    let mut excluded = rebound.clone();
    excluded.extend(cell.bindings.iter().map(ToString::to_string));
    // A saved function may write module bindings from an earlier cell.
    // Its body is outside this local analysis.
    if !cell.functions.is_empty() {
        excluded.extend(bindings.locals.iter().cloned());
    }
    let integer_locals = scope::integer_locals(&module.body, &bindings, &excluded, true);
    let sequence_locals = scope::sequence_locals(&module.body, &bindings, &excluded, true);
    let mut lowerer = Lowerer {
        source,
        calls,
        library: environment.library,
        namespaces: environment.namespaces,
        performed: BTreeMap::new(),
        used: BTreeMap::new(),
        private: BTreeSet::new(),
        taken,
        scopes: vec![Scope {
            kind: ScopeKind::Module,
            bindings,
            integer_locals,
            sequence_locals,
            bound: vec![BTreeSet::new()],
            handlers: Vec::new(),
            loops: 0,
        }],
        renames: Vec::new(),
        unbound_renames: BTreeSet::new(),
        classes: Classes::builtin(),
        rebound,
        buf: Buf::default(),
        kernel_depth: 0,
        span: None,
        nesting: 0,
        max_nesting,
        saved: cell.functions,
        saved_used: BTreeSet::new(),
        declared: BTreeMap::new(),
        declared_notes: BTreeMap::new(),
        module: workflow,
        entry_defs,
        in_entry: false,
        creates_tasks: false,
        inline_closures: Vec::new(),
        captured: std::cell::RefCell::new(None),
        captions: spelled.captions,
        declared_captions: Vec::new(),
        typed_dicts: typed_dicts::TypedDicts::new(),
    };
    lowerer.declare_classes(&module.body)?;
    for name in &declared {
        lowerer.emit(Stmt::Let {
            name: Name::new(name.as_str()),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
    }
    let body = lowerer.block(|this| this.statements(&module.body))?;
    let body = lowerer.task_cleanup(body)?;
    lowerer.append(body);
    let main = std::mem::take(&mut lowerer.buf);
    // Judge dominance in the final scopes: a slot carried after cleanup
    // must be declared outside its try, and after a loop outside that loop.
    let main = lowerer.finish_declarations(
        main,
        lowerer
            .module
            .is_none()
            .then_some(&lowerer.scopes[0].bindings.locals),
    );
    let mut document = Document::new(NumberPolicy::BySpelling, main.stmts);
    document.private_bindings = lowerer.private;
    document.functions = lowerer.declared;
    if let Some(module) = lowerer.module.take() {
        module
            .assemble(&mut document, &mut lowerer.declared_notes)
            .map_err(workflow::ModuleError::into_diagnostic)?;
    }
    document.manifest.functions = reachable(lowerer.used, environment.library);
    document.manifest.effects = lowerer.performed;
    lash_kernel_dialect::require_through_bodies(&mut document.manifest, environment.library);
    if !lowerer.saved_used.is_empty() {
        let catalog: &dyn FunctionCatalog = environment.library;
        lash_kernel_dialect::install(
            &mut document,
            cell.functions,
            &lowerer.saved_used,
            &lash_kernel_doc::FunctionRepresentation::Bare,
            environment.namespaces,
            &|function| catalog.definition(function).is_some(),
        ).map_err(|unusable| {
            let mut error = diagnostics::unplaced(Code::SavedFunctionUnusable, unusable.to_string());
            error.repairs.push("provide the saved function's required tools and library functions, or define a function using this session's tools".to_owned());
            error
        })?;
        document.manifest.functions = reachable(
            std::mem::take(&mut document.manifest.functions),
            environment.library,
        );
        lash_kernel_dialect::require_through_bodies(&mut document.manifest, environment.library);
    }
    validate_document(&document, environment.library).map_err(|invalid| {
        diagnostics::unplaced(
            Code::InvalidDocument,
            format!("the program lowers to a document the kernel refuses: {invalid}"),
        )
    })?;
    let document = lash_kernel_doc::IdentifiedDocument::new(document);
    let lash_kernel_dialect::SiteLayers {
        source_map,
        mut written,
        captions,
    } = lash_kernel_dialect::site_layers(
        &document,
        source,
        lash_kernel_doc::SourceLanguage::Python,
        &main.notes,
        &lowerer.declared_notes,
        &lowerer.declared_captions,
    )
    .map_err(|refused| diagnostics::unplaced(Code::InvalidDocument, refused.to_string()))?;
    for function in cell.functions.values() {
        if document.functions.contains_key(&function.name)
            && let Some(metadata) = &function.written
        {
            written.insert(
                lash_kernel_doc::Site::new(
                    lash_kernel_doc::Unit::Function(function.name.clone()),
                    Vec::new(),
                ),
                metadata.clone(),
            );
        }
    }
    Ok(Lowered {
        document,
        source_map: Some(source_map),
        written,
        captions,
    })
}

/// The functions a document lists: those it calls and those their bodies
/// reach (`K-DOC-002`).
fn reachable(
    used: BTreeMap<FunctionId, FunctionName>,
    library: &dyn Library,
) -> BTreeMap<FunctionId, FunctionName> {
    let mut listed = BTreeMap::new();
    let mut pending: Vec<(FunctionId, FunctionName)> = used.into_iter().collect();
    while let Some((function, name)) = pending.pop() {
        let std::collections::btree_map::Entry::Vacant(entry) = listed.entry(function) else {
            continue;
        };
        entry.insert(name);
        let catalog: &dyn FunctionCatalog = library;
        // A registered function lists its callees without being decoded.
        if let Some(registered) = catalog.registered(&function) {
            pending.extend(registered.listed());
        } else if let Some(body) = catalog
            .definition(&function)
            .and_then(|definition| definition.body())
        {
            // An identity digests its function's name, so a function already
            // listed is listed by the name any other body calls it by.
            pending.extend(
                body.functions
                    .iter()
                    .filter(|(function, _)| !listed.contains_key(*function))
                    .map(|(function, name)| (*function, name.clone())),
            );
        }
    }
    listed
}

impl Lowerer<'_> {
    /// Declaration dominance (FIG-6402 rev2 §11): a straight-line first
    /// assignment may declare the slot, unless its RHS or an earlier node
    /// reads or captures it. Nested assignments cannot declare an outer slot.
    /// A cell's carried names are also used after its root block finishes.
    fn finish_declarations(&self, mut body: Buf, carried: Option<&BTreeSet<String>>) -> Buf {
        /// Whether `action` reads `name`, as the expression it stands for
        /// would: as its callee or as one of its arguments.
        fn action_mentions(action: &lash_kernel_doc::Action, name: &Name) -> bool {
            use lash_kernel_doc::{Action, Atom};
            let (callee, atoms): (Option<&Callee>, &[Atom]) = match action {
                Action::Call { callee, args } | Action::Spawn { callee, args } => {
                    (Some(callee), args)
                }
                Action::Perform { args, .. } => (None, args),
                Action::Sleep { duration: atom, .. }
                | Action::Join { task: atom }
                | Action::JoinMany { tasks: atom, .. }
                | Action::Await { value: atom, .. }
                | Action::Cancel { task: atom } => (None, std::slice::from_ref(atom)),
                Action::Yield => (None, &[]),
            };
            matches!(callee, Some(Callee::Value(read)) if read == name)
                || atoms
                    .iter()
                    .any(|atom| matches!(atom, Atom::Variable(read) if read == name))
        }
        fn mentions(node: lash_kernel_doc::Node<'_>, name: &Name) -> bool {
            match node {
                lash_kernel_doc::Node::Expr(Expr::Variable(read)) if read == name => true,
                lash_kernel_doc::Node::Expr(Expr::Act(act)) if matches!(&act.kind, ActKind::Call(Callee::Value(read)) | ActKind::Spawn(Callee::Value(read)) if read == name) => {
                    true
                }
                lash_kernel_doc::Node::Stmt(Stmt::Assign {
                    place: Place::Variable(write),
                    ..
                }) if write == name => true,
                lash_kernel_doc::Node::Action(action) => action_mentions(action, name),
                _ => node
                    .children()
                    .into_iter()
                    .any(|child| mentions(child, name)),
            }
        }
        let mut remove = BTreeSet::new();
        for index in 0..body.stmts.len() {
            let Stmt::Let {
                name,
                value: Rhs::Expr(Expr::Literal(Literal::Absent)),
            } = &body.stmts[index]
            else {
                continue;
            };
            let name = name.clone();
            for next in index + 1..body.stmts.len() {
                if let Stmt::Assign {
                    place: Place::Variable(write),
                    value,
                    ..
                } = &body.stmts[next]
                    && write == &name
                {
                    let mentioned = match value {
                        Rhs::Expr(value) => mentions(lash_kernel_doc::Node::Expr(value), &name),
                        Rhs::Action(action) => action_mentions(action, &name),
                    };
                    if !mentioned {
                        body.stmts[next] = Stmt::Let {
                            name: name.clone(),
                            value: value.clone(),
                        };
                        remove.insert(index);
                    }
                    break;
                }
                if let Stmt::For {
                    binding,
                    iterable,
                    body: pass,
                    else_block,
                    ..
                } = &body.stmts[next]
                    && !carried.is_some_and(|names| names.contains(name.as_str()))
                    && !mentions(lash_kernel_doc::Node::Expr(iterable), &name)
                    && !mentions(lash_kernel_doc::Node::Block(else_block), &name)
                    && matches!(pass.first(), Some(Stmt::Assign {
                        place: Place::Variable(write), value: Rhs::Expr(Expr::Variable(read)), ..
                    }) if write == &name && read == binding)
                    && !body.stmts[next + 1..]
                        .iter()
                        .any(|stmt| mentions(lash_kernel_doc::Node::Stmt(stmt), &name))
                {
                    fn captured(node: lash_kernel_doc::Node<'_>, name: &Name) -> bool {
                        match node {
                            lash_kernel_doc::Node::Expr(Expr::Closure(_)) => mentions(node, name),
                            _ => node
                                .children()
                                .into_iter()
                                .any(|child| captured(child, name)),
                        }
                    }
                    if !captured(lash_kernel_doc::Node::Block(pass), &name) {
                        if let Stmt::For {
                            binding,
                            body: pass,
                            ..
                        } = &mut body.stmts[next]
                        {
                            *binding = name.clone();
                            pass.remove(0);
                        }
                        // Skip any closure blocks in the iterable to locate
                        // the loop body's notes. Leave the else suite untouched.
                        let mut expression_blocks = 0;
                        let mut pending = match &body.stmts[next] {
                            Stmt::For { iterable, .. } => {
                                vec![lash_kernel_doc::Node::Expr(iterable)]
                            }
                            _ => Vec::new(),
                        };
                        while let Some(node) = pending.pop() {
                            for child in node.children() {
                                if matches!(child, lash_kernel_doc::Node::Block(_)) {
                                    expression_blocks += 1;
                                } else {
                                    pending.push(child);
                                }
                            }
                        }
                        if let Some(notes) = body.notes[next].blocks.get_mut(expression_blocks) {
                            notes.remove(0);
                        }
                        remove.insert(index);
                    }
                    break;
                }
                if mentions(lash_kernel_doc::Node::Stmt(&body.stmts[next]), &name) {
                    break;
                }
            }
        }
        let mut index = 0;
        body.stmts.retain(|_| {
            let keep = !remove.contains(&index);
            index += 1;
            keep
        });
        let mut index = 0;
        body.notes.retain(|_| {
            let keep = !remove.contains(&index);
            index += 1;
            keep
        });
        body
    }

    // Names.

    /// A name the source does not spell and nothing has taken.
    fn fresh(&mut self, base: &str) -> Name {
        let mut number = 1usize;
        loop {
            let candidate = format!("{base}{number}");
            if self.taken.insert(candidate.clone()) {
                return Name::new(candidate);
            }
            number += 1;
        }
    }

    pub(crate) fn temp(&mut self) -> Name {
        self.fresh("t")
    }

    /// The innermost scope. The module's is never popped.
    fn scope(&self) -> &Scope {
        &self.scopes[self.scopes.len() - 1]
    }

    fn scope_mut(&mut self) -> &mut Scope {
        let innermost = self.scopes.len() - 1;
        &mut self.scopes[innermost]
    }

    fn in_module(&self) -> bool {
        self.scopes.len() == 1
    }

    fn in_async(&self) -> bool {
        match self.scope().kind {
            // A cell runs as the body of a coroutine does: it may await.
            ScopeKind::Module => true,
            ScopeKind::Function { is_async } => is_async,
        }
    }

    /// The variable a Python name stands for here, if it is one.
    fn variable(&self, id: &str) -> Option<Found> {
        if let Some((_, renamed)) = self.renames.iter().rev().find(|(name, _)| name == id) {
            return Some(Found {
                name: renamed.clone(),
                scope: None,
            });
        }
        let mut index = self.scopes.len() - 1;
        while index > 0 {
            let bindings = &self.scopes[index].bindings;
            if bindings.globals.contains(id) {
                index = 0;
                break;
            }
            if bindings.locals.contains(id) {
                break;
            }
            index -= 1;
        }
        if index == 0 && self.in_entry {
            // An entry runs apart from its module: a module name is
            // captured, which the entry is refused for.
            if self.scopes[0].bindings.locals.contains(id) {
                self.captured
                    .borrow_mut()
                    .get_or_insert_with(|| (id.to_owned(), self.span));
            }
            return None;
        }
        self.scopes[index]
            .bindings
            .locals
            .contains(id)
            .then(|| Found {
                name: Name::new(id),
                scope: Some(index),
            })
    }

    fn is_bound(&self, scope: usize, id: &str) -> bool {
        self.scopes[scope]
            .bound
            .iter()
            .any(|frame| frame.contains(id))
    }

    /// Records that `id` may hold no value from here on, in every block of
    /// the scope that owns it.
    fn mark_unbound(&mut self, id: &str) {
        if let Some(scope) = self.variable(id).and_then(|found| found.scope) {
            for frame in &mut self.scopes[scope].bound {
                frame.remove(id);
            }
        }
    }

    /// The exception being handled where the lowering is, for a call that
    /// passes it on: absent where none is.
    fn active(&self) -> Operand {
        match self.scope().handlers.last() {
            Some(active) => Operand::temp(active.name().clone(), Ty::Unknown),
            None => Operand::literal(Literal::Absent, Ty::Unknown),
        }
    }

    /// Records that `id` holds a value from here to the end of the block.
    fn mark_bound(&mut self, id: &str) {
        let current = self.scopes.len() - 1;
        if self.variable(id).and_then(|found| found.scope) == Some(current)
            && let Some(frame) = self.scope_mut().bound.last_mut()
        {
            frame.insert(id.to_string());
        }
    }

    /// An inferred type fact applies only to a bound local of the current
    /// scope. An enclosing scope may run differently when a closure is
    /// called, and a branch-local definition may not reach this read.
    fn type_of(&self, found: &Found, id: &str) -> Ty {
        let annotated = found
            .scope
            .and_then(|scope| self.scopes[scope].bindings.types.get(id).copied())
            .unwrap_or(Ty::Unknown);
        // Explicit annotations retain the dialect's existing trust rule.
        if annotated != Ty::Unknown {
            return annotated;
        }
        self.local_type(found, id)
    }

    /// Facts proved from definitions, independently of annotations.
    fn local_type(&self, found: &Found, id: &str) -> Ty {
        if let Some(scope) = found.scope
            && scope == self.scopes.len() - 1
            && self.is_bound(scope, id)
        {
            if self.scopes[scope].integer_locals.contains(id) {
                return Ty::Int;
            }
            if let Some(ty) = self.scopes[scope].sequence_locals.get(id) {
                return *ty;
            }
        }
        Ty::Unknown
    }

    /// The function `id` names, when a `def` is all that binds it.
    fn known_def(&self, id: &str) -> Option<lash_kernel_dialect::CallShape> {
        let found = self.variable(id)?;
        self.scopes[found.scope?].bindings.defs.get(id).cloned()
    }

    /// The functions of every scope open here that take the exception
    /// their caller handles.
    fn reraisers(&self) -> BTreeSet<String> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.bindings.reraisers.iter().cloned())
            .collect()
    }

    /// Whether the function `id` names, when a `def` is all that binds it,
    /// takes the exception its caller handles as its last parameter.
    fn takes_active(&self, id: &str) -> bool {
        self.variable(id)
            .and_then(|found| found.scope)
            .is_some_and(|scope| self.scopes[scope].bindings.reraisers.contains(id))
    }

    /// A read of the name `id`.
    fn read(&mut self, id: &str, range: TextRange) -> Lowering<Operand> {
        let Some(found) = self.variable(id) else {
            return Err(self.not_a_variable(id, range));
        };
        if found.scope == Some(0) && !self.is_bound(0, id) && self.saved.contains_key(&found.name) {
            self.saved_used.insert(found.name.clone());
        }
        let ty = self.type_of(&found, id);
        if let Some(scope) = found.scope
            && !self.is_bound(scope, id)
        {
            let local = Operand::literal(Literal::Bool(scope != 0), Ty::Bool);
            let value = Operand::temp(found.name.clone(), ty);
            let checked = self.invoke("py.name_check", &[value, Operand::text(id), local], ty)?;
            self.mark_bound(id);
            return Ok(checked);
        }
        // A comprehension's variable read in a clause before the one that
        // binds it.
        if found.scope.is_none() && self.unbound_renames.contains(&found.name) {
            let local = Operand::literal(Literal::Bool(true), Ty::Bool);
            let value = Operand::temp(found.name.clone(), ty);
            return self.invoke("py.name_check", &[value, Operand::text(id), local], ty);
        }
        // A name that one `def` binds holds that function from then on.
        if self.known_def(id).is_some() {
            return Ok(Operand::temp(found.name, ty));
        }
        Ok(Operand::inline(Expr::Variable(found.name), ty))
    }

    /// The variable an assignment to `id` writes.
    fn written(&mut self, id: &str, range: TextRange) -> Lowering<Name> {
        match self.variable(id) {
            Some(found) => Ok(found.name),
            None => Err(diagnostics::diagnostic(
                Code::UnknownName,
                format!("`{id}` is not a variable of an enclosing scope"),
                range,
            )),
        }
    }

    // Attribute/method dispatch can refuse before evaluating its receiver.
    // Recognize a missing printed namespace there as well as in plain reads.
    fn printed_receiver(&self, receiver: &ast::Expr) -> Option<Diagnostic> {
        let ast::Expr::Name(name) = receiver else {
            return None;
        };
        if self.variable(name.id.as_str()).is_some() {
            return None;
        }
        diagnostics::printed_source_name(name.id.as_str(), name.range)
    }

    fn not_a_variable(&self, id: &str, range: TextRange) -> Diagnostic {
        if self.typed_dicts.contains_key(id) {
            return diagnostics::with_repair(
                Code::ClassUnsupported,
                format!("the TypedDict `{id}` is a type, not a value in this dialect"),
                range,
                format!(
                    "annotate an entry's parameter or result with `{id}`; build the value as a dict"
                ),
            );
        }
        if self.classes.contains(id) {
            return diagnostics::with_repair(
                Code::ExceptionClass,
                format!("the exception class `{id}` is not a value in this dialect"),
                range,
                format!("name it in `raise {id}()` or `except {id}`"),
            );
        }
        if calls::is_builtin(id) {
            return diagnostics::with_repair(
                Code::BuiltinAsValue,
                format!("the built-in `{id}` is called by name, not passed as a value"),
                range,
                &if calls::is_supported_builtin(id) {
                    format!("wrap `{id}` in a function and call it with its required arguments")
                } else {
                    format!("replace `{id}` with a supported built-in or an ordinary function")
                },
            );
        }
        if let Some(refusal) = diagnostics::printed_source_name(id, range) {
            return refusal;
        }
        diagnostics::diagnostic(
            Code::UnknownName,
            format!("name `{id}` is not defined"),
            range,
        )
    }

    // Emission.

    fn push(&mut self, mut stmt: Stmt, mut blocks: Vec<Vec<Note>>) {
        if let Stmt::Let { value, .. } | Stmt::Assign { value, .. } = &mut stmt
            && let Rhs::Expr(Expr::Act(action)) = value
            && let Some(action) = action.as_action()
        {
            *value = Rhs::Action(action);
        }
        if let Stmt::Let { name, .. } = &stmt
            && self.kernel_depth == 0
            && !self.scopes[0].bindings.locals.contains(name.as_str())
        {
            self.private.insert(name.clone());
        }
        fn nested<'a>(
            node: lash_kernel_doc::Node<'a>,
            out: &mut Vec<(&'a [Stmt], Option<&'a Closure>)>,
        ) {
            if let lash_kernel_doc::Node::Expr(Expr::Closure(closure)) = node {
                out.push((&closure.body, Some(closure)));
                return;
            }
            for child in node.children() {
                if let lash_kernel_doc::Node::Block(body) = child {
                    out.push((body, None));
                } else {
                    nested(child, out);
                }
            }
        }
        let mut nested_blocks = Vec::new();
        nested(lash_kernel_doc::Node::Stmt(&stmt), &mut nested_blocks);
        if nested_blocks.len() > blocks.len() {
            let mut supplied = blocks.into_iter();
            blocks = nested_blocks
                .into_iter()
                .map(|(_, closure)| {
                    closure
                        .and_then(|closure| {
                            self.inline_closures
                                .iter()
                                .find(|(known, _)| known == closure)
                                .map(|(_, notes)| notes.clone())
                        })
                        .unwrap_or_else(|| supplied.next().unwrap_or_default())
                })
                .collect();
        }
        self.buf.stmts.push(stmt);
        self.buf.notes.push(Note {
            span: self.span,
            blocks,
            ..Note::default()
        });
    }

    pub(crate) fn emit(&mut self, stmt: Stmt) {
        self.push(stmt, Vec::new());
    }

    /// Lowers into a new kernel block, which what it binds does not
    /// outlive, and returns it.
    fn block(&mut self, lower: impl FnOnce(&mut Self) -> Lowering<()>) -> Lowering<Buf> {
        let outer = std::mem::take(&mut self.buf);
        self.kernel_depth += 1;
        self.scope_mut().bound.push(BTreeSet::new());
        let result = lower(self);
        self.scope_mut().bound.pop();
        self.kernel_depth -= 1;
        let inner = std::mem::replace(&mut self.buf, outer);
        result.map(|()| inner)
    }

    /// Lowers into a buffer of its own that joins the current block.
    fn aside<T>(&mut self, lower: impl FnOnce(&mut Self) -> Lowering<T>) -> Lowering<(Buf, T)> {
        let outer = std::mem::take(&mut self.buf);
        let result = lower(self);
        let inner = std::mem::replace(&mut self.buf, outer);
        result.map(|value| (inner, value))
    }

    fn append(&mut self, buf: Buf) {
        if self.kernel_depth == 0 {
            for stmt in &buf.stmts {
                if let Stmt::Let { name, .. } = stmt
                    && !self.scopes[0].bindings.locals.contains(name.as_str())
                {
                    self.private.insert(name.clone());
                }
            }
        }
        self.buf.stmts.extend(buf.stmts);
        self.buf.notes.extend(buf.notes);
    }

    fn emit_if(&mut self, condition: Expr, then_block: Buf, else_block: Buf) {
        self.push(
            Stmt::If {
                condition,
                then_block: then_block.stmts,
                else_block: else_block.stmts,
            },
            vec![then_block.notes, else_block.notes],
        );
    }

    fn emit_while(&mut self, condition: Expr, body: Buf, orelse: Buf) {
        let blocks = loop_blocks(body.notes, &orelse);
        self.push(
            Stmt::While {
                metered: 0,
                condition,
                body: body.stmts,
                else_block: orelse.stmts,
            },
            blocks,
        );
    }

    fn emit_for(
        &mut self,
        binding: Name,
        iterable: Expr,
        via: Option<lash_kernel_doc::Protocol>,
        body: Buf,
        orelse: Buf,
    ) {
        let blocks = loop_blocks(body.notes, &orelse);
        self.push(
            Stmt::For {
                metered: 0,
                binding,
                iterable,
                via,
                body: body.stmts,
                else_block: orelse.stmts,
            },
            blocks,
        );
    }

    fn emit_try(&mut self, body: Buf, catch: Option<(Name, Buf)>, finally: Option<Buf>) {
        let mut blocks = vec![body.notes];
        let catch = catch.map(|(binding, buf)| {
            blocks.push(buf.notes);
            Catch {
                binding,
                body: buf.stmts,
            }
        });
        let finally = finally.map(|buf| {
            blocks.push(buf.notes);
            buf.stmts
        });
        self.push(
            Stmt::Try(TryStmt {
                body: body.stmts,
                catch,
                finally,
            }),
            blocks,
        );
    }

    /// Binds a new closure to a temporary.
    fn emit_closure(
        &mut self,
        params: Vec<Name>,
        body: Buf,
        start: Option<lash_kernel_doc::StartAdapter>,
    ) -> Operand {
        let name = self.temp();
        self.push(
            Stmt::Let {
                name: name.clone(),
                value: Rhs::Expr(Expr::Closure(Box::new(Closure {
                    params,
                    body: body.stmts,
                    start,
                }))),
            },
            vec![body.notes],
        );
        Operand::temp(name, Ty::Unknown)
    }

    /// Binds a right-hand side to a new temporary.
    fn let_rhs(&mut self, value: Rhs, ty: Ty) -> Operand {
        let name = self.temp();
        self.emit(Stmt::Let {
            name: name.clone(),
            value,
        });
        Operand::temp(name, ty)
    }

    /// An operand that later statements cannot change.
    fn pin(&mut self, operand: Operand) -> Operand {
        if operand.stable {
            return operand;
        }
        let ty = operand.ty;
        self.let_rhs(Rhs::Expr(operand.expr), ty)
    }

    /// The identity of a library function, listed in the manifest.
    pub(crate) fn function(&mut self, name: &str) -> Lowering<FunctionId> {
        let Some(function) = self.library.resolve(name) else {
            return Err(diagnostics::unplaced(
                Code::LibraryMissing,
                format!("the library function `{name}` is not installed"),
            ));
        };
        let listed = FunctionName::new(name).map_err(|error| {
            diagnostics::unplaced(Code::LibraryMissing, format!("`{name}`: {error}"))
        })?;
        self.used.insert(function, listed);
        Ok(function)
    }

    /// A helper call stays inside the expression that uses its result.
    pub(crate) fn invoke(&mut self, function: &str, args: &[Operand], ty: Ty) -> Lowering<Operand> {
        let callee = Callee::Library(self.function(function)?);
        Ok(Operand::inline(
            Expr::Act(Box::new(ActExpr {
                kind: ActKind::Call(callee),
                args: args.iter().map(|arg| arg.expr.clone()).collect(),
            })),
            ty,
        ))
    }

    /// A helper call whose result nothing reads.
    pub(crate) fn invoke_do(&mut self, function: &str, args: &[Operand]) -> Lowering<()> {
        let value = self.invoke(function, args, Ty::Unknown)?;
        self.discard(value);
        Ok(())
    }

    /// A call of a native library function, inside an expression.
    pub(crate) fn native(&mut self, function: &str, args: Vec<Expr>) -> Lowering<Expr> {
        Ok(Expr::Call {
            function: self.function(function)?,
            args,
        })
    }

    /// The statement that bound `operand` to a temporary, when it is the
    /// last one emitted: its right-hand side can be used in its place.
    fn take_binding(&mut self, operand: &Operand) -> Option<(Rhs, Note)> {
        let Expr::Variable(wanted) = &operand.expr else {
            return None;
        };
        if !operand.stable {
            return None;
        }
        match self.buf.stmts.last() {
            Some(Stmt::Let { name, .. }) if name == wanted => {}
            _ => return None,
        }
        let Some(Stmt::Let { name, value }) = self.buf.stmts.pop() else {
            return None;
        };
        let note = self.buf.notes.pop()?;
        self.private.remove(&name);
        Some((value, note))
    }

    /// A return owns the expression and notes of its final temporary.
    fn return_value(&mut self, value: Operand) {
        if let Some((rhs, note)) = self.take_binding(&value) {
            let expr = match rhs {
                Rhs::Expr(expr) => expr,
                Rhs::Action(action) => action.into(),
            };
            self.push(Stmt::Return { value: expr }, note.blocks);
            if let Some(placed) = self.buf.notes.last_mut() {
                placed.written = note.written;
                placed.nested_written = note.nested_written;
            }
        } else {
            self.emit(Stmt::Return { value: value.expr });
        }
    }

    /// Writes `value` to a place.
    fn store(&mut self, place: Place, value: Operand) {
        let value_slot = match &place {
            Place::Variable(_) => 0,
            Place::Member(lash_kernel_doc::Member::Field { .. }) => 1,
            Place::Member(lash_kernel_doc::Member::Index { .. }) => 2,
        };
        match self.take_binding(&value) {
            Some((rhs, note)) => {
                self.push(
                    Stmt::Assign {
                        place,
                        value: rhs,
                        order: AssignOrder::ValueFirst,
                    },
                    note.blocks,
                );
                if let Some(assigned) = self.buf.notes.last_mut() {
                    assigned.written = note.written;
                    assigned.nested_written = note
                        .nested_written
                        .into_iter()
                        .map(|(mut path, written)| {
                            if let Some(slot) = path.first_mut() {
                                *slot = value_slot;
                            }
                            (path, written)
                        })
                        .collect();
                }
            }
            None => self.emit(Stmt::Assign {
                place,
                value: Rhs::Expr(value.expr),
                order: AssignOrder::ValueFirst,
            }),
        }
    }

    /// Drops a value nothing reads, keeping the statement that made it.
    fn discard(&mut self, value: Operand) {
        if let Some((rhs, note)) = self.take_binding(&value) {
            let nested_written = note.nested_written;
            match rhs {
                Rhs::Action(action) => self.push(
                    Stmt::Do {
                        value: action.into(),
                    },
                    note.blocks,
                ),
                // An expression of natives changes nothing, but it may
                // raise, so it stays, bound to a variable nothing reads.
                Rhs::Expr(expr) => self.push(Stmt::Do { value: expr }, note.blocks),
            }
            if let Some(note) = self.buf.notes.last_mut() {
                note.nested_written = nested_written;
            }
        } else if !matches!(value.expr, Expr::Literal(_) | Expr::Variable(_)) {
            self.emit(Stmt::Do { value: value.expr });
        }
    }

    /// Lowers operands in order. One that a later operand's statements
    /// could change is pinned before they run (`K-EVAL-001`).
    pub(crate) fn operands(&mut self, exprs: &[&ast::Expr]) -> Lowering<Vec<Operand>> {
        let exprs: Vec<Option<&ast::Expr>> = exprs.iter().copied().map(Some).collect();
        self.optional_operands(&exprs)
    }

    /// [`Self::operands`], with None where the source wrote no operand.
    pub(crate) fn optional_operands(
        &mut self,
        exprs: &[Option<&ast::Expr>],
    ) -> Lowering<Vec<Operand>> {
        let mut parts = Vec::with_capacity(exprs.len());
        for expr in exprs {
            parts.push(match expr {
                Some(expr) => self.aside(|this| this.expr(expr))?,
                None => (Buf::default(), Operand::none()),
            });
        }
        let mut pins = vec![false; parts.len()];
        let mut later = false;
        for (index, (buf, operand)) in parts.iter().enumerate().rev() {
            pins[index] = later && !operand.stable;
            later |= !buf.is_empty();
        }
        let mut out = Vec::with_capacity(parts.len());
        for ((buf, operand), pinned) in parts.into_iter().zip(pins) {
            self.append(buf);
            out.push(if pinned { self.pin(operand) } else { operand });
        }
        Ok(out)
    }

    /// A Python value as the bool its truth is.
    pub(crate) fn truth(&mut self, operand: Operand) -> Lowering<Expr> {
        if operand.ty == Ty::Bool {
            return Ok(operand.expr);
        }
        Ok(self.invoke("py.truth", &[operand], Ty::Bool)?.expr)
    }

    fn not(&mut self, condition: Expr) -> Lowering<Expr> {
        self.native("bool.not", vec![condition])
    }

    /// Runs `lower` one level deeper in the source, refusing a program
    /// nested past what the lowerer follows.
    fn nested<T>(
        &mut self,
        range: TextRange,
        lower: impl FnOnce(&mut Self) -> Lowering<T>,
    ) -> Lowering<T> {
        if self.nesting >= self.max_nesting {
            return Err(diagnostics::with_repair(
                Code::TooDeep,
                format!(
                    "the program exceeds the configured {}-level source nesting limit",
                    self.max_nesting
                ),
                range,
                "split the expression or the block into named parts",
            ));
        }
        self.nesting += 1;
        let result = lower(self);
        self.nesting -= 1;
        result
    }

    fn span_of(&mut self, node: &impl Ranged) {
        self.span = Some(diagnostics::span(node.range()));
    }
}
