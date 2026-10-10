//! Lowers the adapter's tree to a kernel document.
//!
//! The output is three-address code. Every operation whose JavaScript
//! meaning differs from the kernel's is a call to a helper with a
//! kernel-code body, and such a call is the whole right-hand side of its
//! own statement (`K-STMT-001`), so each intermediate value is bound to a
//! temporary in the source's evaluation order (`K-STMT-005`). An operand
//! that is a source variable is copied to a temporary whenever anything
//! evaluated after it and before its use could run other code, because that
//! code, or another task while it waits, may assign the variable.
//!
//! Where the type analysis (`crate::types`) knows an operand's type, the
//! operation is instead a direct call of the kernel function for that type,
//! inside an expression: `num.add(a, b)` for two numbers.
//!
//! A JavaScript function is a closure of two parameters, `fn(this, args)`.
//! Source bindings keep their names; a binding that would shadow another in
//! scope, and every temporary, takes a name the source does not spell, so
//! no kernel variable in a document shadows another.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lash_kernel_dialect::{Environment, Library, Lowered};
use lash_kernel_doc::{
    Action, Atom, Block, Callee, Catch, Closure, Document, EffectName, Expr, Float,
    FunctionCatalog, FunctionId, FunctionName, Literal, Member, Name, NumberPolicy, Place, Rhs,
    Signature, Stmt, TryStmt,
};

use crate::adapter as ast;
use crate::builtins::{self, Table};
use crate::node_label::NodeLabel;
use crate::types::{Facts, Ty};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

mod annotate;
mod async_fn;
mod calls;
mod expressions;
mod functions;
mod patterns;
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
    "list.check",
    "list.get",
    "list.len",
    "num.add",
    "num.div",
    "num.le",
    "num.lt",
    "num.mul",
    "num.neg",
    "num.rem_trunc",
    "num.sub",
    "num.to_float",
    "same",
    "text.concat",
    "text.join",
    "text.len",
    "ts.add",
    "ts.arguments",
    "ts.await",
    "ts.assign",
    "ts.bit_and",
    "ts.bit_not",
    "ts.bit_or",
    "ts.bit_xor",
    "ts.call_member",
    "ts.callable",
    "ts.delete",
    "ts.div",
    "ts.ge",
    "ts.get",
    "ts.gt",
    "ts.has",
    "ts.in",
    "ts.is_nullish",
    "ts.iterate",
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
    "ts.sub",
    "ts.tdz",
    "ts.to_boolean",
    "ts.to_number",
    "ts.to_string",
    "ts.typeof",
    "ts.ushr",
];

/// A value an expression lowered to: a variable or a literal, never a
/// computation.
#[derive(Clone, Debug)]
pub(crate) struct Operand {
    pub(crate) atom: Atom,
    pub(crate) ty: Ty,
}

impl Operand {
    pub(crate) fn undefined() -> Self {
        Self {
            atom: Atom::Literal(Literal::Absent),
            ty: Ty::Undefined,
        }
    }

    pub(crate) fn null() -> Self {
        Self {
            atom: Atom::Literal(Literal::Null),
            ty: Ty::Null,
        }
    }

    pub(crate) fn bool(value: bool) -> Self {
        Self {
            atom: Atom::Literal(Literal::Bool(value)),
            ty: Ty::Bool,
        }
    }

    pub(crate) fn number(value: f64) -> Self {
        Self {
            atom: Atom::Literal(Literal::Float(Float::new(value))),
            ty: Ty::Float,
        }
    }

    pub(crate) fn text(value: impl Into<String>) -> Self {
        Self {
            atom: Atom::Literal(Literal::Text(value.into())),
            ty: Ty::Text,
        }
    }

    pub(crate) fn variable(name: Name, ty: Ty) -> Self {
        Self {
            atom: Atom::Variable(name),
            ty,
        }
    }

    pub(crate) fn expr(&self) -> Expr {
        match &self.atom {
            Atom::Variable(name) => Expr::Variable(name.clone()),
            Atom::Literal(literal) => Expr::Literal(literal.clone()),
        }
    }
}

/// Where a statement came from, kept beside the statement until the
/// document is whole and its sites can be counted.
#[derive(Clone, Debug, Default)]
pub(crate) struct Note {
    pub(crate) span: Option<SourceSpan>,
    pub(crate) label: Option<NodeLabel>,
    /// What the function the statement binds is, as its source wrote it.
    pub(crate) written: Option<serde_json::Value>,
    /// The notes of the statement's blocks, in the order a walk of its
    /// children meets them.
    pub(crate) blocks: Vec<Vec<Note>>,
}

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
}

#[derive(Debug, Default)]
struct Scope {
    bindings: HashMap<String, Binding>,
    /// Where in its buffer the scope began: predeclarations go there.
    start: usize,
    kernel_depth: usize,
    predeclare: Vec<(Name, Expr)>,
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
    /// The functions the document declares: its processes.
    declared: BTreeMap<Name, lash_kernel_doc::Function>,
    /// The declared functions a host may start, with their signatures.
    entries: BTreeMap<Name, Signature>,
    /// While a process body is lowered: the cell's names, none of which the
    /// body may read.
    lifting: Option<HashSet<String>>,
    /// The functions the session holds, and the ones the source names: the
    /// document declares those.
    saved: &'a BTreeMap<Name, lash_kernel_dialect::SavedFunction>,
    saved_used: BTreeSet<Name>,
    /// What the closure just emitted is, as its source wrote it, until the
    /// statement that binds it takes it.
    written: Option<serde_json::Value>,
    /// Every name the source assigns to.
    assigned: &'a BTreeSet<String>,
}

/// Session names are checked before either source or saved state can mask a built-in.
/// A tool namespace root (`control`, `tools`, ...: the first segment of an
/// effect the environment offers) is reserved the same way: a binding of
/// that name would hide the session's tools from every later cell.
pub(crate) fn check_binding_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    effects: &BTreeMap<EffectName, lash_kernel_doc::Signature>,
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
        if effects
            .keys()
            .any(|effect| effect.as_str().split('.').next() == Some(name))
        {
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
) -> Lowering<Lowered> {
    let mut taken: HashSet<String> = source
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    taken.extend(
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
        session: environment.bindings,
        table: builtins::table(),
        taken,
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
        facts: Facts::analyse(program, environment.bindings),
        assigned: &program.assigned,
        narrowed: Vec::new(),
        declared: BTreeMap::new(),
        entries: BTreeMap::new(),
        lifting: None,
        saved: environment.functions,
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
    let mut main = std::mem::take(&mut lowerer.buf);
    // A saved function the cell uses is the token it was saved in, which
    // the session starts as a reference to its declaration: made once, and
    // bound to each of its names the cell uses, so they stay one function.
    let mut made: BTreeMap<&Name, &Name> = BTreeMap::new();
    let remade: Vec<(Stmt, Note)> = lowerer
        .saved_used
        .iter()
        .filter_map(|name| {
            let function = environment.functions.get(name)?;
            let value = match made.get(&function.name) {
                Some(first) => Expr::Variable((*first).clone()),
                None => {
                    let token = function.value()?;
                    made.insert(&function.name, name);
                    token
                }
            };
            Some((
                Stmt::Assign {
                    place: Place::Variable(name.clone()),
                    value: Rhs::Expr(value),
                },
                Note::default(),
            ))
        })
        .collect();
    let (stmts, notes): (Vec<Stmt>, Vec<Note>) = remade.into_iter().unzip();
    main.stmts.splice(0..0, stmts);
    main.notes.splice(0..0, notes);
    let mut document = Document::new(NumberPolicy::Float, main.stmts);
    document.private_bindings = lowerer.private;
    document.functions = lowerer.declared;
    document.entries = lowerer.entries;
    document.manifest.functions = reachable(lowerer.used, environment.library);
    document.manifest.effects = lowerer.performed;
    if !lowerer.saved_used.is_empty() {
        // The saved functions the source names are declared in the cell's
        // own document, with what they require.
        let catalog: &dyn FunctionCatalog = environment.library;
        lash_kernel_dialect::install(
            &mut document,
            environment.functions,
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
        for (offset, (name, value)) in scope.predeclare.into_iter().enumerate() {
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
            self.buf.notes.insert(scope.start + offset, Note::default());
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
        self.scopes[scope].predeclare.push((kernel, value));
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
        let Atom::Variable(checked) = checked.atom else {
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
            label: None,
            written: None,
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

    /// Binds a closure to a new temporary.
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
        self.note_written();
        Operand::variable(name, Ty::Unknown)
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
        Operand::variable(name, ty)
    }

    /// Runs an action and binds its result to a temporary.
    pub(crate) fn emit_action(&mut self, action: Action, ty: Ty) -> Operand {
        let name = self.temp();
        self.emit(Stmt::Let {
            name: name.clone(),
            value: Rhs::Action(action),
        });
        Operand::variable(name, ty)
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

    /// Calls a helper as its own statement.
    pub(crate) fn invoke(&mut self, function: &str, args: &[Operand], ty: Ty) -> Lowering<Operand> {
        let function = self.function(function)?;
        Ok(self.emit_action(
            Action::Call {
                callee: Callee::Library(function),
                args: args.iter().map(|arg| arg.atom.clone()).collect(),
            },
            ty,
        ))
    }

    /// A call of a kernel function with a native implementation, as an
    /// expression (`K-STMT-002`).
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
        let Atom::Variable(name) = &operand.atom else {
            return None;
        };
        if !self.temporaries.contains(name) {
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
        let (rhs, blocks) = match self.take_binding_of(&value) {
            Some((rhs, note)) => {
                self.written = note.written;
                (rhs, note.blocks)
            }
            None => (Rhs::Expr(value.expr()), Vec::new()),
        };
        self.push(Stmt::Let { name, value: rhs }, blocks);
        self.note_written();
    }

    /// Assigns `value` to a place whose parts are already atoms.
    fn store(&mut self, place: Place, value: Operand) {
        let (rhs, blocks) = match self.take_binding_of(&value) {
            Some((rhs, note)) => {
                self.written = note.written;
                (rhs, note.blocks)
            }
            None => (Rhs::Expr(value.expr()), Vec::new()),
        };
        self.push(Stmt::Assign { place, value: rhs }, blocks);
        self.note_written();
    }

    /// Drops the name of a value nothing reads: the statement that computed
    /// it runs for its effect alone.
    fn discard(&mut self, value: Operand) {
        if let Some((rhs, note)) = self.take_binding_of(&value) {
            match rhs {
                Rhs::Action(action) => self.push(Stmt::Do { action }, note.blocks),
                Rhs::Expr(expr) => {
                    // Kept: an expression may raise, and a host may read it.
                    let Atom::Variable(name) = value.atom else {
                        unreachable!("a taken binding is a variable");
                    };
                    self.push(
                        Stmt::Let {
                            name,
                            value: Rhs::Expr(expr),
                        },
                        note.blocks,
                    );
                }
            }
        }
    }

    /// Copies a source variable to a temporary, so that code that runs
    /// before the operand is used cannot change what was read.
    fn pin(&mut self, operand: Operand) -> Operand {
        match &operand.atom {
            Atom::Variable(name) if !self.temporaries.contains(name) => {
                self.let_expr(operand.expr(), operand.ty.clone())
            }
            _ => operand,
        }
    }

    /// Lowers operands the source evaluates left to right, pinning each one
    /// that a later operand's evaluation could invalidate.
    pub(crate) fn operands(&mut self, exprs: &[&ast::Expr]) -> Lowering<Vec<Operand>> {
        let mut operands = Vec::with_capacity(exprs.len());
        for (index, expr) in exprs.iter().enumerate() {
            let operand = self.lower_expr(expr)?;
            let later_runs_code = exprs[index + 1..]
                .iter()
                .any(|later| !walk::is_inert(later));
            operands.push(if later_runs_code {
                self.pin(operand)
            } else {
                operand
            });
        }
        Ok(operands)
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
    Diagnostic::defect(
        DiagnosticCode::UnknownBinding,
        format!("`{name}` is not defined"),
        span,
    )
}
