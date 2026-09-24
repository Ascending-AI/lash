use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    AssignPathStep, AssignTarget, CatchClause, Declaration, Expr as LashExpr, FunctionExpr,
    JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, LabelMetadata, ProcessParam,
    ResourceRefExpr, StructuralRole, TryExpr, TypeExpr,
};

use crate::adapter::{
    self, ArrayElement, AssignOp, AssignTarget as TsAssignTarget, BinaryOp, CallArg, Expr,
    Function, FunctionBody, LogicalOp, MemberProperty, ObjectProperty, OptionalOperation, Pattern,
    PropertyKey, Stmt, UnaryOp, VarKind,
};
use crate::node_label::NodeLabel;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};
use spans::SpanMarkers;

mod stdlib;
use stdlib::*;
mod array_callbacks;
mod array_map;
mod attribute_update;
mod await_expr;
mod binding;
mod calls;
mod captures;
mod constructs;
mod entry;
mod graph;
mod json_replacer;
mod loops;
mod param_types;
mod process_wrapper;
mod regex;
mod spans;
mod spread_calls;
mod triggers;
pub(crate) use attribute_update::attribute_update;
use binding::*;
use captures::{CaptureLedger, Site};
use constructs::*;
pub(crate) use entry::{lower, lower_with_ambient, lower_with_context, lower_workflow_fragment};
use graph::{shortest_cycle_through, strongly_connected_components};
use json_replacer::reject_json_parse_reviver;
use param_types::process_param_type;
pub(crate) use process_wrapper::{process_run_wrapper, wrapped_run_body};
use triggers::{
    is_trigger_registration_operation, names_the_retired_trigger_event,
    retired_trigger_event_diagnostic,
};

pub(crate) fn accepts_instance_method(method: &str) -> bool {
    stdlib::is_instance_stdlib_method(method)
}

pub(crate) fn accepted_instance_methods() -> &'static [&'static str] {
    stdlib::instance_stdlib_methods()
}

/// Every binding the lowerer generates carries this prefix, which the dialect
/// reserves so a source identifier can never collide with one.
pub(crate) const GENERATED_BINDING_PREFIX: &str = "__typescript_";

/// A statement list closed by its completion value: `items`' last element is
/// the value the list evaluates to, and every other element is a statement.
pub(super) fn completion_list(items: Vec<LashExpr>) -> LashExpr {
    LashExpr::Role {
        role: StructuralRole::Completion,
        expr: Box::new(LashExpr::Block(items)),
    }
}

struct FunctionContext {
    id: usize,
    captures: BTreeSet<String>,
    /// Where the enclosing frame creates this closure: the point its captures
    /// are copied at.
    creation: Site,
}

/// A hoisted function declaration awaiting a place in the emission order.
struct PendingFunction {
    internal: String,
    captures: BTreeSet<String>,
    expression: LashExpr,
}

/// A binding whose assignment is emitted once every name it captures holds a
/// value.
struct PendingBinding {
    internal: String,
    captures: BTreeSet<String>,
    assignment: LashExpr,
}

#[derive(Default)]
struct PositionContext {
    loop_depth: usize,
    await_depth: usize,
    iterable_sink_depth: usize,
    /// The frame's loop statements enclosing the current position, outermost
    /// first, by ledger id. Unlike `loop_depth`, a loop's test and head count
    /// as inside it: they run again on every iteration.
    loops: Vec<usize>,
}

#[derive(Default)]
struct Lowerer {
    /// How deep the program's own root scope is. One when the lowerer stands
    /// alone; two when an ambient scope of live session globals sits beneath
    /// it. Depth is what "top level" means to a process literal, so it has to
    /// count from the program's root rather than from zero.
    root_scope_depth: usize,
    scopes: Vec<Scope>,
    functions: Vec<FunctionContext>,
    next_binding: usize,
    /// The bindings the lowered program marks private
    /// ([`lashlang::BindingVisibility::Private`]): every name this lowering
    /// invented, and every authored `let`/`const` (loop binders included)
    /// declared in a block of the cell's top level, which ECMA-262 ends with
    /// its block. Only the cell's own top-level bindings (and hoisted `var`s)
    /// are session-visible.
    private_bindings: BTreeSet<String>,
    next_function: usize,
    position: PositionContext,
    switch_breaks: Vec<(String, usize)>,
    continue_epilogues: Vec<Option<LashExpr>>,
    process_depth: usize,
    declarations: Vec<Declaration>,
    /// Private markers that move with sourced nodes until the finished
    /// program resolves them into root-qualified `AstPath` entries.
    span_markers: SpanMarkers,
    /// The span of the TypeScript expression currently being lowered, which is
    /// what a declaration emitted mid-lowering is positioned by.
    current_span: Option<SourceSpan>,
    module_authority_roots: BTreeSet<String>,
    allow_uninitialized_declaration_capture: bool,
    /// Where each closure copies its captures and where each binding is
    /// assigned, judged once the program has lowered.
    capture_ledger: CaptureLedger,
    /// The source-level names this program calls, computed once before
    /// lowering. A `const`-bound async arrow outside this set is a
    /// process-literal candidate (FIG-2997).
    called_bindings: BTreeSet<String>,
    /// Every name the program addresses as `globalThis.name`, computed once
    /// before lowering: a top-level block binding of one of these names takes
    /// a generated slot, so the bare slot stays the session global's.
    global_this_names: BTreeSet<String>,
    /// The session globals a cell boundary dropped for holding a function: a
    /// reference to one is refused by name (`TS_FUNCTION_NOT_PERSISTED`), not
    /// as a name nothing ever bound.
    expired_functions: BTreeSet<String>,
    /// Every name the program writes as `globalThis.name`: such a name is the
    /// program's own again, whatever the session dropped.
    global_this_writes: BTreeSet<String>,
    /// The name ECMA-262's NamedEvaluation/SetFunctionName positions assign to
    /// the next anonymous function lowered. Naming contexts set it just before
    /// lowering their value expression; `lower_function` takes it at entry so
    /// a nested function's own context can never consume it.
    inferred_function_name: Option<String>,
}

impl Lowerer {
    /// The refusal of a read of `name`, which no scope binds: by name when a
    /// cell boundary dropped it for holding a function.
    fn unknown_binding(&self, name: &str, span: Option<SourceSpan>) -> Diagnostic {
        if self.expired_functions.contains(name) {
            return Diagnostic::refusal(
                DiagnosticCode::FunctionNotPersisted,
                format!(
                    "`{name}` held a function, and a function does not persist across cells: its binding ended with the cell that defined it"
                ),
                span,
            );
        }
        if name == "await" {
            // Outside an async function a Script reads `await` as a name, so
            // `await (x)` there calls it; `tsc` answers the same (TS2311).
            return Diagnostic::with_repair(
                DiagnosticCode::UnknownBinding,
                "unknown binding `await`: outside an async function, `await` is an identifier",
                "declare the enclosing function `async` to await inside it",
                span,
            );
        }
        Diagnostic::new(
            DiagnosticCode::UnknownBinding,
            format!("unknown binding `{name}`"),
            span,
        )
    }

    /// Refuses a `globalThis` read of a function the session dropped, unless
    /// this program writes the name itself.
    fn refuse_expired_global_read(&self, name: &str) -> Result<(), Diagnostic> {
        if self.expired_functions.contains(name)
            && !self.global_this_writes.contains(name)
            && !self.has_binding(name)
        {
            return Err(self.unknown_binding(name, self.current_span));
        }
        Ok(())
    }

    fn current_function(&self) -> usize {
        self.functions.last().map_or(0, |function| function.id)
    }

    fn has_binding(&self, name: &str) -> bool {
        self.scopes
            .iter()
            .rev()
            .any(|scope| scope.bindings.contains_key(name))
    }

    fn lower_statements(
        &mut self,
        statements: &[Stmt],
        root: bool,
    ) -> Result<Vec<LashExpr>, Diagnostic> {
        if !root {
            self.scopes.push(Scope::default());
        }
        let hoisted_vars = if root {
            let mut hoisted = Vec::new();
            for name in function_var_names(statements) {
                let existing = self
                    .scopes
                    .last()
                    .and_then(|scope| scope.bindings.get(&name));
                if let Some(binding) = existing {
                    if !matches!(
                        binding.kind,
                        BindingKind::Var | BindingKind::Function | BindingKind::Parameter
                    ) {
                        return Err(Diagnostic::new(
                            DiagnosticCode::DuplicateBinding,
                            format!("var `{name}` conflicts with a lexical binding"),
                            None,
                        ));
                    }
                    continue;
                }
                self.declare(&name, BindingKind::Var, true, root)?;
                let internal = self.binding(&name)?.internal.clone();
                hoisted.push(LashExpr::Assign {
                    target: AssignTarget::variable(internal.into()),
                    expr: Box::new(LashExpr::Undefined),
                });
            }
            hoisted
        } else {
            Vec::new()
        };
        self.predeclare(statements, root)?;

        let local_function_internals = statements
            .iter()
            .filter_map(|statement| match statement.unlabeled() {
                Stmt::Function { name, .. } => {
                    Some(self.binding(name).map(|binding| binding.internal.clone()))
                }
                _ => None,
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let current_function = self.current_function();
        let mut available = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .filter(|binding| {
                // A binding owned by an enclosing function reaches this frame as
                // a capture or a parameter, so it already holds a value whenever
                // these statements run. Only bindings this frame declares itself
                // have to be ordered against the declarations that fill them.
                (binding.initialized || binding.owner_function != current_function)
                    && !local_function_internals.contains(&binding.internal)
            })
            .map(|binding| binding.internal.clone())
            .collect::<BTreeSet<_>>();
        let mut pending = Vec::with_capacity(local_function_internals.len());
        let previous_capture_mode = self.allow_uninitialized_declaration_capture;
        self.allow_uninitialized_declaration_capture = true;
        for statement in statements {
            if let Stmt::Function { name, function } = statement.unlabeled() {
                let binding = self.binding(name)?.clone();
                if function.is_async {
                    self.set_role(name, BindingRole::AsyncHelper)?;
                }
                let expression = self.lower_function(function, Some(binding.internal.clone()))?;
                let definition = match &expression {
                    LashExpr::Function(definition) => definition.as_ref(),
                    LashExpr::BuiltinCall { name, args } => {
                        let [LashExpr::Function(definition), ..] = args.as_slice() else {
                            unreachable!("closure intrinsic starts with a function literal")
                        };
                        debug_assert_eq!(name.as_str(), "__typescript_closure");
                        definition
                    }
                    _ => unreachable!("function lowering returns a function expression"),
                };
                pending.push(PendingFunction {
                    internal: binding.internal.clone(),
                    captures: definition
                        .captures
                        .iter()
                        .map(|capture| capture.as_str().to_string())
                        .collect(),
                    expression,
                });
            }
        }
        self.allow_uninitialized_declaration_capture = previous_capture_mode;
        reject_mutual_recursion(&pending, statements, self)?;
        let mut pending = pending
            .into_iter()
            .map(|function| PendingBinding {
                internal: function.internal.clone(),
                captures: function.captures,
                assignment: LashExpr::Assign {
                    target: AssignTarget::variable(function.internal.into()),
                    expr: Box::new(function.expression),
                },
            })
            .collect::<Vec<_>>();

        // A function declaration holds its value only once it is emitted,
        // which waits until every binding it captures holds one. Until then
        // a read of it is a read before initialization, refused by name like
        // any other, rather than of a name that has no assignment yet.
        for binding in &pending {
            self.set_local_initialized(&binding.internal, false);
        }
        let flush_ready = |pending: &mut Vec<PendingBinding>,
                           available: &mut BTreeSet<String>,
                           output: &mut Vec<LashExpr>| {
            let mut flushed = Vec::new();
            while let Some(index) = pending.iter().position(|binding| {
                binding
                    .captures
                    .iter()
                    .all(|capture| available.contains(capture))
            }) {
                let binding = pending.remove(index);
                available.insert(binding.internal.clone());
                flushed.push(binding.internal);
                output.push(binding.assignment);
            }
            flushed
        };
        let mut output = hoisted_vars;
        for statement in statements {
            for internal in flush_ready(&mut pending, &mut available, &mut output) {
                self.set_local_initialized(&internal, true);
            }
            match statement.unlabeled() {
                Stmt::Function { .. } => {}
                Stmt::Var { declarations, .. } => {
                    output.extend(self.lower_stmt(statement)?);
                    for declaration in declarations {
                        let mut names = Vec::new();
                        pattern_names(&declaration.pattern, &mut names);
                        for name in names {
                            available.insert(self.binding(&name)?.internal.clone());
                        }
                    }
                }
                _ => output.extend(self.lower_stmt(statement)?),
            }
        }
        for internal in flush_ready(&mut pending, &mut available, &mut output) {
            self.set_local_initialized(&internal, true);
        }
        if let Some(function) = pending.first() {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!(
                    "function `{}` captures a binding that is unavailable at declaration time",
                    function.internal
                ),
                None,
            ));
        }
        if !root {
            self.scopes.pop();
        }
        Ok(output)
    }

    fn predeclare(&mut self, statements: &[Stmt], root: bool) -> Result<(), Diagnostic> {
        for statement in statements {
            match statement.unlabeled() {
                Stmt::Var { kind, declarations } => {
                    if *kind == VarKind::Var {
                        continue;
                    }
                    for declaration in declarations {
                        let mut names = Vec::new();
                        pattern_names(&declaration.pattern, &mut names);
                        for name in names {
                            self.declare(
                                &name,
                                match kind {
                                    VarKind::Const => BindingKind::Const,
                                    VarKind::Let => BindingKind::Let,
                                    VarKind::Var => {
                                        unreachable!("var bindings are function-hoisted")
                                    }
                                },
                                *kind == VarKind::Var,
                                root,
                            )?;
                        }
                    }
                }
                Stmt::Function { name, .. } => {
                    self.declare(name, BindingKind::Function, true, root)?
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The label rides on the first lowered expression, which is the node the
    /// graph shows: a lifted process literal is named off its own hoisted
    /// declaration, not off the binding that starts it.
    fn apply_label(&mut self, label: &NodeLabel, mut lowered: Vec<LashExpr>) -> Vec<LashExpr> {
        let metadata = LabelMetadata {
            title: label.title.as_str().into(),
            description: label.description.as_deref().map(Into::into),
        };
        let Some(first) = lowered.first_mut() else {
            // A statement with nothing to run has no node to name.
            return lowered;
        };
        let expression = std::mem::replace(first, LashExpr::Undefined);
        *first = LashExpr::LabelAnnotated {
            label: metadata,
            expr: Box::new(expression),
        };
        lowered
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> Result<Vec<LashExpr>, Diagnostic> {
        Ok(match stmt {
            Stmt::Spanned(span, stmt) => self
                .lower_stmt(stmt)?
                .into_iter()
                .map(|expression| self.span_markers.annotate(*span, expression))
                .collect(),
            Stmt::Empty => Vec::new(),
            Stmt::Labeled { label, stmt } => {
                let lowered = self.lower_stmt(stmt)?;
                self.apply_label(label, lowered)
            }
            Stmt::Expr(expr) => vec![self.lower_expr(expr)?],
            Stmt::Block(statements) => vec![LashExpr::Role {
                role: StructuralRole::Scope,
                expr: Box::new(LashExpr::Block(self.lower_statements(statements, false)?)),
            }],
            Stmt::Var { kind, declarations } => {
                let mut output = Vec::with_capacity(declarations.len());
                for declaration in declarations {
                    if *kind == VarKind::Var && declaration.init.is_none() {
                        continue;
                    }
                    if *kind == VarKind::Const && declaration.init.is_none() {
                        let name = single_pattern_name(&declaration.pattern).unwrap_or("pattern");
                        return Err(Diagnostic::new(
                            DiagnosticCode::MissingInitializer,
                            format!("const `{name}` requires an initializer"),
                            None,
                        ));
                    }
                    let process_name = single_pattern_name(&declaration.pattern);
                    if let (Some(name), Some(Expr::Function(function))) =
                        (process_name, declaration.init.as_ref())
                        && function.is_async
                        && !self.called_bindings.contains(name)
                    {
                        self.set_role(name, BindingRole::AsyncHelper)?;
                    }
                    if let (Some(name), Some(Expr::New { constructor, .. })) =
                        (process_name, declaration.init.as_ref())
                        && let Some(kind) = IterableKind::from_constructor(constructor)
                    {
                        self.set_role(name, BindingRole::ExoticIterable(kind))?;
                    }
                    if let (Some(name), Some(Expr::Call { callee, .. })) =
                        (process_name, declaration.init.as_ref())
                        && let Expr::Member {
                            object,
                            property: MemberProperty::Field(method),
                            ..
                        } = callee.as_ref()
                    {
                        let kind = if matches!(object.as_ref(), Expr::Ident(owner, _) if owner == "Map")
                            && method == "groupBy"
                        {
                            Some(IterableKind::Map)
                        } else if matches!(
                            method.as_str(),
                            "union" | "intersection" | "difference" | "symmetricDifference"
                        ) {
                            Some(IterableKind::Set)
                        } else {
                            None
                        };
                        if let Some(kind) = kind {
                            self.set_role(name, BindingRole::ExoticIterable(kind))?;
                        }
                    }
                    let value = if let (Some(name), Some(Expr::Function(function))) =
                        (process_name, declaration.init.as_ref())
                        && *kind == VarKind::Const
                        && function.is_async
                        && !self.called_bindings.contains(name)
                    {
                        // A `const`-bound arrow the program never calls is an
                        // inline process body (FIG-2997): the linker lifts it
                        // where a `Process` slot asks for it.
                        self.lower_process_literal_arrow(function, Some(name))?
                    } else {
                        // A `let`/`const`/`var` initializer is ECMA's
                        // NamedEvaluation position: an anonymous function
                        // takes the binding's name.
                        declaration
                            .init
                            .as_ref()
                            .map(|expr| self.lower_named_expr(expr, process_name))
                            .transpose()?
                            .unwrap_or(LashExpr::Undefined)
                    };
                    let mut initialization =
                        self.lower_pattern(&declaration.pattern, value, PatternMode::Initialize)?;
                    // A `var` initializer assigns the binding its function
                    // hoisted, so it lowers as the assignment statement it is:
                    // the same program `x = value;` lowers to, which is also
                    // what the lens prints it as.
                    if *kind == VarKind::Var
                        && let [LashExpr::Assign { target, .. }] = initialization.as_slice()
                        && target.is_simple()
                    {
                        let result = LashExpr::Variable(target.root.clone());
                        initialization =
                            vec![completion_list(vec![initialization.remove(0), result])];
                    }
                    output.extend(initialization);
                }
                output
            }
            Stmt::Enum { name, members } => {
                let binding = self.binding(name)?;
                let (internal, binding_id) = (binding.internal.clone(), binding.id);
                // The first declaration of the enum assigns its object.
                self.record_write(binding_id);
                let variable = || LashExpr::Variable(internal.as_str().into());
                let mut output = vec![LashExpr::If {
                    condition: Box::new(variable()),
                    then_block: Box::new(variable()),
                    else_block: Box::new(LashExpr::Block(vec![
                        LashExpr::Assign {
                            target: AssignTarget::variable(internal.as_str().into()),
                            expr: Box::new(LashExpr::Record(Vec::new())),
                        },
                        variable(),
                    ])),
                }];
                for member in members {
                    output.push(LashExpr::Assign {
                        target: AssignTarget {
                            root: internal.as_str().into(),
                            steps: vec![AssignPathStep::Index(LashExpr::String(
                                member.name.as_str().into(),
                            ))],
                        },
                        expr: Box::new(self.lower_expr(&member.value)?),
                    });
                    if member.reverse {
                        output.push(LashExpr::Assign {
                            target: AssignTarget {
                                root: internal.as_str().into(),
                                steps: vec![AssignPathStep::Index(LashExpr::Index {
                                    target: Box::new(variable()),
                                    index: Box::new(LashExpr::String(member.name.as_str().into())),
                                })],
                            },
                            expr: Box::new(LashExpr::String(member.name.as_str().into())),
                        });
                    }
                }
                output
            }
            Stmt::Function { .. } => unreachable!("function declarations are hoisted"),
            Stmt::Return(value) => {
                if self.functions.is_empty() {
                    return Err(Diagnostic::new(
                        DiagnosticCode::ReturnOutsideFunction,
                        "return is only valid in a function",
                        None,
                    ));
                }
                vec![LashExpr::Return(Box::new(
                    value
                        .as_ref()
                        .map(|expr| self.lower_expr(expr))
                        .transpose()?
                        .unwrap_or(LashExpr::Undefined),
                ))]
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => vec![LashExpr::If {
                condition: Box::new(self.lower_expr(test)?),
                then_block: Box::new(self.lower_stmt_block(consequent)?),
                else_block: Box::new(
                    alternate
                        .as_deref()
                        .map(|stmt| self.lower_stmt_block(stmt))
                        .transpose()?
                        .unwrap_or_else(|| completion_list(vec![LashExpr::Undefined])),
                ),
            }],
            Stmt::While { test, body } => self.in_loop_statement(|lowerer| {
                let body = lowerer.with_loop(|lowerer| {
                    lowerer.continue_epilogues.push(None);
                    let body = lowerer.lower_stmt_block(body);
                    lowerer.continue_epilogues.pop();
                    body
                })?;
                Ok(vec![LashExpr::While {
                    condition: Box::new(lowerer.lower_expr(test)?),
                    body: Box::new(body),
                }])
            })?,
            Stmt::DoWhile {
                body,
                test,
                test_span,
            } => self.in_loop_statement(|lowerer| {
                let (epilogue, body) = lowerer.with_loop(|lowerer| {
                    let condition = lowerer.lower_expr(test)?;
                    let condition = lowerer.span_markers.annotate(*test_span, condition);
                    let epilogue = LashExpr::If {
                        condition: Box::new(js_unary(JavaScriptUnaryOp::Not, condition)),
                        then_block: Box::new(LashExpr::Break),
                        else_block: Box::new(LashExpr::Undefined),
                    };
                    lowerer.continue_epilogues.push(Some(epilogue.clone()));
                    let body = lowerer.lower_stmt_block(body);
                    lowerer.continue_epilogues.pop();
                    body.map(|body| (epilogue, body))
                })?;
                Ok(vec![LashExpr::While {
                    condition: Box::new(LashExpr::Bool(true)),
                    body: Box::new(LashExpr::Block(vec![body, epilogue])),
                }])
            })?,
            Stmt::For {
                init,
                test,
                update,
                body,
            } => vec![self.in_loop_statement(|lowerer| {
                lowerer.lower_classic_for(init.as_deref(), test.as_ref(), update.as_ref(), body)
            })?],
            Stmt::ForOf {
                pattern,
                kind,
                iterable,
                body,
            } => {
                // The loop follows its iterable live, as ECMA-262 does
                // (FIG-3625), so its body may change it freely.
                self.in_loop_statement(|lowerer| {
                    lowerer.lower_for_each(pattern, *kind, iterable, body, false)
                })?
            }
            Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => self.in_loop_statement(|lowerer| {
                lowerer.lower_for_each(pattern, *kind, object, body, true)
            })?,
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                vec![self.lower_switch(discriminant, cases)?]
            }
            Stmt::Break => {
                if let Some((flag, switch_loop_depth)) = self.switch_breaks.last()
                    && self.position.loop_depth == *switch_loop_depth
                {
                    vec![LashExpr::Assign {
                        target: AssignTarget::variable(flag.as_str().into()),
                        expr: Box::new(LashExpr::Bool(true)),
                    }]
                } else if self.position.loop_depth == 0 {
                    return Err(Diagnostic::new(
                        DiagnosticCode::LoopControlOutsideLoop,
                        "break is only valid in a loop or switch",
                        None,
                    ));
                } else {
                    vec![LashExpr::Break]
                }
            }
            Stmt::Continue => {
                if self.position.loop_depth == 0 {
                    return Err(Diagnostic::new(
                        DiagnosticCode::LoopControlOutsideLoop,
                        "continue is only valid in a loop",
                        None,
                    ));
                }
                vec![
                    self.continue_epilogues
                        .last()
                        .and_then(Clone::clone)
                        .map_or(LashExpr::Continue, |epilogue| {
                            LashExpr::Block(vec![epilogue, LashExpr::Continue])
                        }),
                ]
            }
            Stmt::Throw(value) => vec![LashExpr::Throw(Box::new(self.lower_expr(value)?))],
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                let body = LashExpr::Block(self.lower_statements(body, false)?);
                let catch = catch
                    .as_ref()
                    .map(|catch| {
                        self.scopes.push(Scope::default());
                        let mut prefix = Vec::new();
                        let exception = if let Some(pattern) = &catch.binding {
                            let mut names = Vec::new();
                            pattern_names(pattern, &mut names);
                            for name in names {
                                self.declare(&name, BindingKind::Catch, false, false)?;
                            }
                            if let Pattern::Ident(name, _) = pattern {
                                self.initialize(name);
                                self.binding(name)?.internal.clone()
                            } else {
                                let exception = self.temporary("caught");
                                prefix.extend(self.lower_pattern(
                                    pattern,
                                    LashExpr::Variable(exception.as_str().into()),
                                    PatternMode::Initialize,
                                )?);
                                exception
                            }
                        } else {
                            self.temporary("caught")
                        };
                        prefix.extend(self.lower_statements(&catch.body, false)?);
                        self.scopes.pop();
                        Ok(CatchClause {
                            binding: exception.into(),
                            body: Box::new(LashExpr::Block(prefix)),
                        })
                    })
                    .transpose()?;
                let finally = finally
                    .as_ref()
                    .map(|statements| {
                        self.lower_statements(statements, false)
                            .map(|body| Box::new(LashExpr::Block(body)))
                    })
                    .transpose()?;
                vec![LashExpr::Try(Box::new(TryExpr {
                    body: Box::new(body),
                    catch,
                    finally,
                }))]
            }
        })
    }

    /// Lowers a statement body (a branch or loop body) to one statement list
    /// closed by its completion value. A braced body is that list itself, not
    /// a nested scope inside it.
    fn lower_stmt_block(&mut self, stmt: &Stmt) -> Result<LashExpr, Diagnostic> {
        let (span, stmt) = match stmt {
            Stmt::Spanned(span, inner) if matches!(inner.as_ref(), Stmt::Block(_)) => {
                (Some(*span), inner.as_ref())
            }
            stmt => (None, stmt),
        };
        let mut expressions = match stmt {
            Stmt::Block(statements) => self.lower_statements(statements, false)?,
            _ => self.lower_stmt(stmt)?,
        };
        expressions.push(LashExpr::Undefined);
        let body = completion_list(expressions);
        Ok(match span {
            Some(span) => self.span_markers.annotate(span, body),
            None => body,
        })
    }

    fn lower_function(
        &mut self,
        function: &Function,
        internal_name: Option<String>,
    ) -> Result<LashExpr, Diagnostic> {
        // The closure is created here, in the enclosing frame: this is the
        // point its captures are copied at.
        let creation = self.capture_ledger.site(&self.position.loops);
        // The name a NamedEvaluation/SetFunctionName context left pending for
        // this function — consumed before the body lowers so a nested
        // function's own naming context cannot see it.
        let inferred_name = self.inferred_function_name.take();
        let outer_position = std::mem::take(&mut self.position);
        let outer_switch_breaks = std::mem::take(&mut self.switch_breaks);
        let outer_continue_epilogues = std::mem::take(&mut self.continue_epilogues);
        let result = self.lower_function_body(function, internal_name, inferred_name, creation);
        self.position = outer_position;
        self.switch_breaks = outer_switch_breaks;
        self.continue_epilogues = outer_continue_epilogues;
        result
    }

    fn lower_function_body(
        &mut self,
        function: &Function,
        internal_name: Option<String>,
        inferred_name: Option<String>,
        creation: Site,
    ) -> Result<LashExpr, Diagnostic> {
        self.next_function += 1;
        let id = self.next_function;
        self.functions.push(FunctionContext {
            id,
            captures: BTreeSet::new(),
            creation,
        });
        // ECMA binds a function's own name inside its body. A declaration
        // already owns an outer binding to reuse; a named function expression
        // needs a fresh one, visible only here, which the VM's self-slot fills.
        // The name lives in a scope of its own around the parameters and the
        // body (ECMA-262's funcEnv), so a body declaration of the same name
        // shadows it rather than duplicating it.
        self.scopes.push(Scope::default());
        let internal_name = match (&function.name, internal_name) {
            (Some(source_name), None) => {
                if is_reserved_name(source_name) {
                    self.scopes.pop();
                    self.functions.pop();
                    return Err(reserved_identifier(source_name));
                }
                Some(self.generated_binding(source_name))
            }
            (_, internal_name) => internal_name,
        };
        if let (Some(source_name), Some(internal)) = (&function.name, &internal_name) {
            let binding_id = self.declare_in_ledger(source_name, BindingKind::Function);
            #[expect(
                clippy::unwrap_used,
                reason = "the lowerer pushes the program root scope before any function and never pops past it"
            )]
            self.scopes.last_mut().unwrap().bindings.insert(
                source_name.clone(),
                Binding {
                    id: binding_id,
                    internal: internal.clone(),
                    kind: BindingKind::Function,
                    initialized: true,
                    owner_function: id,
                    // A function expression's own name has never carried the
                    // async-helper fact: recursion inside the body is a plain
                    // call, and this change does not alter that.
                    role: BindingRole::Plain,
                },
            );
        }
        self.scopes.push(Scope::default());
        let required_count = function
            .params
            .iter()
            .take_while(|param| !matches!(param, Pattern::Assign { .. } | Pattern::Rest(_)))
            .count();
        let accepts_rest = matches!(function.params.last(), Some(Pattern::Rest(_)));
        let has_defaults = function
            .params
            .iter()
            .any(|param| matches!(param, Pattern::Assign { .. }));
        if function
            .params
            .iter()
            .take(function.params.len().saturating_sub(1))
            .any(|param| matches!(param, Pattern::Rest(_)))
        {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "rest parameters must be last",
                None,
            ));
        }
        for pattern in &function.params {
            let mut names = Vec::new();
            pattern_names(pattern, &mut names);
            for name in names {
                self.declare(&name, BindingKind::Parameter, false, true)?;
            }
        }
        let mut params = Vec::with_capacity(function.params.len());
        let mut prologue = Vec::new();
        for pattern in &function.params {
            let target = match pattern {
                Pattern::Rest(target) => target.as_ref(),
                pattern => pattern,
            };
            let slot = if let Some(name) = single_pattern_name(target) {
                self.binding(name)?.internal.clone()
            } else {
                self.temporary("parameter")
            };
            params.push(slot.as_str().into());
            match pattern {
                Pattern::Ident(name, _) => self.initialize(name),
                Pattern::Rest(target) if matches!(target.as_ref(), Pattern::Ident(..)) => {
                    if let Pattern::Ident(name, _) = target.as_ref() {
                        self.initialize(name);
                    }
                }
                Pattern::Rest(target) => prologue.extend(self.lower_pattern(
                    target,
                    LashExpr::Variable(slot.as_str().into()),
                    PatternMode::Initialize,
                )?),
                pattern => prologue.extend(self.lower_pattern(
                    pattern,
                    LashExpr::Variable(slot.as_str().into()),
                    PatternMode::Initialize,
                )?),
            }
        }
        let tail = match &function.body {
            FunctionBody::Expression(expr) => {
                let lowered = LashExpr::Return(Box::new(self.lower_expr(expr)?));
                if let Some(span) = source_span(expr) {
                    self.span_markers.annotate(span, lowered)
                } else {
                    lowered
                }
            }
            FunctionBody::Block(statements) => {
                let mut body = std::mem::take(&mut prologue);
                body.extend(self.lower_statements(statements, true)?);
                body.push(LashExpr::Undefined);
                completion_list(body)
            }
        };
        let body = if prologue.is_empty() && matches!(tail, LashExpr::Role { .. }) {
            tail
        } else {
            prologue.push(tail);
            LashExpr::Block(prologue)
        };
        // The parameters' and body's scope, then the function name's.
        self.scopes.pop();
        self.scopes.pop();
        #[expect(
            clippy::expect_used,
            reason = "this is the matching pop for the function context pushed at the top of the same call"
        )]
        let context = self.functions.pop().expect("function context exists");
        let function = LashExpr::Function(Box::new(FunctionExpr {
            name: internal_name.map(Into::into),
            // The ECMA `name`: the function's own identifier when it has one,
            // else the name the enclosing NamedEvaluation/SetFunctionName
            // position assigned. `""` when no naming context applies.
            js_name: function.name.clone().or(inferred_name).map(Into::into),
            params,
            captures: context.captures.into_iter().map(Into::into).collect(),
            body: Box::new(body),
        }));
        if accepts_rest || has_defaults {
            Ok(LashExpr::BuiltinCall {
                name: "__typescript_closure".into(),
                args: vec![
                    function,
                    LashExpr::Number(required_count as f64),
                    LashExpr::Bool(accepts_rest),
                ],
            })
        } else {
            Ok(function)
        }
    }

    /// ECMA-262's IsAnonymousFunctionDefinition: only an anonymous function
    /// definition — a `function`/`async function` expression or (async) arrow
    /// with no own name — receives a name from a NamedEvaluation or
    /// SetFunctionName context.
    fn is_anonymous_function_definition(expr: &Expr) -> bool {
        matches!(expr, Expr::Function(function) if function.name.is_none())
    }

    /// Lowers `expr` in a position that assigns `name` to an anonymous
    /// function definition: NamedEvaluation (initializers, simple assigns,
    /// keyed properties) and the SetFunctionName step of binding defaults.
    /// The pending name is consumed by `lower_function` at entry, before its
    /// body lowers, so only the outermost anonymous function sees it.
    pub(super) fn lower_named_expr(
        &mut self,
        expr: &Expr,
        name: Option<&str>,
    ) -> Result<LashExpr, Diagnostic> {
        if name.is_some() && Self::is_anonymous_function_definition(expr) {
            self.inferred_function_name = name.map(str::to_string);
        }
        let lowered = self.lower_expr(expr);
        // The definition always consumes the pending name; clearing it keeps a
        // refused lowering from leaking one into an unrelated context.
        self.inferred_function_name = None;
        lowered
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<LashExpr, Diagnostic> {
        let Some(source) = source_span(expr) else {
            return self.lower_expr_inner(expr);
        };
        let enclosing = self.current_span.replace(source);
        let lowered = self.lower_expr_inner(expr);
        self.current_span = enclosing;
        let lowered = lowered?;
        Ok(self.span_markers.annotate(source, lowered))
    }

    fn lower_expr_inner(&mut self, expr: &Expr) -> Result<LashExpr, Diagnostic> {
        Ok(match expr {
            Expr::Undefined => LashExpr::Undefined,
            Expr::Null => LashExpr::Null,
            Expr::Bool(value) => LashExpr::Bool(*value),
            Expr::Number(value) => LashExpr::Number(*value),
            Expr::String(value) => LashExpr::String(value.as_str().into()),
            Expr::RegExp { pattern, flags } => LashExpr::BuiltinCall {
                name: "__typescript_heap_new".into(),
                args: vec![
                    LashExpr::String("RegExp".into()),
                    LashExpr::String(pattern.as_str().into()),
                    LashExpr::String(flags.as_str().into()),
                ],
            },
            Expr::Ident(name, _) if name == "undefined" && !self.has_binding(name) => {
                LashExpr::Undefined
            }
            Expr::Ident(name, _) if name == "NaN" && !self.has_binding(name) => {
                LashExpr::Number(f64::NAN)
            }
            Expr::Ident(name, _) if name == "Infinity" && !self.has_binding(name) => {
                LashExpr::Number(f64::INFINITY)
            }
            Expr::Ident(name, _)
                if matches!(name.as_str(), "String" | "Number" | "Boolean")
                    && !self.has_binding(name) =>
            {
                self.lower_conversion_function(name)
            }
            Expr::Ident(name, _) if name == "globalThis" && !self.has_binding(name) => {
                return Err(Diagnostic::refusal(
                    DiagnosticCode::UnsupportedExpression,
                    "Unsupported: bare globalThis. Use globalThis.identifier for durable session state.",
                    None,
                ));
            }
            Expr::Ident(name, _) if name == "arguments" && !self.has_binding(name) => {
                return Err(Diagnostic::new(
                    DiagnosticCode::ThisUnsupported,
                    "Unsupported: arguments. Declare an explicit ...rest parameter instead.",
                    None,
                ));
            }
            Expr::This if !self.functions.is_empty() => LashExpr::Undefined,
            Expr::This => {
                return Err(Diagnostic::new(
                    DiagnosticCode::ThisUnsupported,
                    "Unsupported: top-level this. Use explicit bindings; function this is undefined in the module dialect.",
                    None,
                ));
            }
            Expr::Ident(name, _) => LashExpr::Variable(self.resolve(name)?.into()),
            Expr::Array(items) => self.lower_array_literal(items)?,
            Expr::Object(entries) => self.lower_object_literal(entries)?,
            Expr::Assign { target, op, value } => self.lower_assignment(target, *op, value)?,
            Expr::Member {
                object, property, ..
            } => self.lower_member(object, property)?,
            Expr::Unary { op, value } => match op {
                UnaryOp::Void => {
                    LashExpr::Block(vec![self.lower_expr(value)?, LashExpr::Undefined])
                }
                UnaryOp::Plus => js_unary(JavaScriptUnaryOp::Plus, self.lower_expr(value)?),
                UnaryOp::Minus => js_unary(JavaScriptUnaryOp::Negate, self.lower_expr(value)?),
                UnaryOp::Not => js_unary(JavaScriptUnaryOp::Not, self.lower_expr(value)?),
                UnaryOp::BitNot => self.lower_bit_not(value)?,
                // `typeof` on a name nothing binds answers "undefined" without
                // resolving it — except the reserved value idents, which are
                // never unbound names: each lowers to a concrete literal below,
                // and `typeof` must classify that literal.
                UnaryOp::TypeOf
                    if matches!(value.as_ref(), Expr::Ident(name, _)
                        if !self.has_binding(name)
                            && !matches!(name.as_str(), "undefined" | "NaN" | "Infinity")) =>
                {
                    let Expr::Ident(name, _) = value.as_ref() else {
                        unreachable!("the guard matched an identifier")
                    };
                    if self.expired_functions.contains(name) {
                        return Err(self.unknown_binding(name, self.current_span));
                    }
                    LashExpr::String("undefined".into())
                }
                UnaryOp::TypeOf
                    if matches!(value.as_ref(), Expr::Ident(name, _) if self
                        .binding(name)
                        .is_ok_and(|binding| binding.role == BindingRole::GlobalProperty)) =>
                {
                    let Expr::Ident(name, _) = value.as_ref() else {
                        unreachable!("the guard matched an identifier")
                    };
                    js_unary(
                        JavaScriptUnaryOp::TypeOf,
                        LashExpr::BuiltinCall {
                            name: "__typescript_global_get".into(),
                            args: vec![LashExpr::String(name.as_str().into())],
                        },
                    )
                }
                UnaryOp::TypeOf => js_unary(JavaScriptUnaryOp::TypeOf, self.lower_expr(value)?),
            },
            Expr::Binary { left, op, right } => self.lower_binary_expr(left, *op, right)?,
            Expr::Logical { left, op, right } => LashExpr::JavaScriptLogical {
                left: Box::new(self.lower_expr(left)?),
                op: match op {
                    LogicalOp::And => JavaScriptLogicalOp::And,
                    LogicalOp::Or => JavaScriptLogicalOp::Or,
                    LogicalOp::Nullish => JavaScriptLogicalOp::NullishCoalesce,
                },
                right: Box::new(self.lower_expr(right)?),
            },
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => LashExpr::If {
                condition: Box::new(self.lower_expr(test)?),
                then_block: Box::new(self.lower_expr(consequent)?),
                else_block: Box::new(self.lower_expr(alternate)?),
            },
            Expr::Template {
                quasis,
                expressions,
            } => {
                let mut value = LashExpr::String(quasis.first().map_or("", String::as_str).into());
                for (index, expression) in expressions.iter().enumerate() {
                    value = js_add(value, self.lower_expr(expression)?);
                    value = js_add(
                        value,
                        LashExpr::String(quasis.get(index + 1).map_or("", String::as_str).into()),
                    );
                }
                value
            }
            Expr::Function(function) => self.lower_function(function, None)?,
            Expr::Call { callee, args, .. } => self.lower_call(callee, args)?,
            Expr::New { constructor, args } => self.lower_constructor(constructor, args)?,
            Expr::OptionalChain { base, operations } => {
                self.lower_optional_chain(base, operations)?
            }
            Expr::Await { value, .. } => self.lower_await(value)?,
            Expr::Update {
                target,
                delta,
                prefix,
            } => self.lower_update(target, *delta, *prefix)?,
            Expr::Delete { object, property } => self.lower_delete(object, property)?,
            Expr::LoneSurrogateString => {
                return Err(Diagnostic::new(
                    DiagnosticCode::LoneSurrogateLiteralUnsupported,
                    "string literals containing lone UTF-16 surrogates",
                    None,
                ));
            }
        })
    }

    /// Lowers an async arrow to a process literal.
    ///
    /// `name` is the `const` the arrow is bound to when there is one: the
    /// lifted declaration's own name is a digest, so the authored binding is
    /// the only spelling a diagnostic can show an author.
    fn lower_process_literal_arrow(
        &mut self,
        function: &Function,
        name: Option<&str>,
    ) -> Result<LashExpr, Diagnostic> {
        debug_assert!(function.is_async, "caller checked the arrow is async");
        // The lifted body becomes a process declaration, never a function
        // value: no ECMA `name` is observable on it, and carrying one would
        // perturb the content-derived identity the lift and the workflow
        // lens must agree on.
        let closure = self.with_process(|lowerer| lowerer.lower_function(function, None))?;
        let run = match &closure {
            LashExpr::Function(function) => function.as_ref(),
            LashExpr::BuiltinCall { args, .. } => {
                let [LashExpr::Function(function), ..] = args.as_slice() else {
                    unreachable!("closure intrinsic contains a function")
                };
                function
            }
            _ => unreachable!("run lowering returns a function"),
        };
        // FIG-2998: a lifted body may read immutable, durably representable
        // cell locals; each such read becomes a hidden start argument carrying
        // the value the variable had when the process started. Anything less
        // durable refuses naming the variable and the rewrite.
        let mut hidden_args = Vec::with_capacity(run.captures.len());
        for capture in &run.captures {
            let binding = self
                .binding_by_internal(capture)
                .ok_or_else(|| {
                    Diagnostic::defect(
                        DiagnosticCode::NonLiftableCapture,
                        format!("a process body references unknown binding `{capture}`"),
                        None,
                    )
                })?
                .clone();
            if let BindingRole::ProcessHandle = binding.role {
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::NonLiftableCapture,
                    format!(
                        "a process body cannot capture the process-handle binding `{}`: a process sees the value a variable had when it started, and a handle is not a durable value it may copy",
                        binding.internal
                    ),
                    "pass the handle into the process body through its `run` arguments",
                    None,
                ));
            }
            if !matches!(binding.kind, BindingKind::Const) {
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::NonLiftableCapture,
                    format!(
                        "a process body reads the mutable binding `{}`, and ADR 0011 forbids capturing a mutable name: bind the copy before the cell, or pass the value in as a start argument",
                        binding.internal
                    ),
                    "pass the value to the process through its `run` arguments",
                    None,
                ));
            }
            hidden_args.push(ProcessParam {
                name: binding.internal.as_str().into(),
                ty: TypeExpr::Any,
            });
        }
        let params = run
            .params
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let ty = match function.params.get(index) {
                    Some(Pattern::Ident(source_name, Some(annotation))) => {
                        process_param_type(name.unwrap_or("process"), source_name, annotation)?
                    }
                    _ => TypeExpr::Any,
                };
                Ok(ProcessParam {
                    name: slot.clone(),
                    ty,
                })
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;
        let call_args = run
            .params
            .iter()
            .map(|name| LashExpr::Variable(name.clone()))
            .collect();
        Ok(LashExpr::ProcessLiteral(Box::new(
            lashlang::ProcessLiteralExpr {
                params,
                hidden_args,
                body: Box::new(process_wrapper::process_run_wrapper(closure, call_args)),
            },
        )))
    }

    fn lower_assign_target(&mut self, target: &TsAssignTarget) -> Result<AssignTarget, Diagnostic> {
        match target {
            TsAssignTarget::Ident(name) => {
                let Some(binding) = self
                    .scopes
                    .iter()
                    .rev()
                    .find_map(|scope| scope.bindings.get(name))
                    .cloned()
                else {
                    return Err(self.unknown_binding(name, None));
                };
                if binding.kind != BindingKind::Let {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AssignConst,
                        format!("cannot assign to `{name}`"),
                        None,
                    ));
                }
                if binding.owner_function != self.current_function() {
                    return Err(Diagnostic::new(
                        DiagnosticCode::MutableCaptureUnsupported,
                        format!(
                            "mutable binding `{name}` cannot be captured until live lexical cells are available"
                        ),
                        None,
                    ));
                }
                self.record_write(binding.id);
                Ok(AssignTarget::variable(binding.internal.into()))
            }
            TsAssignTarget::Member { object, property } => {
                self.member_assign_target(object, property)
            }
            TsAssignTarget::Pattern(_) => Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "destructuring targets are lowered as a pattern, not a scalar assignment",
                None,
            )),
        }
    }

    fn member_assign_target(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<AssignTarget, Diagnostic> {
        let (root, mut steps) = self.member_path(object)?;
        steps.push(match property {
            MemberProperty::Field(field) => AssignPathStep::Field(field.as_str().into()),
            MemberProperty::Index(index) => AssignPathStep::Index(self.lower_expr(index)?),
        });
        Ok(AssignTarget {
            root: root.into(),
            steps,
        })
    }

    fn member_path(&mut self, expr: &Expr) -> Result<(String, Vec<AssignPathStep>), Diagnostic> {
        match expr {
            Expr::Ident(name, _) => Ok((self.resolve(name)?, Vec::new())),
            Expr::Member {
                object, property, ..
            } => {
                let (root, mut steps) = self.member_path(object)?;
                steps.push(match property {
                    MemberProperty::Field(field) => AssignPathStep::Field(field.as_str().into()),
                    MemberProperty::Index(index) => AssignPathStep::Index(self.lower_expr(index)?),
                });
                Ok((root, steps))
            }
            _ => Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "assignment target must start at a lexical binding",
                None,
            )),
        }
    }

    fn lower_member(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<LashExpr, Diagnostic> {
        if matches!(property, MemberProperty::Field(field) if field == "stack") {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: Error.stack is nondeterministic across engines. Inspect error.name and error.message instead.",
                None,
            ));
        }
        if matches!(object, Expr::Ident(name, _) if name == "globalThis" && !self.has_binding(name))
        {
            return match property {
                MemberProperty::Field(field)
                    if !matches!(field.as_str(), "undefined" | "NaN" | "Infinity") =>
                {
                    // The session slot, read live wherever the read runs: a
                    // function or closure reads the root frame's current
                    // value, as a global object property read does, never a
                    // copy and never a local of the same name.
                    self.refuse_global_this_in_process(field)?;
                    self.refuse_expired_global_read(field)?;
                    Ok(LashExpr::BuiltinCall {
                        name: "__typescript_global_get".into(),
                        args: vec![LashExpr::String(field.as_str().into())],
                    })
                }
                MemberProperty::Field(field) => Err(Diagnostic::new(
                    DiagnosticCode::ReservedIdentifier,
                    format!("globalThis.{field} is a reserved value identifier"),
                    None,
                )),
                MemberProperty::Index(_) => Err(Diagnostic::refusal(
                    DiagnosticCode::UnsupportedExpression,
                    "Unsupported: computed globalThis access. Use globalThis.identifier so session state remains statically named.",
                    None,
                )),
            };
        }
        if let Expr::Ident(owner, _) = object
            && is_known_runtime_global(owner)
            && !self.has_binding(owner)
        {
            let name = match property {
                MemberProperty::Field(field) => field.as_str(),
                MemberProperty::Index(_) => "computed property",
            };
            let constant = match (owner.as_str(), name) {
                ("Number", "EPSILON") => Some(f64::EPSILON),
                ("Number", "NaN") => Some(f64::NAN),
                ("Number", "MIN_SAFE_INTEGER") => Some(-9_007_199_254_740_991.0),
                ("Number", "MAX_SAFE_INTEGER") => Some(9_007_199_254_740_991.0),
                ("Number", "MAX_VALUE") => Some(f64::MAX),
                ("Number", "MIN_VALUE") => Some(f64::from_bits(1)),
                ("Number", "POSITIVE_INFINITY") => Some(f64::INFINITY),
                ("Number", "NEGATIVE_INFINITY") => Some(f64::NEG_INFINITY),
                ("Math", "PI") => Some(std::f64::consts::PI),
                ("Math", "E") => Some(std::f64::consts::E),
                ("Math", "LN2") => Some(std::f64::consts::LN_2),
                ("Math", "LN10") => Some(std::f64::consts::LN_10),
                ("Math", "LOG2E") => Some(std::f64::consts::LOG2_E),
                ("Math", "LOG10E") => Some(std::f64::consts::LOG10_E),
                ("Math", "SQRT2") => Some(std::f64::consts::SQRT_2),
                ("Math", "SQRT1_2") => Some(std::f64::consts::FRAC_1_SQRT_2),
                _ => None,
            };
            if let Some(value) = constant {
                return Ok(LashExpr::Number(value));
            }
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                format!("property `{owner}.{name}` is not in the TypeScript runtime surface"),
                None,
            ));
        }
        // The retired global, named and refused rather than left to reject as
        // an unknown binding, which said nothing about where the event went.
        if names_the_retired_trigger_event(object, property) && !self.has_binding("trigger") {
            return Err(retired_trigger_event_diagnostic());
        }
        let target = Box::new(self.lower_expr(object)?);
        Ok(match property {
            MemberProperty::Field(field) => LashExpr::Field {
                target,
                field: field.as_str().into(),
            },
            MemberProperty::Index(index) => LashExpr::Index {
                target,
                index: Box::new(self.lower_expr(index)?),
            },
        })
    }
}

/// v1 lowers a closure's captures by value, so a cycle of hoisted function
/// declarations has no emission order: each member needs its peers' values
/// before any of them exists. Routing the cycle through a shared mutable frame
/// record would work in memory but builds a heap cycle reachable from a durable
/// root, which the durable encoding rejects — the program would run and then
/// fail to persist. The dialect therefore rejects the shape up front and names
/// the cycle it found.
fn reject_mutual_recursion(
    pending: &[PendingFunction],
    statements: &[Stmt],
    lowerer: &Lowerer,
) -> Result<(), Diagnostic> {
    let index_by_internal = pending
        .iter()
        .enumerate()
        .map(|(index, function)| (function.internal.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let edges = pending
        .iter()
        .map(|function| {
            function
                .captures
                .iter()
                .filter_map(|capture| index_by_internal.get(capture.as_str()).copied())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let Some(component) = strongly_connected_components(&edges)
        .into_iter()
        .find(|component| component.len() > 1)
    else {
        return Ok(());
    };

    let source_names = statements
        .iter()
        .filter_map(|statement| match statement {
            Stmt::Function { name, .. } => lowerer
                .binding(name)
                .ok()
                .map(|binding| (binding.internal.clone(), name.clone())),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let display = |member: usize| {
        let internal = &pending[member].internal;
        source_names.get(internal).unwrap_or(internal).clone()
    };
    let cycle = shortest_cycle_through(&edges, component[0])
        .into_iter()
        .map(display)
        .collect::<Vec<_>>()
        .join(" -> ");
    Err(Diagnostic::with_repair(
        DiagnosticCode::MutualRecursionUnsupported,
        format!("mutually recursive function declarations are not supported in v1; cycle: {cycle}"),
        "restructure so one function calls the other, or drive the recursion with an explicit work list",
        None,
    ))
}

fn map_binary(op: BinaryOp) -> JavaScriptBinaryOp {
    match op {
        BinaryOp::Add => JavaScriptBinaryOp::Add,
        BinaryOp::Subtract => JavaScriptBinaryOp::Subtract,
        BinaryOp::Multiply => JavaScriptBinaryOp::Multiply,
        BinaryOp::Divide => JavaScriptBinaryOp::Divide,
        BinaryOp::Remainder => JavaScriptBinaryOp::Remainder,
        BinaryOp::StrictEqual => JavaScriptBinaryOp::StrictEqual,
        BinaryOp::StrictNotEqual => JavaScriptBinaryOp::StrictNotEqual,
        BinaryOp::LooseEqual => JavaScriptBinaryOp::LooseEqual,
        BinaryOp::LooseNotEqual => JavaScriptBinaryOp::LooseNotEqual,
        BinaryOp::Less => JavaScriptBinaryOp::Less,
        BinaryOp::LessEqual => JavaScriptBinaryOp::LessEqual,
        BinaryOp::Greater => JavaScriptBinaryOp::Greater,
        BinaryOp::GreaterEqual => JavaScriptBinaryOp::GreaterEqual,
        BinaryOp::Exponent
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::ShiftLeft
        | BinaryOp::ShiftRight
        | BinaryOp::ShiftRightUnsigned
        | BinaryOp::In
        | BinaryOp::InstanceOf => unreachable!("operator has a dedicated lowering"),
    }
}

/// The source position a lowered TypeScript expression is reported at.
///
/// Only the forms a diagnostic points at carry one: a call, a member access
/// and an await. Everything else inherits the nearest enclosing span the
/// linker is already carrying, which is what the lashlang parser's tables did
/// for a sub-expression it recorded no span for.
fn source_span(expr: &Expr) -> Option<SourceSpan> {
    match expr {
        Expr::Call { span, .. } | Expr::Member { span, .. } | Expr::Await { span, .. } => {
            Some(*span)
        }
        Expr::Ident(_, span) => *span,
        _ => None,
    }
}
