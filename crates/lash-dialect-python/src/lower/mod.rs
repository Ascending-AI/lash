//! Lowers a parsed Python module to a kernel document.
//!
//! The lowerer emits three-address statements, so the statement rule holds
//! by construction: every call of a function, a closure or a helper, and
//! every wait, is its own statement with atoms for arguments, in Python's
//! evaluation order (`K-STMT-005`). An operand read before a later
//! operand's statements run is pinned to a temporary first.
//!
//! A Python function is a kernel closure over the variables of the scopes
//! around it. Every name a body binds is declared at the top of the kernel
//! block the body lowers to, holding absent until it is assigned, because
//! Python scopes a name to the whole function and a closure may name it
//! before its assignment runs.

mod annotate;
mod calls;
mod expressions;
mod format;
mod repairs;
mod saved;
mod statements;

use std::collections::{BTreeMap, BTreeSet, HashSet};

use lash_kernel_dialect::{Diagnostic, Environment, Library, Lowered, Span};
use lash_kernel_doc::{
    Action, Atom, Callee, Catch, Closure, Document, EffectName, Expr, FunctionCatalog, FunctionId,
    FunctionName, Literal, Name, NumberPolicy, Place, Rhs, Signature, Stmt, TryStmt,
    validate_document,
};
use ruff_python_ast::{self as ast};
use ruff_text_size::{Ranged, TextRange};

use crate::diagnostics::{self, Code};
use crate::exceptions::Classes;
use crate::scope::{self, Bindings, Ty};

pub(crate) type Lowering<T> = Result<T, Diagnostic>;

/// The deepest the lowerer follows nested expressions and statements. The
/// kernel's own limit (`K-DOC-006`) is met long before.
const MAX_SOURCE_NESTING: usize = 96;

/// Where a kernel statement came from, and the notes of the blocks under
/// it in the order a walk of the statement meets them.
#[derive(Clone, Debug)]
pub(crate) struct Note {
    span: Option<Span>,
    blocks: Vec<Vec<Note>>,
    written: Option<serde_json::Value>,
}

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

/// A lowered Python expression: a kernel expression that calls nothing but
/// native library functions, what is known of its type, and whether later
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
    /// Names that hold a value at this point, one frame per kernel block
    /// open in this scope. A read of a name in none of them is checked.
    bound: Vec<BTreeSet<String>>,
    /// The values the `except` clauses being lowered caught, innermost
    /// last: what a bare `raise` raises again.
    handlers: Vec<Name>,
    /// The loops being lowered, innermost last, each with the variable
    /// that records its `break` when something needs to know.
    loops: Vec<Option<Name>>,
}

/// A Python variable as the kernel names it.
struct Found {
    name: Name,
    /// The scope that owns it; none for a comprehension's variable.
    scope: Option<usize>,
}

pub(crate) struct Lowerer<'a> {
    source: &'a str,
    library: &'a dyn Library,
    effects: &'a BTreeMap<EffectName, Signature>,
    /// The effects whose call ends the turn: a control call ends `main`
    /// when it settles, and is written only where it can.
    controls: &'a BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>,
    performed: BTreeMap<EffectName, Signature>,
    used: BTreeMap<FunctionId, FunctionName>,
    private: BTreeSet<Name>,
    taken: HashSet<String>,
    scopes: Vec<Scope>,
    /// Comprehension variables, which Python scopes to the comprehension.
    renames: Vec<(String, Name)>,
    classes: Classes,
    rebound: BTreeSet<String>,
    buf: Buf,
    kernel_depth: usize,
    span: Option<Span>,
    nesting: usize,
    saved: &'a BTreeMap<Name, lash_kernel_dialect::SavedFunction>,
    saved_used: BTreeSet<Name>,
    saved_startable: BTreeSet<Name>,
    declared: BTreeMap<Name, lash_kernel_doc::Function>,
    entries: BTreeMap<Name, Signature>,
}

/// No module or restored session binding may mask the dialect's built-ins,
/// nor a tool name or catalog namespace root (`control_finish`, `control`,
/// ...). Roots are carried separately because flattened names are ambiguous.
pub(crate) fn check_binding_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    effects: &BTreeMap<EffectName, lash_kernel_doc::Signature>,
    tool_roots: &BTreeSet<Name>,
) -> Lowering<()> {
    for name in names {
        let message = if calls::is_builtin(name) || crate::exceptions::is_builtin(name) {
            format!("`{name}` is a built-in; a top-level binding cannot reuse its name")
        } else if name == "control"
            || tool_roots.contains(&Name::new(name))
            || effects.keys().any(|effect| effect.as_str() == name)
        {
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
pub(crate) fn lower(
    module: &ast::ModModule,
    source: &str,
    environment: &Environment<'_>,
) -> Lowering<Lowered> {
    let mut taken: HashSet<String> = source
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    taken.extend(
        environment
            .bindings
            .iter()
            .map(|name| name.as_str().to_string()),
    );
    let (globals, nonlocals) = scope::rebound(&module.body);
    let rebound: BTreeSet<String> = globals.union(&nonlocals).cloned().collect();
    let mut bindings = scope::bindings(&module.body, None, &rebound);
    let saved_startable = environment
        .functions
        .keys()
        .filter(|name| !bindings.locals.contains(name.as_str()) && !rebound.contains(name.as_str()))
        .cloned()
        .collect();
    for (name, saved) in environment.functions {
        if !bindings.locals.contains(name.as_str())
            && let Some(signature) = saved::call_signature(saved)
        {
            bindings.defs.insert(name.to_string(), signature);
        }
    }
    // A `global` statement anywhere makes the name the module's, and an
    // earlier cell's bindings are the module's too.
    bindings.locals.extend(globals);
    check_binding_names(
        bindings.locals.iter().map(String::as_str),
        environment.effects,
        environment.tool_roots,
    )?;
    for statement in &module.body {
        if let ast::Stmt::ClassDef(class) = statement {
            check_binding_names(
                [class.name.id.as_str()],
                environment.effects,
                environment.tool_roots,
            )?;
        }
    }
    let declared: Vec<String> = bindings
        .locals
        .iter()
        .filter(|name| !environment.bindings.contains(&Name::new(name.as_str())))
        .cloned()
        .collect();
    bindings.locals.extend(
        environment
            .bindings
            .iter()
            .map(|name| name.as_str().to_string()),
    );
    let mut lowerer = Lowerer {
        source,
        library: environment.library,
        effects: environment.effects,
        controls: environment.controls,
        performed: BTreeMap::new(),
        used: BTreeMap::new(),
        private: BTreeSet::new(),
        taken,
        scopes: vec![Scope {
            kind: ScopeKind::Module,
            bindings,
            bound: vec![BTreeSet::new()],
            handlers: Vec::new(),
            loops: Vec::new(),
        }],
        renames: Vec::new(),
        classes: Classes::builtin(),
        rebound,
        buf: Buf::default(),
        kernel_depth: 0,
        span: None,
        nesting: 0,
        saved: environment.functions,
        saved_used: BTreeSet::new(),
        saved_startable,
        declared: BTreeMap::new(),
        entries: BTreeMap::new(),
    };
    lowerer.declare_classes(&module.body)?;
    for name in &declared {
        lowerer.emit(Stmt::Let {
            name: Name::new(name.as_str()),
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        });
    }
    // asyncio cancels the tasks still running when the program ends; the
    // kernel would call them an error (`K-TASK-018`, `K-TASK-019`).
    if source.contains("create_task") || source.contains("gather") {
        let body = lowerer.block(|this| this.statements(&module.body))?;
        let cancel = lowerer.function("tasks.cancel_all")?;
        let finally = Buf {
            stmts: vec![Stmt::Do {
                action: Action::Call {
                    callee: Callee::Library(cancel),
                    args: Vec::new(),
                },
            }],
            notes: vec![Note {
                span: None,
                blocks: Vec::new(),
                written: None,
            }],
        };
        lowerer.span = None;
        lowerer.emit_try(body, None, Some(finally));
    } else {
        lowerer.statements(&module.body)?;
    }
    let main = std::mem::take(&mut lowerer.buf);
    let mut document = Document::new(NumberPolicy::BySpelling, main.stmts);
    document.private_bindings = lowerer.private;
    document.functions = lowerer.declared;
    document.entries = lowerer.entries;
    document.manifest.functions = reachable(lowerer.used, environment.library);
    document.manifest.effects = lowerer.performed;
    if !lowerer.saved_used.is_empty() {
        let catalog: &dyn FunctionCatalog = environment.library;
        lash_kernel_dialect::install(
            &mut document,
            environment.functions,
            &lowerer.saved_used,
            &lash_kernel_dialect::FunctionValues::Bare,
            environment.effects,
            environment.controls,
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
    }
    validate_document(&document, environment.library).map_err(|invalid| {
        diagnostics::unplaced(
            Code::InvalidDocument,
            format!("the program lowers to a document the kernel refuses: {invalid}"),
        )
    })?;
    let annotations = annotate::annotations(&document, &main.notes, source)?;
    Ok(Lowered {
        document,
        annotations,
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
        if listed.insert(function, name).is_some() {
            continue;
        }
        let catalog: &dyn FunctionCatalog = library;
        if let Some(body) = catalog
            .definition(&function)
            .and_then(|definition| definition.body())
        {
            pending.extend(
                body.functions
                    .iter()
                    .map(|(function, name)| (*function, name.clone())),
            );
        }
    }
    listed
}

impl Lowerer<'_> {
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

    /// Records that `id` holds a value from here to the end of the block.
    fn mark_bound(&mut self, id: &str) {
        let current = self.scopes.len() - 1;
        if self.variable(id).and_then(|found| found.scope) == Some(current)
            && let Some(frame) = self.scope_mut().bound.last_mut()
        {
            frame.insert(id.to_string());
        }
    }

    /// The annotated type of a variable, which the front end trusts.
    fn type_of(&self, found: &Found, id: &str) -> Ty {
        found
            .scope
            .and_then(|scope| self.scopes[scope].bindings.types.get(id).copied())
            .unwrap_or(Ty::Unknown)
    }

    /// The function `id` names, when a `def` is all that binds it.
    fn known_def(&self, id: &str) -> Option<scope::Signature> {
        let found = self.variable(id)?;
        self.scopes[found.scope?].bindings.defs.get(id).cloned()
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
            self.invoke_do("py.name_check", &[value, Operand::text(id), local])?;
            self.mark_bound(id);
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

    fn not_a_variable(&self, id: &str, range: TextRange) -> Diagnostic {
        if self.effects.keys().any(|effect| effect.as_str() == id) {
            return diagnostics::with_repair(
                Code::CoroutineNotAwaited,
                format!("`{id}` is a tool; it is called and awaited, not passed around"),
                range,
                format!(
                    "call `{id}` with its required arguments and await the call; use `asyncio.create_task` to run that call beside this code"
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
        diagnostics::diagnostic(
            Code::UnknownName,
            format!("name `{id}` is not defined"),
            range,
        )
    }

    // Emission.

    fn push(&mut self, stmt: Stmt, blocks: Vec<Vec<Note>>) {
        if let Stmt::Let { name, .. } = &stmt
            && self.kernel_depth == 0
            && !self.scopes[0].bindings.locals.contains(name.as_str())
        {
            self.private.insert(name.clone());
        }
        self.buf.stmts.push(stmt);
        self.buf.notes.push(Note {
            span: self.span,
            blocks,
            written: None,
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

    fn emit_while(&mut self, condition: Expr, body: Buf) {
        self.push(
            Stmt::While {
                condition,
                body: body.stmts,
            },
            vec![body.notes],
        );
    }

    fn emit_for(&mut self, binding: Name, iterable: Expr, body: Buf) {
        self.push(
            Stmt::For {
                binding,
                iterable,
                body: body.stmts,
            },
            vec![body.notes],
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
    fn emit_closure(&mut self, params: Vec<Name>, body: Buf) -> Operand {
        let name = self.temp();
        self.push(
            Stmt::Let {
                name: name.clone(),
                value: Rhs::Expr(Expr::Closure(Box::new(Closure {
                    params,
                    body: body.stmts,
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

    /// An operand as an action's argument.
    fn atom(&mut self, operand: Operand) -> Atom {
        match operand.expr {
            Expr::Literal(literal) => Atom::Literal(literal),
            Expr::Variable(name) => Atom::Variable(name),
            expr => {
                let name = self.temp();
                self.emit(Stmt::Let {
                    name: name.clone(),
                    value: Rhs::Expr(expr),
                });
                Atom::Variable(name)
            }
        }
    }

    fn atoms(&mut self, operands: &[Operand]) -> Vec<Atom> {
        operands
            .iter()
            .map(|operand| self.atom(operand.clone()))
            .collect()
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

    /// A call of a helper or of another function with a kernel body, as
    /// its own statement.
    pub(crate) fn invoke(&mut self, function: &str, args: &[Operand], ty: Ty) -> Lowering<Operand> {
        let callee = Callee::Library(self.function(function)?);
        let args = self.atoms(args);
        Ok(self.let_rhs(Rhs::Action(Action::Call { callee, args }), ty))
    }

    /// A helper call whose result nothing reads.
    pub(crate) fn invoke_do(&mut self, function: &str, args: &[Operand]) -> Lowering<()> {
        let callee = Callee::Library(self.function(function)?);
        let args = self.atoms(args);
        self.emit(Stmt::Do {
            action: Action::Call { callee, args },
        });
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

    /// Writes `value` to a place.
    fn store(&mut self, place: Place, value: Operand) {
        match self.take_binding(&value) {
            Some((rhs, note)) => {
                self.push(Stmt::Assign { place, value: rhs }, note.blocks);
                if let Some(assigned) = self.buf.notes.last_mut() {
                    assigned.written = note.written;
                }
            }
            None => self.emit(Stmt::Assign {
                place,
                value: Rhs::Expr(value.expr),
            }),
        }
    }

    /// Drops a value nothing reads, keeping the statement that made it.
    fn discard(&mut self, value: Operand) {
        if let Some((rhs, note)) = self.take_binding(&value) {
            match rhs {
                Rhs::Action(action) => self.push(Stmt::Do { action }, note.blocks),
                // An expression of natives changes nothing, but it may
                // raise, so it stays, bound to a variable nothing reads.
                Rhs::Expr(expr) => {
                    let name = self.temp();
                    self.push(
                        Stmt::Let {
                            name,
                            value: Rhs::Expr(expr),
                        },
                        note.blocks,
                    );
                }
            }
        } else if !matches!(value.expr, Expr::Literal(_) | Expr::Variable(_)) {
            let name = self.temp();
            self.emit(Stmt::Let {
                name,
                value: Rhs::Expr(value.expr),
            });
        }
    }

    /// Lowers operands in order. One that a later operand's statements
    /// could change is pinned before they run (`K-STMT-005`).
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
        if self.nesting >= MAX_SOURCE_NESTING {
            return Err(diagnostics::with_repair(
                Code::TooDeep,
                "the program nests deeper than the dialect lowers",
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
