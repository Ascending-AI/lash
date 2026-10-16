//! Lowers the adapter's tree to a kernel document.
//!
//! Native calls, member reads and constructions stay in expressions
//! (`K-LIB-002`). Actions remain statements with atom arguments: the front
//! end nests none (`K-EVAL-008`). An earlier operand is pinned when a later
//! operand emits statements that could change its value or run before its
//! raise (`K-EVAL-001`).
//!
//! A JavaScript function is a closure of two parameters, `fn(this, args)`.
//! Source bindings keep their names; a binding that would shadow another in
//! scope, and every temporary, takes a name the source does not spell, so
//! no kernel variable in a document shadows another.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lash_kernel_dialect::{CellEnvironment, Environment, Library, Lowered, Mode, workflow};
use lash_kernel_doc::{
    Action, Atom, Block, Callee, Catch, Closure, Document, EffectName, Expr, Float,
    FunctionCatalog, FunctionId, FunctionName, Literal, Member, Name, NumberPolicy, Place, Rhs,
    Signature, Stmt, TryStmt,
};

use crate::adapter as ast;
use crate::builtins::{self, Table};
use crate::types::{Facts, PlainRecord, Ty};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

mod async_fn;
mod calls;
pub(crate) use calls::tool_calls;
mod expressions;
mod functions;
mod lazy;
mod patterns;
mod positional;
mod primitive_methods;
mod process;
mod statements;
mod walk;

pub(crate) type Lowering<T> = Result<T, Diagnostic>;

/// The helpers and kernel functions the lowerer itself emits calls to. A
/// library that lacks one cannot run a lowered document.
#[cfg(test)]
pub(crate) const CORE_OPERATIONS: &[&str] = &[
    "bool.not",
    "eq",
    "error.new",
    "float.to_int",
    "list.check",
    "list.get",
    "list.len",
    "list.slice",
    "num.add",
    "num.div",
    "num.le",
    "num.lt",
    "num.max",
    "num.min",
    "num.mul",
    "num.neg",
    "num.rem_trunc",
    "num.sub",
    "num.to_float",
    "same",
    "text.concat",
    "text.join",
    "text.len",
    "text.split",
    "text.trim_set",
    "text.utf16_find",
    "text.utf16_len",
    "ts.add",
    "ts.arguments",
    "ts.await",
    "ts.assign",
    "ts.array.iterator",
    "ts.bit_and",
    "ts.bit_not",
    "ts.bit_or",
    "ts.bit_xor",
    "ts.callable",
    "ts.delete",
    "ts.div",
    "ts.for_of.item",
    "ts.for_of.next",
    "ts.for_of.open",
    "ts.ge",
    "ts.get",
    "ts.gt",
    "ts.has",
    "ts.in",
    "ts.is_nullish",
    "ts.keys",
    "ts.le",
    "ts.loose_equals",
    "ts.loose_not_equals",
    "ts.lt",
    "ts.mul",
    "ts.neg",
    "ts.not",
    "ts.object_rest",
    "ts.pad",
    "ts.pow",
    "ts.promise.pending",
    "ts.promise.run",
    "ts.read",
    "ts.rem",
    "ts.require_object_coercible",
    "ts.rest",
    "ts.set",
    "ts.shl",
    "ts.shr",
    "ts.spread",
    "ts.strict_equals",
    "ts.strict_not_equals",
    "ts.string.slice_units",
    "ts.sub",
    "ts.tdz",
    "ts.to_boolean",
    "ts.to_number",
    "ts.to_string",
    "ts.to_property_key",
    "ts.typeof",
    "ts.ushr",
];

/// A native-only expression, its representation facts, and whether later
/// statements can change its value or run before it raises.
#[derive(Clone, Debug)]
pub(crate) struct Operand {
    pub(crate) expr: Expr,
    stable: bool,
    pub(crate) ty: Ty,
    /// Proven representation and stable nested fields, never an annotation
    /// or part of a saved value. Boundness is checked before this is attached.
    pub(crate) plain: Option<PlainRecord>,
}

impl Operand {
    pub(crate) fn undefined() -> Self {
        Self {
            expr: Expr::Literal(Literal::Absent),
            ty: Ty::Undefined,
            plain: None,
            stable: true,
        }
    }

    pub(crate) fn null() -> Self {
        Self {
            expr: Expr::Literal(Literal::Null),
            ty: Ty::Null,
            plain: None,
            stable: true,
        }
    }

    pub(crate) fn bool(value: bool) -> Self {
        Self {
            expr: Expr::Literal(Literal::Bool(value)),
            ty: Ty::Bool,
            plain: None,
            stable: true,
        }
    }

    pub(crate) fn number(value: f64) -> Self {
        Self {
            expr: Expr::Literal(Literal::Float(Float::new(value))),
            ty: Ty::Float,
            plain: None,
            stable: true,
        }
    }

    pub(crate) fn text(value: impl Into<String>) -> Self {
        Self {
            expr: Expr::Literal(Literal::Text(value.into())),
            ty: Ty::Text,
            plain: None,
            stable: true,
        }
    }

    pub(crate) fn variable(name: Name, ty: Ty) -> Self {
        Self {
            expr: Expr::Variable(name),
            ty,
            plain: None,
            stable: false,
        }
    }

    fn temp(name: Name, ty: Ty) -> Self {
        Self {
            stable: true,
            ..Self::variable(name, ty)
        }
    }

    pub(crate) fn expr(&self) -> Expr {
        self.expr.clone()
    }
}

/// The private entry of a stable local function, taking its arguments as slots.
#[derive(Clone, Debug)]
struct Positional {
    name: Name,
    arity: usize,
}

pub(crate) use lash_kernel_dialect::Note;

/// A kernel block under construction.
#[derive(Debug, Default)]
pub(crate) struct Buf {
    pub(crate) stmts: Block,
    pub(crate) notes: Vec<Note>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindingKind {
    Var,
    Let,
    Const,
    Function,
    /// A parameter, a catch binding or a loop binding: live where declared.
    Local,
}

#[derive(Clone, Debug)]
struct Binding {
    kernel: Name,
    kind: BindingKind,
    /// Whether the declaration has run where lowering now stands.
    live: bool,
    /// Whether the variable is declared at its scope's start, ahead of its
    /// declaration: it was named before the declaration, or the declaration
    /// sits in a deeper kernel block than the scope.
    predeclared: bool,
    /// The function the binding belongs to, as an index into the stack.
    function: usize,
    positional: Option<Positional>,
}

#[derive(Debug, Default)]
struct Scope {
    bindings: HashMap<String, Binding>,
    /// Where in its buffer the scope began: predeclarations go there.
    start: usize,
    kernel_depth: usize,
    /// Each predeclaration, with the span of the reference that needed it.
    predeclare: Vec<(Name, Expr, Option<SourceSpan>)>,
}

/// What `break` and `continue` leave.
#[derive(Debug)]
enum Control {
    Loop {
        /// What a `continue` runs before the kernel's own `continue`.
        before_continue: Block,
    },
    Switch {
        /// Set by a `continue` inside the switch, which first has to leave
        /// the loop the switch is lowered to.
        continue_flag: Option<Name>,
    },
}

#[derive(Debug)]
struct FunctionFrame {
    arrow: bool,
    this: Option<Name>,
    args: Option<Name>,
    arguments_used: bool,
    controls: Vec<Control>,
}

pub(crate) struct Lowerer<'a> {
    source: &'a str,
    /// The entries of a workflow module, as they are lowered; `None` for a
    /// cell.
    module: Option<workflow::Module>,
    library: &'a dyn Library,
    /// The effects the host supplies: a call of one is a tool call.
    effects: &'a BTreeMap<EffectName, Signature>,
    /// The effects whose call ends the turn: a control call ends `main`
    /// when it settles, and is written only where it can.
    controls: &'a BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>,
    /// The effects the document performs, for its manifest (`K-DOC-002`).
    performed: BTreeMap<EffectName, Signature>,
    session: &'a BTreeSet<Name>,
    table: &'static Table,
    /// Every word of the source, every session binding and every name
    /// issued so far: a generated name is none of them.
    taken: HashSet<String>,
    /// For each base `fresh` was given, the number its search resumes at:
    /// every lower one is taken, and `taken` only grows.
    next_fresh: HashMap<String, usize>,
    temporaries: HashSet<Name>,
    scopes: Vec<Scope>,
    functions: Vec<FunctionFrame>,
    pub(crate) buf: Buf,
    kernel_depth: usize,
    span: Option<SourceSpan>,
    used: BTreeMap<FunctionId, FunctionName>,
    private: BTreeSet<Name>,
    facts: Facts,
    /// What tests have shown of names, innermost last, for the code being
    /// lowered now.
    narrowed: Vec<(String, Ty)>,
    paths: Vec<expressions::LoopPaths>,
    /// The functions the document declares: its processes.
    declared: BTreeMap<Name, lash_kernel_doc::Function>,
    /// The notes of each declared function's body.
    declared_notes: BTreeMap<Name, Vec<Note>>,
    /// The declared functions a host may start, with their signatures.
    /// While a process body is lowered: the cell's names, none of which the
    /// body may read.
    lifting: Option<HashSet<String>>,
    /// The functions the session holds, and the ones the source names: the
    /// document declares those.
    saved: &'a BTreeMap<Name, lash_kernel_dialect::SavedFunction>,
    saved_used: BTreeSet<Name>,
    /// What the closure just emitted is, as its source wrote it, until the
    /// statement that binds it takes it.
    written: Option<lash_kernel_dialect::Written>,
    /// Every name the source assigns to.
    assigned: &'a BTreeSet<String>,
}

/// Session names are checked before either source or saved state can mask a built-in.
/// `control` stays reserved; ordinary tool namespaces obey lexical shadowing.
pub(crate) fn check_binding_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    _effects: &BTreeMap<EffectName, lash_kernel_doc::Signature>,
) -> Lowering<()> {
    for name in names.into_iter().collect::<BTreeSet<_>>() {
        if builtins::is_global(name) {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::ShadowsBuiltin,
                format!("`{name}` is a built-in; a top-level binding cannot reuse its name"),
                format!("rename `{name}` to `{name}_` and update its references"),
                None,
            ));
        }
        if name == "control" {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::ShadowsBuiltin,
                format!(
                    "`{name}` names the session's tools; a top-level binding cannot reuse its name"
                ),
                format!("rename `{name}` to `{name}_` and update its references"),
                None,
            ));
        }
    }
    Ok(())
}

/// Lowers a parsed program against `environment`.
pub(crate) fn lower(
    program: &ast::Program,
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
) -> Lowering<Lowered> {
    let (cell, module) = match mode {
        Mode::Cell(cell) => (cell, None),
        Mode::Module => (CellEnvironment::first(), Some(workflow::Module::new())),
    };
    let mut taken: HashSet<String> = source
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    taken.extend(cell.bindings.iter().map(|name| name.as_str().to_string()));
    let mut lowerer = Lowerer {
        source,
        module,
        library: environment.library,
        effects: environment.effects,
        controls: environment.controls,
        performed: BTreeMap::new(),
        session: cell.bindings,
        table: builtins::table(),
        taken,
        next_fresh: HashMap::new(),
        temporaries: HashSet::new(),
        scopes: Vec::new(),
        functions: vec![FunctionFrame {
            arrow: false,
            this: None,
            args: None,
            arguments_used: false,
            controls: Vec::new(),
        }],
        buf: Buf::default(),
        kernel_depth: 0,
        span: None,
        used: BTreeMap::new(),
        private: BTreeSet::new(),
        facts: Facts::analyse(
            program,
            cell.bindings,
            &environment
                .controls
                .iter()
                .filter(|(_, controls)| {
                    controls.contains(&lash_kernel_dialect::EffectControl::Finish)
                })
                .map(|(name, _)| name.as_str().to_string())
                .collect(),
            environment.effects,
        ),
        assigned: &program.assigned,
        narrowed: Vec::new(),
        paths: Vec::new(),
        declared: BTreeMap::new(),
        declared_notes: BTreeMap::new(),
        lifting: None,
        saved: cell.functions,
        saved_used: BTreeSet::new(),
        written: None,
    };
    lowerer.push_scope();
    lowerer.declare_vars(&program.statements);
    lowerer.declare_block(&program.statements)?;
    check_binding_names(
        lowerer.scopes[0].bindings.keys().map(String::as_str),
        environment.effects,
    )?;
    lowerer.lower_statements(&program.statements)?;
    lowerer.pop_scope();
    let main = std::mem::take(&mut lowerer.buf);
    // Saved function values are already bound by S8 adoption. Their token
    // and frozen captures stay in heap pages; lowering never reconstructs
    // those payloads as source expressions.
    let mut document = Document::new(NumberPolicy::Float, main.stmts);
    document.private_bindings = lowerer.private;
    document.functions = lowerer.declared;
    if let Some(module) = lowerer.module.take() {
        module
            .assemble(&mut document, &mut lowerer.declared_notes)
            .map_err(Diagnostic::module)?;
    }
    document.manifest.functions = reachable(lowerer.used, environment.library);
    document.manifest.effects = lowerer.performed;
    if !lowerer.saved_used.is_empty() {
        // The saved functions the source names are declared in the cell's
        // own document, with what they require.
        let catalog: &dyn FunctionCatalog = environment.library;
        lash_kernel_dialect::install(
            &mut document,
            cell.functions,
            &lowerer.saved_used,
            &crate::function_values(),
            environment.effects,
            environment.controls,
            &|function| catalog.definition(function).is_some(),
        )
        .map_err(|unusable| {
            Diagnostic::new(
                DiagnosticCode::SavedFunctionUnusable,
                unusable.to_string(),
                None,
            )
        })?;
        document.manifest.functions = reachable(
            std::mem::take(&mut document.manifest.functions),
            environment.library,
        );
    }
    let document = lash_kernel_doc::IdentifiedDocument::new(document);
    let (source_map, written) = lash_kernel_dialect::site_layers(
        &document,
        source,
        lash_kernel_doc::SourceLanguage::TypeScript,
        &main.notes,
        &lowerer.declared_notes,
    )
    .map_err(|refused| Diagnostic::new(DiagnosticCode::InvalidAst, refused.to_string(), None))?;
    Ok(Lowered {
        document,
        source_map: Some(source_map),
        written,
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
        let mut number = self.next_fresh.get(base).copied().unwrap_or(1);
        loop {
            let candidate = format!("{base}{number}");
            number += 1;
            if self.taken.insert(candidate.clone()) {
                self.next_fresh.insert(base.to_string(), number);
                return Name::new(candidate);
            }
        }
    }

    pub(crate) fn temp(&mut self) -> Name {
        let name = self.fresh("t");
        self.temporaries.insert(name.clone());
        name
    }

    // Scopes.

    fn push_scope(&mut self) {
        self.scopes.push(Scope {
            start: self.buf.stmts.len(),
            kernel_depth: self.kernel_depth,
            ..Scope::default()
        });
    }

    /// Ends a scope, declaring at its start every variable that had to
    /// exist before its declaration ran.
    fn pop_scope(&mut self) {
        // A variable of `main`'s own scope is a session binding, declared
        // ahead of its declaration or not.
        let session_scope = self.scopes.len() == 1 && self.in_main();
        let Some(scope) = self.scopes.pop() else {
            unreachable!("a scope is open");
        };
        debug_assert_eq!(scope.kernel_depth, self.kernel_depth);
        for (offset, (name, value, span)) in scope.predeclare.into_iter().enumerate() {
            if !session_scope {
                self.note_private(&name);
            }
            self.buf.stmts.insert(
                scope.start + offset,
                Stmt::Let {
                    name,
                    value: Rhs::Expr(value),
                },
            );
            self.buf.notes.insert(
                scope.start + offset,
                Note {
                    span,
                    ..Note::default()
                },
            );
        }
    }

    fn frame(&self) -> &FunctionFrame {
        match self.functions.last() {
            Some(frame) => frame,
            None => unreachable!("`main` is always on the stack"),
        }
    }

    fn frame_mut(&mut self) -> &mut FunctionFrame {
        match self.functions.last_mut() {
            Some(frame) => frame,
            None => unreachable!("`main` is always on the stack"),
        }
    }

    /// Whether lowering stands in `main`, outside every function.
    fn in_main(&self) -> bool {
        self.functions.len() == 1
    }

    /// Declares a source binding in the innermost scope and gives the kernel
    /// name it is lowered to.
    pub(crate) fn declare(&mut self, name: &str, kind: BindingKind) -> Name {
        if let Some(existing) = self
            .scopes
            .last()
            .and_then(|scope| scope.bindings.get(name))
        {
            // `var` twice, a function over a `var`, a parameter named again.
            return existing.kernel.clone();
        }
        let session_slot = self.in_main() && self.scopes.len() == 1;
        let shadows = self.session.contains(&Name::new(name))
            || self
                .scopes
                .iter()
                .flat_map(|scope| scope.bindings.values())
                .any(|binding| binding.kernel.as_str() == name);
        let inner_of_main = self.in_main() && self.scopes.len() > 1;
        let kernel = if session_slot || !(shadows || inner_of_main) {
            Name::new(name)
        } else {
            self.fresh(&format!("{name}_"))
        };
        let function = self.functions.len() - 1;
        let Some(scope) = self.scopes.last_mut() else {
            unreachable!("a scope is open");
        };
        scope.bindings.insert(
            name.to_string(),
            Binding {
                kernel: kernel.clone(),
                kind,
                live: kind == BindingKind::Local,
                predeclared: false,
                function,
                positional: None,
            },
        );
        kernel
    }

    fn binding_mut(&mut self, name: &str) -> Option<(usize, &mut Binding)> {
        self.scopes
            .iter_mut()
            .enumerate()
            .rev()
            .find_map(|(index, scope)| scope.bindings.get_mut(name).map(|binding| (index, binding)))
    }

    /// Whether `name` is a function declaration that nothing assigns to,
    /// which therefore always holds the token of the function it declares.
    pub(crate) fn holds_declared_function(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .is_some_and(|binding| binding.kind == BindingKind::Function)
            && !self.assigned.contains(name)
    }

    pub(crate) fn is_bound(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.bindings.contains_key(name))
            || self.session.contains(&Name::new(name))
    }

    /// Marks a binding as declared at its scope's start.
    fn predeclare(&mut self, scope: usize, name: &str) {
        let Some(binding) = self.scopes[scope].bindings.get_mut(name) else {
            return;
        };
        if binding.predeclared {
            return;
        }
        binding.predeclared = true;
        let kernel = binding.kernel.clone();
        let value = match binding.kind {
            // The empty tuple marks a binding that is not initialised.
            BindingKind::Let | BindingKind::Const => Expr::Tuple(Vec::new()),
            _ => Expr::Literal(Literal::Absent),
        };
        if scope == 0 && self.session.contains(&kernel) {
            // An earlier cell declared it; this cell assigns it.
            return;
        }
        self.scopes[scope]
            .predeclare
            .push((kernel, value, self.span));
    }

    /// The variable a source name reads, checked for use before its
    /// declaration.
    fn resolve(&mut self, name: &str, span: Option<SourceSpan>) -> Lowering<Option<Name>> {
        let function = self.functions.len() - 1;
        let Some((scope, binding)) = self.binding_mut(name) else {
            if let Some(captured) = self.captured(name) {
                return Err(captured);
            }
            let session = Name::new(name);
            if !self.session.contains(&session) {
                return Ok(None);
            }
            if self.saved.contains_key(&session) {
                self.saved_used.insert(session.clone());
            }
            return Ok(Some(session));
        };
        let kernel = binding.kernel.clone();
        if binding.live {
            return Ok(Some(kernel));
        }
        let lexical = matches!(binding.kind, BindingKind::Let | BindingKind::Const);
        if lexical && binding.function == function {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!("`{name}` is used before its declaration"),
                span,
            ));
        }
        self.predeclare(scope, name);
        if lexical {
            let checked = self.initialised(kernel, name)?;
            return Ok(Some(checked));
        }
        Ok(Some(kernel))
    }

    /// Checks that a `let` or `const` a function reads from an enclosing
    /// scope has been initialised, and gives the checked value's variable.
    fn initialised(&mut self, kernel: Name, name: &str) -> Lowering<Name> {
        let checked = self.invoke(
            "ts.tdz",
            &[Operand::variable(kernel, Ty::Unknown), Operand::text(name)],
            Ty::Unknown,
        )?;
        let Expr::Variable(checked) = checked.expr else {
            unreachable!("a call is bound to a temporary");
        };
        Ok(checked)
    }

    /// The variable a source name writes.
    fn resolve_for_write(&mut self, name: &str, span: Option<SourceSpan>) -> Lowering<Name> {
        if self.binding_mut(name).is_none()
            && let Some(captured) = self.captured(name)
        {
            return Err(captured);
        }
        if let Some((_, binding)) = self.binding_mut(name) {
            if binding.kind == BindingKind::Const {
                return Err(Diagnostic::new(
                    DiagnosticCode::AssignConst,
                    format!("`{name}` is a constant and cannot be assigned"),
                    span,
                ));
            }
            let kernel = binding.kernel.clone();
            if !binding.live {
                // The check reads the variable; a `var` or a function is
                // only made to exist.
                if let Some(checked) = self.resolve(name, span)?
                    && checked != kernel
                {
                    self.discard(Operand::variable(checked, Ty::Unknown));
                }
            }
            return Ok(kernel);
        }
        if self.session.contains(&Name::new(name)) {
            return Ok(Name::new(name));
        }
        if builtins::is_global(name) {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::ReflectionUnsupported,
                format!(
                    "`{name}` is a built-in global; replacing it is outside the TypeScript dialect"
                ),
                format!("assign to a new name such as `{name}_` instead"),
                span,
            ));
        }
        Err(unknown_binding(name, span))
    }

    /// Binds the value of a declaration that has now run.
    fn initialise(&mut self, name: &str, value: Operand) {
        let depth = self.kernel_depth;
        let session = self.session.contains(&Name::new(name));
        let Some((scope, binding)) = self.binding_mut(name) else {
            unreachable!("`{name}` is declared before it is initialised");
        };
        let kernel = binding.kernel.clone();
        let declared = binding.predeclared || binding.live || (scope == 0 && session);
        binding.live = true;
        if declared {
            self.store(Place::Variable(kernel), value);
        } else if self.scopes[scope].kernel_depth != depth {
            // The declaration sits in a deeper block than its scope, and
            // the variable outlives that block.
            self.predeclare(scope, name);
            self.store(Place::Variable(kernel), value);
        } else {
            self.bind(kernel, value);
        }
    }

    // Emission.

    fn note_private(&mut self, name: &Name) {
        let session_slot = self.scopes.first().is_some_and(|scope| {
            scope
                .bindings
                .values()
                .any(|binding| &binding.kernel == name)
        });
        if self.kernel_depth == 0 && !session_slot {
            self.private.insert(name.clone());
        }
    }

    fn push(&mut self, stmt: Stmt, blocks: Vec<Vec<Note>>) {
        if let Stmt::Let { name, .. } = &stmt {
            let name = name.clone();
            self.note_private(&name);
        }
        self.buf.stmts.push(stmt);
        self.buf.notes.push(Note {
            span: self.span,
            written: None,
            nested_written: Vec::new(),
            blocks,
        });
    }

    pub(crate) fn emit(&mut self, stmt: Stmt) {
        self.push(stmt, Vec::new());
    }

    /// Lowers into a new kernel block and returns it.
    fn block(&mut self, lower: impl FnOnce(&mut Self) -> Lowering<()>) -> Lowering<Buf> {
        let outer = std::mem::take(&mut self.buf);
        self.kernel_depth += 1;
        let result = lower(self);
        self.kernel_depth -= 1;
        let inner = std::mem::replace(&mut self.buf, outer);
        result.map(|()| inner)
    }

    /// A block with a scope of its own.
    fn scoped_block(&mut self, lower: impl FnOnce(&mut Self) -> Lowering<()>) -> Lowering<Buf> {
        self.block(|this| {
            this.push_scope();
            let result = lower(this);
            this.pop_scope();
            result
        })
    }

    fn append(&mut self, buf: Buf) {
        for statement in &buf.stmts {
            if let Stmt::Let { name, .. } = statement {
                self.note_private(name);
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

    fn emit_while(&mut self, condition: Expr, body: Buf) {
        self.push(
            Stmt::While {
                condition,
                body: body.stmts,
                else_block: Vec::new(),
            },
            vec![body.notes],
        );
    }

    fn emit_for(
        &mut self,
        binding: Name,
        iterable: Expr,
        via: Option<lash_kernel_doc::Protocol>,
        body: Buf,
    ) {
        self.push(
            Stmt::For {
                binding,
                iterable,
                via,
                body: body.stmts,
                else_block: Vec::new(),
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

    /// Binds a closure to a new temporary.
    fn emit_closure(&mut self, params: Vec<Name>, body: Buf) -> Operand {
        let name = self.temp();
        let start = self
            .written
            .as_ref()
            .and_then(|written| written.start.clone())
            .and_then(|signature| self.start_adapter(signature));
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
        self.note_written();
        Operand::temp(name, Ty::Unknown)
    }

    /// How a function value of this signature is started when it crosses
    /// to a tool (`K-CLO-004`): the token's closure, called with an absent
    /// `this` and the list of the start's arguments, its promise awaited by
    /// `ts.await`, and undefined ending the start with null.
    fn start_adapter(&mut self, signature: Signature) -> Option<lash_kernel_doc::StartAdapter> {
        let await_fn = self.function("ts.await").ok()?;
        let same_fn = self.function("same").ok()?;
        Some(lash_kernel_doc::StartAdapter {
            signature,
            representation: crate::function_values(),
            convention: lash_kernel_doc::StartConvention::TsArgsList { await_fn, same_fn },
        })
    }

    /// Hands the last statement what is known of the function it binds.
    fn note_written(&mut self) {
        if let (Some(written), Some(note)) = (self.written.take(), self.buf.notes.last_mut()) {
            note.written = Some(written);
        }
    }

    pub(crate) fn let_expr(&mut self, value: Expr, ty: Ty) -> Operand {
        let name = self.temp();
        self.emit(Stmt::Let {
            name: name.clone(),
            value: Rhs::Expr(value),
        });
        Operand::temp(name, ty)
    }

    /// Runs an action and binds its result to a temporary.
    pub(crate) fn emit_action(&mut self, action: Action, ty: Ty) -> Operand {
        let name = self.temp();
        self.emit(Stmt::Let {
            name: name.clone(),
            value: Rhs::Action(action),
        });
        Operand::temp(name, ty)
    }

    /// The identity of a library function, listed in the manifest.
    pub(crate) fn function(&mut self, name: &str) -> Lowering<FunctionId> {
        let function = self.library.resolve(name).ok_or_else(|| {
            Diagnostic::new(
                DiagnosticCode::LinkError,
                format!("the library function `{name}` is not installed"),
                self.span,
            )
        })?;
        if let Ok(qualified) = FunctionName::new(name) {
            self.used.insert(function, qualified);
        }
        Ok(function)
    }

    /// Native calls nest; helpers with kernel bodies remain actions.
    pub(crate) fn invoke(&mut self, function: &str, args: &[Operand], ty: Ty) -> Lowering<Operand> {
        let function = self.function(function)?;
        // A helper's checked Text result is representation proof. Losing it
        // makes a later member read emit impossible record/list branches,
        // which admission correctly rejects (FIG-6372).
        let ty = if ty == Ty::Unknown
            && self
                .library
                .definition(&function)
                .is_some_and(|definition| {
                    definition.signature.result == lash_kernel_doc::Type::Text
                }) {
            Ty::Text
        } else {
            ty
        };
        if self
            .library
            .definition(&function)
            .is_some_and(|definition| definition.has_native())
        {
            return Ok(self.inline(
                Expr::Call {
                    function,
                    args: args.iter().map(Operand::expr).collect(),
                },
                ty,
            ));
        }
        let args = self.atoms(args);
        Ok(self.emit_action(
            Action::Call {
                callee: Callee::Library(function),
                args,
            },
            ty,
        ))
    }

    /// A call of a kernel function with a native implementation, as an
    /// expression (`K-LIB-002`).
    pub(crate) fn native(&mut self, function: &str, args: Vec<Expr>) -> Lowering<Expr> {
        Ok(Expr::Call {
            function: self.function(function)?,
            args,
        })
    }

    /// A number as a float. An operand known only to be a number may be an
    /// integer, which JavaScript's arithmetic and comparison do not have:
    /// `num.to_float` converts it, and raises `type_error` when the operand
    /// is no number at all.
    pub(crate) fn float(&mut self, operand: &Operand) -> Lowering<Expr> {
        if operand.ty == Ty::Float {
            return Ok(operand.expr());
        }
        self.native("num.to_float", vec![operand.expr()])
    }

    // Types.

    /// The type of the source binding `name`, where lowering now stands.
    pub(crate) fn type_of_binding(&self, name: &str) -> Ty {
        if let Some((_, ty)) = self.narrowed.iter().rev().find(|(entry, _)| entry == name) {
            return ty.clone();
        }
        self.facts.of(name).cloned().unwrap_or(Ty::Unknown)
    }

    /// What `test` proves about the names it reads.
    pub(crate) fn narrowing(&self, test: &ast::Expr) -> crate::types::Narrowing {
        let current = |name: &str| {
            self.facts
                .narrowable(name)
                .then(|| self.type_of_binding(name))
        };
        let undefined = self.facts.of("undefined").is_none() && !self.is_bound("undefined");
        crate::types::narrow(test, &current, undefined)
    }

    /// Lowers code that runs only where `shown` holds.
    pub(crate) fn narrowed<T>(
        &mut self,
        shown: Vec<(String, Ty)>,
        lower: impl FnOnce(&mut Self) -> Lowering<T>,
    ) -> Lowering<T> {
        let outer = self.narrowed.len();
        self.narrowed.extend(shown);
        let result = lower(self);
        self.narrowed.truncate(outer);
        result
    }

    /// `same(a, b)`, the kernel's identity test, as an expression.
    fn same(&mut self, left: Expr, right: Expr) -> Lowering<Expr> {
        Ok(Expr::Call {
            function: self.function("same")?,
            args: vec![left, right],
        })
    }

    /// The statement that just bound `operand`, when `operand` is a
    /// temporary nothing else has read yet.
    fn take_binding_of(&mut self, operand: &Operand) -> Option<(Rhs, Note)> {
        let Expr::Variable(name) = &operand.expr else {
            return None;
        };
        if !operand.stable || !self.temporaries.contains(name) {
            return None;
        }
        match self.buf.stmts.last() {
            Some(Stmt::Let { name: bound, .. }) if bound == name => {}
            _ => return None,
        }
        self.private.remove(name);
        let Some(Stmt::Let { value, .. }) = self.buf.stmts.pop() else {
            unreachable!("the last statement binds the temporary");
        };
        let Some(note) = self.buf.notes.pop() else {
            unreachable!("each statement has a note");
        };
        Some((value, note))
    }

    /// Declares `name` with `value`, reusing the statement that computed
    /// the value when it is the last one emitted.
    fn bind(&mut self, name: Name, value: Operand) {
        let (rhs, blocks, nested_written) = match self.take_binding_of(&value) {
            Some((rhs, note)) => {
                self.written = note.written;
                (rhs, note.blocks, note.nested_written)
            }
            None => (Rhs::Expr(value.expr()), Vec::new(), Vec::new()),
        };
        self.push(Stmt::Let { name, value: rhs }, blocks);
        self.note_written();
        if let Some(note) = self.buf.notes.last_mut() {
            note.nested_written = nested_written;
        }
    }

    /// Assigns `value` to a place whose parts are already atoms.
    fn store(&mut self, place: Place, value: Operand) {
        let value_slot = match &place {
            Place::Variable(_) => 0,
            Place::Member(lash_kernel_doc::Member::Field { .. }) => 1,
            Place::Member(lash_kernel_doc::Member::Index { .. }) => 2,
        };
        let (rhs, blocks, nested_written) = match self.take_binding_of(&value) {
            Some((rhs, note)) => {
                self.written = note.written;
                (rhs, note.blocks, note.nested_written)
            }
            None => (Rhs::Expr(value.expr()), Vec::new(), Vec::new()),
        };
        self.push(Stmt::Assign { place, value: rhs }, blocks);
        self.note_written();
        if let Some(note) = self.buf.notes.last_mut() {
            note.nested_written = nested_written
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

    /// Drops the name of a value nothing reads: the statement that computed
    /// it runs for its effect alone.
    fn discard(&mut self, value: Operand) {
        if let Some((rhs, note)) = self.take_binding_of(&value) {
            let nested_written = note.nested_written;
            match rhs {
                Rhs::Action(action) => self.push(
                    Stmt::Do {
                        value: action.into(),
                    },
                    note.blocks,
                ),
                Rhs::Expr(expr) => self.push(Stmt::Do { value: expr }, note.blocks),
            }
            if let Some(note) = self.buf.notes.last_mut() {
                note.nested_written = nested_written;
            }
        } else if !matches!(value.expr, Expr::Literal(_) | Expr::Variable(_)) {
            self.emit(Stmt::Do { value: value.expr });
        }
    }

    /// Holds a read (or a possible raise) before later statements run.
    fn pin(&mut self, operand: Operand) -> Operand {
        if operand.stable {
            return operand;
        }
        let mut pinned = self.let_expr(operand.expr, operand.ty);
        pinned.plain = operand.plain;
        pinned
    }

    /// Repeated native probes can reread an atom. A construction or nested
    /// read must run once, before the ladder branches or reuses its result.
    fn reuse(&mut self, operand: Operand) -> Operand {
        if matches!(operand.expr, Expr::Variable(_) | Expr::Literal(_)) {
            operand
        } else {
            self.pin(operand)
        }
    }

    /// Arguments of the actions the front end emits are atoms.
    fn atom(&mut self, operand: Operand) -> Atom {
        match operand.expr {
            Expr::Literal(literal) => Atom::Literal(literal),
            Expr::Variable(name) => Atom::Variable(name),
            expr => {
                let bound = self.let_expr(expr, operand.ty);
                let Expr::Variable(name) = bound.expr else {
                    unreachable!("a bound expression is a variable");
                };
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

    /// Statements emitted while lowering one operand, without opening a scope.
    fn aside<T>(&mut self, lower: impl FnOnce(&mut Self) -> Lowering<T>) -> Lowering<(Buf, T)> {
        let outer = std::mem::take(&mut self.buf);
        let result = lower(self);
        let inner = std::mem::replace(&mut self.buf, outer);
        result.map(|value| (inner, value))
    }

    /// Commit operand buffers in source order, pinning only before later statements.
    fn ordered(&mut self, parts: Vec<(Buf, Operand)>) -> Vec<Operand> {
        let mut pins = vec![false; parts.len()];
        let mut later = false;
        for (index, (buf, operand)) in parts.iter().enumerate().rev() {
            pins[index] = later && !operand.stable;
            later |= !buf.stmts.is_empty();
        }
        parts
            .into_iter()
            .zip(pins)
            .map(|((buf, operand), pin)| {
                self.append(buf);
                if pin { self.pin(operand) } else { operand }
            })
            .collect()
    }

    pub(crate) fn operands(&mut self, exprs: &[&ast::Expr]) -> Lowering<Vec<Operand>> {
        let mut parts = Vec::with_capacity(exprs.len());
        for expr in exprs {
            parts.push(self.aside(|this| this.lower_expr(expr))?);
        }
        Ok(self.ordered(parts))
    }

    /// Leave room for the enclosing statement, blocks, and native wrappers
    /// under K-DOC-006. Hoist a deep child before its parent adds another level.
    fn inline(&mut self, expr: Expr, ty: Ty) -> Operand {
        let mut pending = vec![(lash_kernel_doc::Node::Expr(&expr), 0)];
        let limit = lash_kernel_doc::MAX_NESTING_DEPTH.saturating_sub(2 * self.kernel_depth + 8);
        while let Some((node, depth)) = pending.pop() {
            if depth >= limit {
                return self.let_expr(expr, ty);
            }
            pending.extend(node.children().into_iter().map(|child| (child, depth + 1)));
        }
        Operand {
            expr,
            ty,
            stable: false,
            plain: None,
        }
    }

    /// A bool the kernel's `if` and `while` accept: the operand itself when
    /// it is ty to be one, else its ToBoolean.
    fn condition(&mut self, operand: Operand) -> Lowering<Expr> {
        if operand.ty == Ty::Bool {
            return Ok(operand.expr());
        }
        Ok(self.invoke("ts.to_boolean", &[operand], Ty::Bool)?.expr())
    }

    /// A read of `target[index]` where the target is ty to be a list or
    /// a tuple the lowerer built.
    fn element(target: &Operand, index: usize) -> Expr {
        Expr::Member(Box::new(Member::Index {
            target: target.expr(),
            #[expect(clippy::cast_precision_loss, reason = "a position in a source list")]
            index: Expr::Literal(Literal::Float(Float::new(index as f64))),
        }))
    }
}

pub(crate) fn unknown_binding(name: &str, span: Option<SourceSpan>) -> Diagnostic {
    // These unbound spellings are references into the printer's base, not
    // authored variables. Bound names have already resolved before this point.
    if name.starts_with("helper_") || name.starts_with("kernel_name_") || name == "declared" {
        return Diagnostic::refusal(
            DiagnosticCode::PrintedSourceNeedsBase,
            format!(
                "`{name}` needs its printed document's base; use `typescript::read(source, base)`"
            ),
            span,
        );
    }
    Diagnostic::defect(
        DiagnosticCode::UnknownBinding,
        format!("`{name}` is not defined"),
        span,
    )
}
