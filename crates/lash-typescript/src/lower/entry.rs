//! The lowerer's entry points.
//!
//! Each names one caller of the TypeScript front end — a whole cell, a cell in
//! a live session, a module with authority roots, or one editable
//! workflow-graph fragment — and they differ only in the ambient root scope
//! they install beneath the program's own.

use lashlang::{Expr as LashExpr, Program as LashProgram};

use super::{Binding, BindingKind, BindingRole, Lowerer, Scope};
use crate::adapter;
use crate::{Diagnostic, DiagnosticCode};

pub(crate) fn lower(program: &adapter::Program) -> Result<LashProgram, Diagnostic> {
    lower_with_context(
        program,
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
    )
}

/// Lowers `program` with `ambient` names already in scope.
///
/// The RLM session model is that top-level bindings persist across cells: cell
/// A writes `const findings = ...`, and cell B reads `findings` while the
/// prompt lists it under `=== BOUND VARIABLES ===` with its value. Lashlang
/// parses permissively and resolves those names at *link*, where the live
/// session globals are known. This lowerer resolves every name at parse against
/// source-local scopes, so cell B rejected with `TS_UNKNOWN_BINDING` for a name
/// the session was showing it — which breaks every stateful multi-cell
/// TypeScript session.
///
/// The names arrive as an ambient root scope beneath the program's own: they
/// are initialized (no temporal dead zone), immutable (a bare `findings = 1`
/// with no declaration is still refused, and a capture of one is legal), and
/// they never mangle, because a root declaration of the same name keeps the
/// author's spelling — which is how a cell rebinds a session global.
///
/// A name in neither the source nor the session is still `TS_UNKNOWN_BINDING`
/// at parse. That distinction is the whole contract: "unknown everywhere"
/// stays an error, "known to the session" does not.
pub(crate) fn lower_with_ambient(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
    expired_functions: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    lower_with_context(
        program,
        ambient,
        process_handles,
        &std::collections::BTreeSet::new(),
        expired_functions,
    )
}

/// Lowers one editable workflow-graph fragment.
///
/// A fragment is a slice cut out of a program the lens already projected, so
/// the names it reads are the bindings live at that point rather than session
/// globals — and a statement that reassigned one of them still has to lower,
/// which a `const` ambient scope would refuse (FIG-3033). `session_globals`
/// are the names the program itself linked against: readable as a cell's own
/// globals are, but immutable, and a live binding of the same name wins.
pub(crate) fn lower_workflow_fragment(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    session_globals: &std::collections::BTreeSet<String>,
    processes: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    lower_with_ambient_kind(
        program,
        ambient,
        session_globals,
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
        BindingKind::Let,
        processes,
        &std::collections::BTreeSet::new(),
    )
}

pub(crate) fn lower_with_context(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
    module_authority_roots: &std::collections::BTreeSet<String>,
    expired_functions: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    lower_with_ambient_kind(
        program,
        ambient,
        &std::collections::BTreeSet::new(),
        process_handles,
        module_authority_roots,
        BindingKind::Const,
        &std::collections::BTreeSet::new(),
        expired_functions,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "each set is one caller's ambient fact"
)]
fn lower_with_ambient_kind(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    session_globals: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
    module_authority_roots: &std::collections::BTreeSet<String>,
    ambient_kind: BindingKind,
    ambient_processes: &std::collections::BTreeSet<String>,
    expired_functions: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    let pass = |cells| {
        lower_pass(
            program,
            ambient,
            session_globals,
            process_handles,
            module_authority_roots,
            ambient_kind,
            ambient_processes,
            expired_functions,
            cells,
        )
    };
    // The first pass judges which slots a closure shares with an assignment
    // (FIG-3707). Only a program with such a slot lowers again, knowing them,
    // so every declaration of one mints its cell.
    let (lowerer, main) = pass(std::collections::BTreeSet::new())?;
    let cells = lowerer.capture_ledger.cell_slots();
    if cells.is_empty() {
        return Ok(finish(lowerer, main));
    }
    let (mut lowerer, mut main) = pass(cells)?;
    // The boxing pass lowers the same program, so it must judge the same
    // slots; a disagreement would leave a slot boxed on one path only.
    if lowerer.capture_ledger.cell_slots() != lowerer.cells {
        return Err(Diagnostic::defect(
            DiagnosticCode::UnsupportedExpression,
            "the lowering passes disagree on which captured bindings share a binding cell",
            None,
        ));
    }
    let session_slots = lowerer
        .cells
        .iter()
        .filter(|(owner, internal)| *owner == 0 && !lowerer.private_bindings.contains(internal))
        .map(|(_, internal)| internal.clone())
        .collect();
    super::cells::box_captured_bindings(&mut main, &session_slots, &mut lowerer);
    Ok(finish(lowerer, main))
}

#[expect(
    clippy::too_many_arguments,
    reason = "each set is one caller's ambient fact"
)]
fn lower_pass(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    session_globals: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
    module_authority_roots: &std::collections::BTreeSet<String>,
    ambient_kind: BindingKind,
    ambient_processes: &std::collections::BTreeSet<String>,
    expired_functions: &std::collections::BTreeSet<String>,
    cells: std::collections::BTreeSet<super::captures::SlotKey>,
) -> Result<(Lowerer, LashExpr), Diagnostic> {
    let mut lowerer = Lowerer {
        root_scope_depth: 2,
        module_authority_roots: module_authority_roots.clone(),
        called_bindings: super::binding::called_binding_names(&program.statements),
        global_this_names: super::binding::global_this_names(&program.statements),
        global_this_writes: super::binding::global_this_writes(&program.statements),
        expired_functions: expired_functions.clone(),
        cells,
        ..Lowerer::default()
    };
    let mut ambient_scope = Scope::default();
    // Session globals go in first, immutable: a fragment reads one exactly as
    // the cell's link bound it, and the fragment's own ambient names still
    // shadow them.
    for name in session_globals {
        let id = lowerer
            .capture_ledger
            .declare((0, name.clone()), Vec::new());
        ambient_scope.bindings.insert(
            name.clone(),
            Binding {
                id,
                internal: name.clone(),
                kind: BindingKind::Const,
                initialized: true,
                owner_function: 0,
                role: BindingRole::Plain,
            },
        );
    }
    for name in ambient.union(ambient_processes) {
        let id = lowerer
            .capture_ledger
            .declare((0, name.clone()), Vec::new());
        ambient_scope.bindings.insert(
            name.clone(),
            Binding {
                id,
                internal: name.clone(),
                kind: ambient_kind,
                initialized: true,
                owner_function: 0,
                role: if process_handles.contains(name) {
                    BindingRole::ProcessHandle
                } else {
                    BindingRole::Plain
                },
            },
        );
    }
    lowerer.scopes.push(ambient_scope);
    lowerer.scopes.push(Scope::default());
    // A bare write to an advertised built-in (`Object = x`, `Math ??= y`,
    // `for (JSON of xs)`, `{ n: Number } = now`) lands on the global
    // property the name spells: the cell binds it as a session slot ahead
    // of lowering and, where the session holds no value for it yet, seeds
    // the built-in object the name answered before any write — so a read
    // anywhere in the program, inside a function included, sees ECMA's
    // value. A `let`/`const`/`function`/`enum` of the same name at the top
    // level shadows the property and keeps its own binding; a `var`
    // deliberately binds through the property itself.
    let root_declared = super::binding::root_declaration_names(&program.statements);
    let mut seeds = Vec::new();
    for name in super::binding::builtin_global_writes(&program.statements) {
        if !root_declared.contains(&name) && lowerer.ensure_global_binding(&name)? {
            seeds.push(name);
        }
    }
    let mut expressions = seeds
        .into_iter()
        .map(|name| LashExpr::If {
            condition: Box::new(super::constructs::js_unary(
                lashlang::JavaScriptUnaryOp::Not,
                LashExpr::BuiltinCall {
                    name: "__typescript_global_has".into(),
                    args: vec![LashExpr::String(name.clone().into())],
                },
            )),
            then_block: Box::new(LashExpr::Assign {
                target: lashlang::AssignTarget::variable(name.as_str().into()),
                expr: Box::new(Lowerer::stdlib_call(
                    "Lash.Builtin",
                    vec![LashExpr::String(name.into())],
                )),
            }),
            else_block: Box::new(LashExpr::Undefined),
        })
        .collect::<Vec<_>>();
    expressions.extend(lowerer.lower_statements(&program.statements, true)?);
    Ok((lowerer, LashExpr::Block(expressions)))
}

fn finish(mut lowerer: Lowerer, main: LashExpr) -> LashProgram {
    let mut program = LashProgram {
        language: lashlang::SourceLanguage::new(crate::TYPESCRIPT_LANGUAGE),
        declarations: std::mem::take(&mut lowerer.declarations),
        main,
        private_bindings: std::mem::take(&mut lowerer.private_bindings)
            .into_iter()
            .map(Into::into)
            .collect(),
        spans: Default::default(),
    };
    lowerer.span_markers.resolve(&mut program);
    program
}
