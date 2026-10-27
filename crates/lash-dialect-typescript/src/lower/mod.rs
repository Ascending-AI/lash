//! Lowers the adapter's tree to a kernel document.
//!
//! Native calls, member reads and constructions stay in expressions
//! (`K-LIB-002`). Calls, awaits and effects nest too (`K-EVAL-008`).
//! An earlier operand is pinned when a later
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
    AssignOrder, Block, Callee, Catch, Closure, Document, Expr, Float, Literal, Member,
    NumberPolicy, Place, Rhs, Stmt, TryStmt,
};

use {lash_kernel_value::FunctionId, lash_kernel_value::FunctionName, lash_kernel_value::Name};

use crate::adapter as ast;
use crate::types::{Facts, PlainRecord, Ty};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};
use lash_ext_ts_helpers::builtins::{self, Table};
use lash_kernel_dialect::Spelling;

mod async_fn;
mod calls;
mod captions;
mod constructors;
pub(crate) use calls::tool_calls;
mod expressions;
mod functions;
mod implicit_text;
mod labels;
mod lazy;
mod packages;
mod patterns;
mod positional;
pub(crate) mod primitive_methods;
mod process;
mod statements;
pub(crate) mod walk;

pub(crate) type Lowering<T> = Result<T, Diagnostic>;

/// An expression, its representation facts, and whether later
/// statements can change its value or run before it raises.
#[derive(Clone, Debug)]
pub(crate) struct Operand {
    pub(crate) expr: Expr,
    stable: bool,
    pub(crate) ty: Ty,
    /// Proven representation and stable nested fields, never an annotation
    /// or part of a saved value. Boundness is checked before this is attached.
    pub(crate) plain: Option<PlainRecord>,
    /// The literal text the value is proven by construction to begin
    /// with, never by an annotation: `Some` only where it is a text.
    pub(crate) text_prefix: Option<String>,
}

impl Operand {
    pub(crate) fn undefined() -> Self {
        Self {
            expr: Expr::Literal(Literal::Absent),
            ty: Ty::Undefined,
            plain: None,
            text_prefix: None,
            stable: true,
        }
    }

    pub(crate) fn null() -> Self {
        Self {
            expr: Expr::Literal(Literal::Null),
            ty: Ty::Null,
            plain: None,
            text_prefix: None,
            stable: true,
        }
    }

    pub(crate) fn bool(value: bool) -> Self {
        Self {
            expr: Expr::Literal(Literal::Bool(value)),
            ty: Ty::Bool,
            plain: None,
            text_prefix: None,
            stable: true,
        }
    }

    pub(crate) fn number(value: f64) -> Self {
        Self {
            expr: Expr::Literal(Literal::Float(Float::new(value))),
            ty: Ty::Float,
            plain: None,
            text_prefix: None,
            stable: true,
        }
    }

    pub(crate) fn text(value: impl Into<String>) -> Self {
        Self {
            expr: Expr::Literal(Literal::Text(value.into())),
            ty: Ty::Text,
            plain: None,
            text_prefix: None,
            stable: true,
        }
    }

    pub(crate) fn variable(name: Name, ty: Ty) -> Self {
        Self {
            expr: Expr::Variable(name),
            ty,
            plain: None,
            text_prefix: None,
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

/// A kernel block under construction: each statement with the note of
/// its source layers, so a move, removal or fold carries both.
#[derive(Debug, Default)]
pub(crate) struct Buf {
    implicit_return: bool,
    emitted: Vec<Emitted>,
}

#[derive(Debug)]
struct Emitted {
    stmt: Stmt,
    note: Note,
}

impl Buf {
    /// The block of the one statement `stmt`.
    fn one(stmt: Stmt, note: Note) -> Self {
        Self {
            implicit_return: false,
            emitted: vec![Emitted { stmt, note }],
        }
    }

    fn len(&self) -> usize {
        self.emitted.len()
    }

    fn is_empty(&self) -> bool {
        self.emitted.is_empty()
    }

    fn stmts(&self) -> impl DoubleEndedIterator<Item = &Stmt> + ExactSizeIterator {
        self.emitted.iter().map(|emitted| &emitted.stmt)
    }

    fn notes(&self) -> impl Iterator<Item = &Note> {
        self.emitted.iter().map(|emitted| &emitted.note)
    }

    fn last_stmt(&self) -> Option<&Stmt> {
        self.emitted.last().map(|emitted| &emitted.stmt)
    }

    fn last_note_mut(&mut self) -> Option<&mut Note> {
        self.emitted.last_mut().map(|emitted| &mut emitted.note)
    }

    /// The statement at `index` and its note, to change.
    fn get_mut(&mut self, index: usize) -> Option<(&Stmt, &mut Note)> {
        self.emitted
            .get_mut(index)
            .map(|emitted| (&emitted.stmt, &mut emitted.note))
    }

    fn push(&mut self, stmt: Stmt, note: Note) {
        self.emitted.push(Emitted { stmt, note });
    }

    fn insert(&mut self, index: usize, stmt: Stmt, note: Note) {
        self.emitted.insert(index, Emitted { stmt, note });
    }

    fn pop(&mut self) -> Option<(Stmt, Note)> {
        self.emitted
            .pop()
            .map(|emitted| (emitted.stmt, emitted.note))
    }

    /// Puts `buf`'s statements before the one at `index`.
    fn splice(&mut self, index: usize, buf: Buf) {
        self.emitted.splice(index..index, buf.emitted);
    }

    fn extend(&mut self, buf: Buf) {
        if !buf.is_empty() {
            self.implicit_return = buf.implicit_return;
        }
        self.emitted.extend(buf.emitted);
    }

    /// The finished block, and the notes of its statements in order.
    fn into_parts(self) -> (Block, Vec<Note>) {
        self.emitted
            .into_iter()
            .map(|emitted| (emitted.stmt, emitted.note))
            .unzip()
    }

    fn into_notes(self) -> impl Iterator<Item = Note> {
        self.emitted.into_iter().map(|emitted| emitted.note)
    }
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

/// Work a source `continue` must run before the kernel's own continue.
#[derive(Clone, Debug)]
enum BeforeContinue {
    Statements(Block),
    /// Lower at each continue site with fresh temporaries, in the head's
    /// lexical environment rather than a shadowing body block's.
    ForUpdate {
        expression: Box<ast::Expr>,
        scope_depth: usize,
        narrowed_depth: usize,
    },
}

/// What `break` and `continue` leave.
#[derive(Debug)]
enum Control {
    Loop {
        /// What a `continue` runs before the kernel's own `continue`.
        before_continue: BeforeContinue,
    },
    Switch {
        /// Set by a `continue` inside the switch, which first has to leave
        /// the loop the switch is lowered to.
        continue_flag: Option<Name>,
    },
}

#[derive(Debug)]
struct FunctionFrame {
    implicit_text_conversions: Vec<lash_kernel_doc::ImplicitTextConversion>,
    arrow: bool,
    async_body: bool,
    this: Option<Name>,
    args: Option<Name>,
    arguments_used: bool,
    context_used: bool,
    controls: Vec<Control>,
    labels: Vec<labels::Label>,
}

pub(crate) struct Lowerer<'a> {
    source: &'a str,
    implicit_text_conversion: workflow::ImplicitTextConversionPolicy,
    imported: &'a BTreeMap<String, Vec<String>>,
    /// The entries of a workflow module, as they are lowered; `None` for a
    /// cell.
    module: Option<workflow::Module>,
    library: &'a dyn Library,
    /// The packages and offered operations a dotted call resolves through:
    /// a call of an operation is a tool call. An operation that declares
    /// controls ends the turn: a control call ends `main` when it settles,
    /// and is written only where it can.
    namespaces: &'a lash_kernel_dialect::Namespaces,
    /// The effects the document performs, for its manifest (`K-DOC-002`).
    imports: lash_kernel_dialect::authoring::Imports,
    local_functions: BTreeMap<FunctionId, lash_kernel_doc::FunctionDefinition>,
    templates: BTreeMap<usize, Name>,
    local_notes: BTreeMap<FunctionId, Vec<Note>>,
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
    /// The notes of each declared function's body.
    declared_notes: BTreeMap<FunctionId, Vec<Note>>,
    /// While a process body is lowered: the cell's names, none of which the
    /// body may read.
    lifting: Option<HashSet<String>>,
    module_statements: &'a [ast::Stmt],
    /// The functions the session holds, and the ones the source names: the
    /// document declares those.
    /// What the closure just emitted is, as its source wrote it, until the
    /// statement that binds it takes it.
    written: Option<lash_kernel_dialect::Written>,
    /// The captions the source spells, which lowering hands each to the one
    /// statement that owns its construct.
    spelled: &'a crate::captions::Spelled,
    /// The captions of the module's entries, their parameters and their
    /// types' fields.
    declared_captions: Vec<lash_kernel_doc::CaptionEntry>,
    cell_result_at: Option<usize>,
}

/// Lowers a parsed program under the host's module admission policy.
#[cfg(test)]
pub(crate) fn lower(
    program: &ast::Program,
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    policy: workflow::ImplicitTextConversionPolicy,
) -> Lowering<Lowered> {
    lower_result(program, source, environment, mode, policy, false)
}

pub(crate) fn lower_result(
    program: &ast::Program,
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    policy: workflow::ImplicitTextConversionPolicy,
    cell_result: bool,
) -> Lowering<Lowered> {
    let cell_result = cell_result && matches!(mode, Mode::Cell(_));
    let policy = match mode {
        Mode::Cell(_) => workflow::ImplicitTextConversionPolicy::Allow,
        Mode::Module => policy,
    };
    lower_beside_result(program, source, environment, mode, &[], policy, cell_result)
}

/// Lower with the words of adjacent generated source reserved from fresh names.
/// Print verification selects `Allow`; authored reads and modules carry host policy.
pub(crate) fn lower_beside(
    program: &ast::Program,
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    beside: &[String],
    policy: workflow::ImplicitTextConversionPolicy,
) -> Lowering<Lowered> {
    lower_beside_result(program, source, environment, mode, beside, policy, false)
}

fn lower_beside_result(
    program: &ast::Program,
    source: &str,
    environment: &Environment<'_>,
    mode: Mode<'_>,
    beside: &[String],
    policy: workflow::ImplicitTextConversionPolicy,
    cell_result: bool,
) -> Lowering<Lowered> {
    let implicit_text_conversion = policy;
    let (cell, module) = match mode {
        Mode::Cell(cell) => (cell, None),
        Mode::Module => (CellEnvironment::first(), Some(workflow::Module::new())),
    };
    // A word of a comment names nothing, so caption text never changes a
    // generated name (`K-CAP-001`).
    let mut code = Vec::with_capacity(program.comments.len() + 1);
    let mut from = 0;
    for comment in &program.comments {
        code.push(&source[from..comment.start]);
        from = comment.end;
    }
    code.push(&source[from..]);
    let mut taken: HashSet<String> = code
        .into_iter()
        .flat_map(|code| code.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    taken.extend(cell.bindings.iter().map(|name| name.as_str().to_string()));
    taken.extend(beside.iter().cloned());
    let mut lowerer = Lowerer {
        source,
        implicit_text_conversion,
        imported: &program.imported,
        module,
        library: environment.library,
        namespaces: environment.namespaces,
        imports: lash_kernel_dialect::authoring::Imports::default().reserving(
            lash_ext_ts_helpers::package::helper_operations()
                .iter()
                .map(|operation| operation.slot),
        ),
        local_functions: BTreeMap::new(),
        templates: BTreeMap::new(),
        local_notes: BTreeMap::new(),
        session: cell.bindings,
        table: builtins::table(),
        taken,
        next_fresh: HashMap::new(),
        temporaries: HashSet::new(),
        scopes: Vec::new(),
        functions: vec![FunctionFrame {
            implicit_text_conversions: Vec::new(),
            arrow: false,
            async_body: false,
            this: None,
            args: None,
            arguments_used: false,
            context_used: false,
            controls: Vec::new(),
            labels: Vec::new(),
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
                .namespaces
                .operations()
                .filter(|(_, offered)| {
                    offered
                        .controls
                        .contains(lash_kernel_value::TurnControlKind::Finish)
                })
                .map(|(operation, _)| {
                    lash_kernel_dialect::spell_operation(Spelling::TypeScript, &operation)
                })
                .collect(),
            &environment
                .namespaces
                .operations()
                .map(|(operation, offered)| {
                    (
                        lash_kernel_dialect::spell_operation(Spelling::TypeScript, &operation),
                        &offered.signature,
                    )
                })
                .collect(),
            crate::types::Packages::of(environment.namespaces),
            environment.library,
        ),
        narrowed: Vec::new(),
        paths: Vec::new(),
        declared_notes: BTreeMap::new(),
        lifting: None,
        module_statements: &program.statements,
        written: None,
        spelled: &program.captions,
        declared_captions: Vec::new(),
        cell_result_at: if cell_result {
            program
                .statements
                .iter()
                .rev()
                .find(|stmt| !matches!(stmt.unspanned(), ast::Stmt::Empty))
                .and_then(|stmt| match stmt {
                    ast::Stmt::Spanned(span, inner)
                        if matches!(inner.unspanned(), ast::Stmt::Expr(_)) =>
                    {
                        Some(span.start)
                    }
                    _ => None,
                })
        } else {
            None
        },
    };
    lowerer.push_scope();
    lowerer.declare_vars(&program.statements);
    lowerer.declare_block(&program.statements)?;
    lowerer.lower_statements(&program.statements)?;
    lowerer.pop_scope();
    let (main, main_notes) = std::mem::take(&mut lowerer.buf).into_parts();
    // Saved function values are already bound by S8 adoption. Their token
    // and frozen captures stay in heap pages; lowering never reconstructs
    // those payloads as source expressions.
    let mut document = Document::new(NumberPolicy::Float, main);
    document.private_bindings = lowerer.private;
    document.functions = lowerer.local_functions;
    if let Some(module) = lowerer.module.take() {
        module
            .assemble(&mut document, &mut lowerer.declared_notes)
            .map_err(Diagnostic::module)?;
        // The module contract exports its declared entries. An authored
        // alias names that same callable; it creates no second definition.
        let aliases: Vec<_> = program
            .export_aliases
            .iter()
            .filter_map(|(alias, local)| {
                document
                    .exports
                    .get(&Name::new(local))
                    .cloned()
                    .map(|export| (Name::new(alias), export))
            })
            .collect();
        for (alias, export) in aliases {
            if document
                .exports
                .get(&alias)
                .is_some_and(|existing| existing != &export)
            {
                return Err(Diagnostic::module(
                    workflow::ModuleRefusal::DuplicateEntry.at(
                        format!("the module declares more than one entry named `{alias}`"),
                        None,
                    ),
                ));
            }
            document.exports.insert(alias, export);
        }
    }
    document.imports = lowerer.imports.into_map();
    document
        .requires
        .kernels
        .insert(lash_kernel_doc::KERNEL_VERSION);
    lash_kernel_dialect::authoring::include_library_dependencies(
        &mut document,
        lowerer.used.keys().copied(),
        environment.library,
    )
    .map_err(|message| Diagnostic::new(DiagnosticCode::LinkError, message, lowerer.span))?;
    // The saved functions the source names, and each one carried data
    // holds, are declared in the cell's own document, with what they
    // require.
    lowerer.declared_notes.extend(lowerer.local_notes);
    let document = lash_kernel_doc::IdentifiedDocument::new(document);
    let layers = lash_kernel_dialect::site_layers(
        &document,
        source,
        lash_kernel_doc::SourceLanguage::TypeScript,
        &main_notes,
        &lowerer.declared_notes,
        &lowerer.declared_captions,
    )
    .map_err(|refused| Diagnostic::new(DiagnosticCode::InvalidAst, refused.to_string(), None))?;
    Ok(Lowered {
        document,
        source_map: Some(layers.source_map),
        written: layers.written,
        captions: layers.captions,
        validated: false,
    })
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
            start: self.buf.len(),
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
            self.buf.insert(
                scope.start + offset,
                Stmt::Let {
                    name,
                    value: Rhs::Expr(value),
                },
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

    /// Whether the program's spelling `name` reads a function declaration
    /// that nothing assigns to, which therefore always holds the token of
    /// the function it declares.
    pub(crate) fn holds_declared_function(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .is_some_and(|binding| binding.kind == BindingKind::Function)
            && !self.facts.assigned(name)
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
        let checked = self.pin(checked);
        let Expr::Variable(checked) = checked.expr else {
            unreachable!("a pinned check is a variable");
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

    fn push(&mut self, mut stmt: Stmt, blocks: Vec<Vec<Note>>) {
        if let Stmt::Let { value, .. } | Stmt::Assign { value, .. } = &mut stmt
            && let Rhs::Expr(Expr::Act(action)) = value
            && let Some(action) = action.as_action()
        {
            *value = Rhs::Action(action);
        }
        if let Stmt::Let { name, .. } = &stmt {
            let name = name.clone();
            self.note_private(&name);
        }
        self.buf.push(
            stmt,
            Note {
                span: self.span,
                blocks,
                ..Note::default()
            },
        );
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
        for statement in buf.stmts() {
            if let Stmt::Let { name, .. } = statement {
                self.note_private(name);
            }
        }
        self.buf.extend(buf);
    }

    fn emit_if(&mut self, condition: Expr, then_block: Buf, else_block: Buf) {
        let (then_block, then_notes) = then_block.into_parts();
        let (else_block, else_notes) = else_block.into_parts();
        self.push(
            Stmt::If {
                condition,
                then_block,
                else_block,
            },
            vec![then_notes, else_notes],
        );
    }

    fn emit_while(&mut self, condition: Expr, body: Buf) {
        let (body, notes) = body.into_parts();
        self.push(
            Stmt::While {
                metered: 0,
                condition,
                body,
                else_block: Vec::new(),
            },
            vec![notes],
        );
    }

    fn emit_for(
        &mut self,
        binding: Name,
        iterable: Expr,
        via: Option<lash_kernel_doc::Protocol>,
        body: Buf,
    ) {
        let (body, notes) = body.into_parts();
        self.push(
            Stmt::For {
                metered: 0,
                binding,
                iterable,
                via,
                body,
                else_block: Vec::new(),
            },
            vec![notes],
        );
    }

    fn emit_try(&mut self, body: Buf, catch: Option<(Name, Buf)>, finally: Option<Buf>) {
        let (body, notes) = body.into_parts();
        let mut blocks = vec![notes];
        let catch = catch.map(|(binding, buf)| {
            let (body, notes) = buf.into_parts();
            blocks.push(notes);
            Catch { binding, body }
        });
        let finally = finally.map(|buf| {
            let (body, notes) = buf.into_parts();
            blocks.push(notes);
            body
        });
        self.push(
            Stmt::Try(TryStmt {
                body,
                catch,
                finally,
            }),
            blocks,
        );
    }

    fn emit_closure(
        &mut self,
        params: Vec<Name>,
        body: Buf,
        start: Option<lash_kernel_doc::CallablePresentation>,
    ) -> Operand {
        self.emit_named_closure(params, body, start, "")
    }

    fn emit_named_closure(
        &mut self,
        params: Vec<Name>,
        body: Buf,
        start: Option<lash_kernel_doc::CallablePresentation>,
        authored_name: &str,
    ) -> Operand {
        let name = self.temp();
        let implicit_return = body.implicit_return;
        let (block, notes) = body.into_parts();
        let mut definition = lash_kernel_dialect::authoring::local_function(
            params,
            block,
            &self.imports.as_map(),
            self.library,
            &self.local_functions,
        );
        if let Ok(name) = lash_kernel_value::FunctionName::new(authored_name) {
            definition.name = name;
        }
        if implicit_return {
            definition.projection = lash_kernel_doc::ProjectionRole::ImplicitReturn;
        }
        let function = definition.identity();
        self.local_functions.insert(function, definition);
        self.local_notes.insert(function, notes);
        self.emit(Stmt::Let {
            name: name.clone(),
            value: Rhs::Expr(Expr::Closure(Box::new(Closure {
                function,
                presentation: start,
            }))),
        });
        self.note_written();
        Operand::temp(name, Ty::Unknown)
    }

    /// A source function token always holds a closure of `(this, args)`.
    /// Its presentation describes that retained body, including after crossing.
    fn callable_presentation(
        &mut self,
        function: &ast::Function,
    ) -> Option<lash_kernel_doc::CallablePresentation> {
        let (signature, _) = self.entry_declaration(function);
        let convention = self.entry_convention(true).ok()?;
        Some(lash_kernel_doc::CallablePresentation {
            signature,
            representation: crate::function_values(),
            convention,
        })
    }

    /// Hands the last statement what is known of the function it binds.
    fn note_written(&mut self) {
        if let (Some(written), Some(note)) = (self.written.take(), self.buf.last_note_mut()) {
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

    fn action(&mut self, kind: lash_kernel_doc::ActKind, args: Vec<Expr>, ty: Ty) -> Operand {
        self.inline(
            Expr::Act(Box::new(lash_kernel_doc::ActExpr { kind, args })),
            ty,
        )
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

    /// Build helpers retain exact identity and execute in the caller's operation context.
    fn function_target(&mut self, name: &str) -> Lowering<lash_kernel_doc::FunctionTarget> {
        Ok(lash_kernel_doc::FunctionTarget::Local(self.function(name)?))
    }

    fn helper_callee(&mut self, name: &str) -> Lowering<Callee> {
        Ok(match self.function_target(name)? {
            lash_kernel_doc::FunctionTarget::Local(function) => Callee::Local(function),
            lash_kernel_doc::FunctionTarget::Import(slot) => Callee::Import(slot),
        })
    }

    /// Native and body helpers both nest, using their admitted call form.
    pub(crate) fn invoke(&mut self, function: &str, args: &[Operand], ty: Ty) -> Lowering<Operand> {
        let identity = self.function(function)?;
        let target = self.helper_callee(function)?;
        // Its header alone: a first use decodes no body (FIG-6931).
        let header = self.library.header(&identity);
        // Checked helper results prove their representation. Losing that
        // proof emits impossible record/list probes in later member reads.
        let ty = if ty == Ty::Unknown {
            match header.as_ref().map(|header| &header.signature.result) {
                Some(lash_kernel_value::Type::Text) => Ty::Text,
                Some(lash_kernel_value::Type::List(_)) => Ty::Array,
                _ => ty,
            }
        } else {
            ty
        };
        if header.as_ref().is_some_and(|header| header.has_native()) {
            return Ok(self.inline(
                Expr::Call {
                    function: identity,
                    args: args.iter().map(Operand::expr).collect(),
                },
                ty,
            ));
        }
        Ok(self.action(
            lash_kernel_doc::ActKind::Call(target),
            args.iter().map(Operand::expr).collect(),
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

    /// A proven float is already a Number. A trusted Number annotation
    /// must check its runtime kind, including refusing a BigInt.
    pub(crate) fn float(&mut self, operand: &Operand) -> Lowering<Expr> {
        if operand.ty == Ty::Float {
            return Ok(operand.expr());
        }
        self.native("ts.number.checked", vec![operand.expr()])
    }

    // Types.

    /// The type of the source binding `name` reads, where lowering now
    /// stands. `name` is the program's own spelling of the read, which the
    /// type analysis resolved to its binding.
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
        let undefined = !self.facts.names("undefined") && !self.is_bound("undefined");
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
        match self.buf.last_stmt() {
            Some(Stmt::Let { name: bound, .. }) if bound == name => {}
            _ => return None,
        }
        self.private.remove(name);
        let Some((Stmt::Let { value, .. }, note)) = self.buf.pop() else {
            unreachable!("the last statement binds the temporary");
        };
        Some((value, note))
    }

    /// Declares `name` with `value`, reusing the statement that computed
    /// the value when it is the last one emitted.
    fn bind(&mut self, name: Name, value: Operand) {
        let (rhs, note) = match self.take_binding_of(&value) {
            Some((rhs, mut note)) => {
                self.written = note.written.take();
                (rhs, note)
            }
            None => (Rhs::Expr(value.expr()), Note::default()),
        };
        let Note {
            blocks,
            nested_written,
            caption,
            arm_captions,
            ..
        } = note;
        self.push(Stmt::Let { name, value: rhs }, blocks);
        self.note_written();
        if let Some(note) = self.buf.last_note_mut() {
            note.nested_written = nested_written;
            note.caption = caption;
            note.arm_captions = arm_captions;
        }
    }

    /// Assigns `value` to a place whose parts are already atoms.
    fn store(&mut self, place: Place, value: Operand) {
        let value_slot = match &place {
            Place::Variable(_) => 0,
            Place::Member(lash_kernel_doc::Member::Field { .. }) => 1,
            Place::Member(lash_kernel_doc::Member::Index { .. }) => 2,
        };
        let (rhs, note) = match self.take_binding_of(&value) {
            Some((rhs, mut note)) => {
                self.written = note.written.take();
                (rhs, note)
            }
            None => (Rhs::Expr(value.expr()), Note::default()),
        };
        let Note {
            blocks,
            nested_written,
            caption,
            arm_captions,
            ..
        } = note;
        self.push(
            Stmt::Assign {
                place,
                value: rhs,
                order: AssignOrder::ValueFirst,
            },
            blocks,
        );
        self.note_written();
        if let Some(note) = self.buf.last_note_mut() {
            note.caption = caption;
            note.arm_captions = arm_captions;
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
            let Note {
                blocks,
                nested_written,
                caption,
                arm_captions,
                ..
            } = note;
            match rhs {
                Rhs::Action(action) => self.push(
                    Stmt::Do {
                        value: action.into(),
                    },
                    blocks,
                ),
                Rhs::Expr(expr) => self.push(Stmt::Do { value: expr }, blocks),
            }
            if let Some(note) = self.buf.last_note_mut() {
                note.nested_written = nested_written;
                note.caption = caption;
                note.arm_captions = arm_captions;
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
        pinned.text_prefix = operand.text_prefix;
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
            later |= !buf.is_empty();
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
            text_prefix: None,
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
    Diagnostic::defect(
        DiagnosticCode::UnknownBinding,
        format!("`{name}` is not defined"),
        span,
    )
}
