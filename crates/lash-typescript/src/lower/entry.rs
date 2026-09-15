//! The lowerer's entry points.
//!
//! Each names one caller of the TypeScript front end — a whole cell, a cell in
//! a live session, a module with authority roots, or one editable
//! workflow-graph fragment — and they differ only in the ambient root scope
//! they install beneath the program's own.

use lashlang::{AssignTarget, Expr as LashExpr, Program as LashProgram};

use super::{Binding, BindingKind, BindingRole, GENERATED_BINDING_PREFIX, Lowerer, Scope};
use crate::Diagnostic;
use crate::adapter;

pub(crate) fn lower(program: &adapter::Program) -> Result<LashProgram, Diagnostic> {
    lower_with_context(
        program,
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
) -> Result<LashProgram, Diagnostic> {
    lower_with_context(
        program,
        ambient,
        process_handles,
        &std::collections::BTreeSet::new(),
    )
}

/// Lowers one editable workflow-graph fragment.
///
/// A fragment is a slice cut out of a program the lens already projected, so
/// the names it reads are the bindings live at that point rather than session
/// globals — and a statement that reassigned one of them still has to lower,
/// which a `const` ambient scope would refuse (FIG-3033).
pub(crate) fn lower_workflow_fragment(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    processes: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    lower_with_ambient_kind(
        program,
        ambient,
        &std::collections::BTreeSet::new(),
        &std::collections::BTreeSet::new(),
        BindingKind::Let,
        processes,
    )
}

pub(crate) fn lower_with_context(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
    module_authority_roots: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    lower_with_ambient_kind(
        program,
        ambient,
        process_handles,
        module_authority_roots,
        BindingKind::Const,
        &std::collections::BTreeSet::new(),
    )
}

fn lower_with_ambient_kind(
    program: &adapter::Program,
    ambient: &std::collections::BTreeSet<String>,
    process_handles: &std::collections::BTreeSet<String>,
    module_authority_roots: &std::collections::BTreeSet<String>,
    ambient_kind: BindingKind,
    ambient_processes: &std::collections::BTreeSet<String>,
) -> Result<LashProgram, Diagnostic> {
    let mut lowerer = Lowerer {
        root_scope_depth: 2,
        module_authority_roots: module_authority_roots.clone(),
        called_bindings: super::binding::called_binding_names(&program.statements),
        ..Lowerer::default()
    };
    let mut ambient_scope = Scope::default();
    for name in ambient.union(ambient_processes) {
        // The generated namespace is reserved and never durable, so a name
        // carrying it is not a session global this cell may read.
        if name.starts_with(GENERATED_BINDING_PREFIX) {
            continue;
        }
        ambient_scope.bindings.insert(
            name.clone(),
            Binding {
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
    let expressions = lowerer.lower_statements(&program.statements, true)?;
    let mut root_global_initializers = lowerer
        .intrinsic_global_slots
        .iter()
        .map(|name| LashExpr::Assign {
            target: AssignTarget::variable(name.as_str().into()),
            expr: Box::new(LashExpr::Undefined),
        })
        .collect::<Vec<_>>();
    root_global_initializers.extend(expressions);
    let main = LashExpr::Block(root_global_initializers);
    let expression_source_spans = super::spans::source_spans(&main, &lowerer.span_notes);
    Ok(LashProgram {
        declarations: lowerer.declarations,
        main,
        declaration_spans: lowerer.declaration_spans,
        // Left empty deliberately: this table is the linker's per-root-statement
        // fallback, and lowering only knows a statement's position when one of
        // its expressions carries a source span. A placeholder here would put a
        // caret on line 1 of a statement whose position is unknown, which is
        // worse than the message-only rendering the fallback already gives.
        expression_spans: Vec::new(),
        expression_source_spans,
    })
}
