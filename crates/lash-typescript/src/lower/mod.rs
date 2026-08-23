use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    AssignPathStep, AssignTarget, CatchClause, Declaration, Expr as LashExpr, FunctionExpr,
    JavaScriptBinaryOp, JavaScriptLogicalOp, JavaScriptUnaryOp, ProcessDecl, ProcessParam,
    ProcessSignalDecl, ProcessStartExpr, Program as LashProgram, ResourceRefExpr, TryExpr,
    TypeExpr,
};

use crate::adapter::{
    self, ArrayElement, AssignOp, AssignTarget as TsAssignTarget, BinaryOp, CallArg, Expr,
    Function, FunctionBody, LogicalOp, MemberProperty, ObjectProperty, OptionalOperation, Pattern,
    PropertyKey, Stmt, UnaryOp, VarKind,
};
use crate::{Diagnostic, DiagnosticCode};

mod stdlib;
use stdlib::*;
mod loops;
use loops::*;
mod array_callbacks;
mod array_map;
mod await_expr;
mod binding;
mod calls;
mod constructs;
mod graph;
mod json_replacer;
mod regex;
use binding::*;
use constructs::*;
use graph::{shortest_cycle_through, strongly_connected_components};
use json_replacer::reject_json_parse_reviver;

pub(crate) fn accepts_instance_method(method: &str) -> bool {
    stdlib::is_instance_stdlib_method(method)
}

pub(crate) fn accepted_instance_methods() -> &'static [&'static str] {
    stdlib::instance_stdlib_methods()
}

/// Every binding the lowerer generates carries this prefix, which the dialect
/// reserves so a source identifier can never collide with one.
pub(crate) const GENERATED_BINDING_PREFIX: &str = "__typescript_";

pub(crate) fn lower(program: &adapter::Program) -> Result<LashProgram, Diagnostic> {
    lower_with_ambient(program, &std::collections::BTreeSet::new())
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
) -> Result<LashProgram, Diagnostic> {
    let mut lowerer = Lowerer {
        root_scope_depth: 2,
        ..Lowerer::default()
    };
    let mut ambient_scope = Scope::default();
    for name in ambient {
        // The generated namespace is reserved and never durable, so a name
        // carrying it is not a session global this cell may read.
        if name.starts_with(GENERATED_BINDING_PREFIX) {
            continue;
        }
        ambient_scope.bindings.insert(
            name.clone(),
            Binding {
                internal: name.clone(),
                kind: BindingKind::Const,
                initialized: true,
                owner_function: 0,
                role: BindingRole::Plain,
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
    Ok(LashProgram {
        declarations: lowerer.declarations,
        main: LashExpr::Block(root_global_initializers),
        declaration_spans: Vec::new(),
        expression_spans: Vec::new(),
        expression_source_spans: Vec::new(),
    })
}

#[derive(Default)]
struct FunctionContext {
    id: usize,
    captures: BTreeSet<String>,
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
}

#[derive(Default)]
struct Lowerer {
    /// How deep the program's own root scope is. One when the lowerer stands
    /// alone; two when an ambient scope of live session globals sits beneath
    /// it. Depth is what "top level" means to `defineProcess`, so it has to
    /// count from the program's root rather than from zero.
    root_scope_depth: usize,
    scopes: Vec<Scope>,
    functions: Vec<FunctionContext>,
    next_binding: usize,
    next_function: usize,
    position: PositionContext,
    switch_breaks: Vec<(String, usize)>,
    continue_epilogues: Vec<Option<LashExpr>>,
    process_depth: usize,
    declarations: Vec<Declaration>,
    intrinsic_global_slots: BTreeSet<String>,
    allow_uninitialized_declaration_capture: bool,
}

impl Lowerer {
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
            .filter_map(|statement| match statement {
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
            if let Stmt::Function { name, function } = statement {
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

        let flush_ready = |pending: &mut Vec<PendingBinding>,
                           available: &mut BTreeSet<String>,
                           output: &mut Vec<LashExpr>| {
            while let Some(index) = pending.iter().position(|binding| {
                binding
                    .captures
                    .iter()
                    .all(|capture| available.contains(capture))
            }) {
                let binding = pending.remove(index);
                available.insert(binding.internal);
                output.push(binding.assignment);
            }
        };
        let mut output = hoisted_vars;
        for statement in statements {
            flush_ready(&mut pending, &mut available, &mut output);
            match statement {
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
        flush_ready(&mut pending, &mut available, &mut output);
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
            match statement {
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

    fn declare(
        &mut self,
        name: &str,
        kind: BindingKind,
        initialized: bool,
        preserve_name: bool,
    ) -> Result<(), Diagnostic> {
        if matches!(name, "undefined" | "NaN" | "Infinity") {
            return Err(Diagnostic::new(
                DiagnosticCode::ReservedIdentifier,
                format!(
                    "`{name}` is a reserved TypeScript value identifier and cannot be shadowed"
                ),
                None,
            ));
        }
        if name.starts_with(GENERATED_BINDING_PREFIX) {
            return Err(reserved_identifier(name));
        }
        // Mangling exists to stop an inner scope from overwriting an outer slot
        // of the same name. Where nothing of that name is visible there is
        // nothing to protect, and a mangled root-level binding would publish a
        // generated name into the durable globals and the bound-variables
        // prompt, so keep the author's name in that case.
        let preserve_name = preserve_name || !self.has_binding(name);
        let owner_function = self.current_function();
        let scope = self.scopes.last_mut().expect("a scope is always active");
        if scope.bindings.contains_key(name) {
            return Err(Diagnostic::new(
                DiagnosticCode::DuplicateBinding,
                format!("duplicate lexical binding `{name}`"),
                None,
            ));
        }
        let internal = if preserve_name {
            name.to_string()
        } else {
            let id = self.next_binding;
            self.next_binding += 1;
            format!("{GENERATED_BINDING_PREFIX}{id}_{name}")
        };
        scope.bindings.insert(
            name.to_string(),
            Binding {
                internal,
                kind,
                initialized,
                owner_function,
                role: BindingRole::Plain,
            },
        );
        Ok(())
    }

    /// Records what the binding `name` resolves to *is*.
    ///
    /// The role is learned from the initializer, so it is always set after the
    /// declaration that a lexical scope hoists — the same resolution `binding`
    /// performs, against the same scope stack, so the fact lands on the
    /// binding the reads will find and dies when its scope pops.
    fn set_role(&mut self, name: &str, role: BindingRole) -> Result<(), Diagnostic> {
        let binding = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.bindings.get_mut(name))
            .ok_or_else(|| {
                Diagnostic::new(
                    DiagnosticCode::UnknownBinding,
                    format!("unknown binding `{name}`"),
                    None,
                )
            })?;
        binding.role = role;
        Ok(())
    }

    fn binding(&self, name: &str) -> Result<&Binding, Diagnostic> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .ok_or_else(|| {
                Diagnostic::new(
                    DiagnosticCode::UnknownBinding,
                    format!("unknown binding `{name}`"),
                    None,
                )
            })
    }

    fn resolve(&mut self, name: &str) -> Result<String, Diagnostic> {
        let Some(binding) = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .cloned()
        else {
            return Err(Diagnostic::new(
                DiagnosticCode::UnknownBinding,
                format!("unknown binding `{name}`"),
                None,
            ));
        };
        let current_function = self.current_function();
        if current_function == binding.owner_function && !binding.initialized {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!("`{name}` is read before initialization"),
                None,
            ));
        }
        if current_function != binding.owner_function {
            if !binding.initialized && !self.allow_uninitialized_declaration_capture {
                return Err(Diagnostic::new(
                    DiagnosticCode::TemporalDeadZone,
                    format!(
                        "captured binding `{name}` is not initialized when the closure is created"
                    ),
                    None,
                ));
            }
            let first_capturing_function = self
                .functions
                .iter()
                .position(|function| function.id == binding.owner_function)
                .map_or(0, |owner| owner + 1);
            for function in &mut self.functions[first_capturing_function..] {
                function.captures.insert(binding.internal.clone());
            }
        }
        Ok(binding.internal)
    }

    fn initialize(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.get_mut(name) {
                binding.initialized = true;
                return;
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> Result<Vec<LashExpr>, Diagnostic> {
        Ok(match stmt {
            Stmt::Empty => Vec::new(),
            Stmt::Expr(expr) => vec![self.lower_expr(expr)?],
            Stmt::Block(statements) => {
                vec![LashExpr::Block(self.lower_statements(statements, false)?)]
            }
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
                        } = callee.as_ref()
                    {
                        let kind = if matches!(object.as_ref(), Expr::Ident(owner) if owner == "Map")
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
                    let value = if let Some(init) = declaration.init.as_ref()
                        && is_define_process_call(init)
                    {
                        if *kind != VarKind::Const {
                            return Err(Diagnostic::new(
                                DiagnosticCode::ProcessDefinitionNotTopLevel,
                                "defineProcess must initialize a top-level const binding",
                                None,
                            ));
                        }
                        // The program's own root scope, which sits directly
                        // above the ambient session scope.
                        if self.scopes.len() != self.root_scope_depth || !self.functions.is_empty()
                        {
                            return Err(Diagnostic::new(
                                DiagnosticCode::ProcessDefinitionNotTopLevel,
                                "defineProcess must initialize a top-level binding",
                                None,
                            ));
                        }
                        let Some(process_name) = process_name else {
                            return Err(Diagnostic::new(
                                DiagnosticCode::ProcessDefinitionNotTopLevel,
                                "defineProcess must initialize one identifier binding",
                                None,
                            ));
                        };
                        self.lower_process_definition(process_name, init)?
                    } else {
                        declaration
                            .init
                            .as_ref()
                            .map(|expr| self.lower_expr(expr))
                            .transpose()?
                            .unwrap_or(LashExpr::Undefined)
                    };
                    if let Some(name) = process_name
                        && *kind == VarKind::Const
                        && matches!(&value, LashExpr::StartProcess(_))
                    {
                        self.set_role(name, BindingRole::ProcessHandle)?;
                    }
                    output.extend(self.lower_pattern(
                        &declaration.pattern,
                        value,
                        PatternMode::Initialize,
                    )?);
                }
                output
            }
            Stmt::Enum { name, members } => {
                let internal = self.binding(name)?.internal.clone();
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
                        .unwrap_or(LashExpr::Undefined),
                ),
            }],
            Stmt::While { test, body } => {
                let body = self.with_loop(|lowerer| {
                    lowerer.continue_epilogues.push(None);
                    let body = lowerer.lower_stmt_block(body);
                    lowerer.continue_epilogues.pop();
                    body
                })?;
                vec![LashExpr::While {
                    condition: Box::new(self.lower_expr(test)?),
                    body: Box::new(body),
                }]
            }
            Stmt::DoWhile { body, test } => {
                let (epilogue, body) = self.with_loop(|lowerer| {
                    let epilogue = LashExpr::If {
                        condition: Box::new(js_unary(
                            JavaScriptUnaryOp::Not,
                            lowerer.lower_expr(test)?,
                        )),
                        then_block: Box::new(LashExpr::Break),
                        else_block: Box::new(LashExpr::Undefined),
                    };
                    lowerer.continue_epilogues.push(Some(epilogue.clone()));
                    let body = lowerer.lower_stmt_block(body);
                    lowerer.continue_epilogues.pop();
                    body.map(|body| (epilogue, body))
                })?;
                vec![LashExpr::While {
                    condition: Box::new(LashExpr::Bool(true)),
                    body: Box::new(LashExpr::Block(vec![body, epilogue])),
                }]
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => vec![self.lower_classic_for(
                init.as_deref(),
                test.as_ref(),
                update.as_ref(),
                body,
            )?],
            Stmt::ForOf {
                pattern,
                kind,
                iterable,
                body,
            } => {
                if let Some(reason) = body_may_mutate_iterable(iterable, body) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::ForOfUnsupported,
                        format!(
                            "this for-of body {reason}; the v1 iterator walks a snapshot, so mutating the iterable mid-loop is not supported"
                        ),
                        None,
                    ));
                }
                self.lower_for_each(pattern, *kind, iterable, body, false)?
            }
            Stmt::ForIn {
                pattern,
                kind,
                object,
                body,
            } => self.lower_for_each(pattern, *kind, object, body, true)?,
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
                            if let Pattern::Ident(name) = pattern {
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

    fn lower_stmt_block(&mut self, stmt: &Stmt) -> Result<LashExpr, Diagnostic> {
        let mut expressions = match stmt {
            Stmt::Block(statements) => self.lower_statements(statements, false)?,
            _ => self.lower_stmt(stmt)?,
        };
        expressions.push(LashExpr::Undefined);
        Ok(LashExpr::Block(expressions))
    }

    fn lower_function(
        &mut self,
        function: &Function,
        internal_name: Option<String>,
    ) -> Result<LashExpr, Diagnostic> {
        let outer_position = std::mem::take(&mut self.position);
        let outer_switch_breaks = std::mem::take(&mut self.switch_breaks);
        let outer_continue_epilogues = std::mem::take(&mut self.continue_epilogues);
        let result = self.lower_function_body(function, internal_name);
        self.position = outer_position;
        self.switch_breaks = outer_switch_breaks;
        self.continue_epilogues = outer_continue_epilogues;
        result
    }

    fn lower_function_body(
        &mut self,
        function: &Function,
        internal_name: Option<String>,
    ) -> Result<LashExpr, Diagnostic> {
        self.next_function += 1;
        let id = self.next_function;
        self.functions.push(FunctionContext {
            id,
            ..FunctionContext::default()
        });
        self.scopes.push(Scope::default());
        // ECMA binds a function's own name inside its body. A declaration
        // already owns an outer binding to reuse; a named function expression
        // needs a fresh one, visible only here, which the VM's self-slot fills.
        let internal_name = match (&function.name, internal_name) {
            (Some(source_name), None) => {
                if source_name.starts_with(GENERATED_BINDING_PREFIX) {
                    self.scopes.pop();
                    self.functions.pop();
                    return Err(reserved_identifier(source_name));
                }
                let generated = self.next_binding;
                self.next_binding += 1;
                Some(format!(
                    "{GENERATED_BINDING_PREFIX}{generated}_{source_name}"
                ))
            }
            (_, internal_name) => internal_name,
        };
        if let (Some(source_name), Some(internal)) = (&function.name, &internal_name) {
            self.scopes.last_mut().unwrap().bindings.insert(
                source_name.clone(),
                Binding {
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
                Pattern::Ident(name) => self.initialize(name),
                Pattern::Rest(target) if matches!(target.as_ref(), Pattern::Ident(_)) => {
                    if let Pattern::Ident(name) = target.as_ref() {
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
            FunctionBody::Expression(expr) => LashExpr::Return(Box::new(self.lower_expr(expr)?)),
            FunctionBody::Block(statements) => {
                let mut body = self.lower_statements(statements, true)?;
                body.push(LashExpr::Undefined);
                LashExpr::Block(body)
            }
        };
        prologue.push(tail);
        let body = LashExpr::Block(prologue);
        self.scopes.pop();
        let context = self.functions.pop().expect("function context exists");
        let function = LashExpr::Function(Box::new(FunctionExpr {
            name: internal_name.map(Into::into),
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

    fn lower_expr(&mut self, expr: &Expr) -> Result<LashExpr, Diagnostic> {
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
            Expr::Ident(name) if name == "undefined" && !self.has_binding(name) => {
                LashExpr::Undefined
            }
            Expr::Ident(name) if name == "NaN" && !self.has_binding(name) => {
                LashExpr::Number(f64::NAN)
            }
            Expr::Ident(name) if name == "Infinity" && !self.has_binding(name) => {
                LashExpr::Number(f64::INFINITY)
            }
            Expr::Ident(name)
                if matches!(name.as_str(), "String" | "Number" | "Boolean")
                    && !self.has_binding(name) =>
            {
                self.lower_conversion_function(name)
            }
            Expr::Ident(name) if name == "globalThis" && !self.has_binding(name) => {
                return Err(Diagnostic::refusal(
                    DiagnosticCode::UnsupportedExpression,
                    "Unsupported: bare globalThis. Use globalThis.identifier for durable session state.",
                    None,
                ));
            }
            Expr::Ident(name) if name == "arguments" && !self.has_binding(name) => {
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
            Expr::Ident(name) => LashExpr::Variable(self.resolve(name)?.into()),
            Expr::Array(items) => self.lower_array_literal(items)?,
            Expr::Object(entries) => self.lower_object_literal(entries)?,
            Expr::Assign { target, op, value } => self.lower_assignment(target, *op, value)?,
            Expr::Member { object, property } => self.lower_member(object, property)?,
            Expr::Unary { op, value } => match op {
                UnaryOp::Void => {
                    LashExpr::Block(vec![self.lower_expr(value)?, LashExpr::Undefined])
                }
                UnaryOp::Plus => js_unary(JavaScriptUnaryOp::Plus, self.lower_expr(value)?),
                UnaryOp::Minus => js_unary(JavaScriptUnaryOp::Negate, self.lower_expr(value)?),
                UnaryOp::Not => js_unary(JavaScriptUnaryOp::Not, self.lower_expr(value)?),
                UnaryOp::BitNot => self.lower_bit_not(value)?,
                UnaryOp::TypeOf if matches!(value.as_ref(), Expr::Ident(name) if !self.has_binding(name)) => {
                    LashExpr::String("undefined".into())
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
            Expr::Call { callee, args } => self.lower_call(callee, args)?,
            Expr::New { constructor, args } => self.lower_constructor(constructor, args)?,
            Expr::OptionalChain { base, operations } => {
                self.lower_optional_chain(base, operations)?
            }
            Expr::Await(inner) => self.lower_await(inner)?,
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

    fn lower_process_definition(
        &mut self,
        binding_name: &str,
        expression: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        let Expr::Call { args, .. } = expression else {
            unreachable!("caller identifies defineProcess calls")
        };
        let [CallArg::Value(Expr::Object(properties))] = args.as_slice() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessConfigLiteralRequired,
                "defineProcess expects one object literal",
                None,
            ));
        };
        let entries = properties
            .iter()
            .map(|property| match property {
                ObjectProperty::KeyValue(PropertyKey::Static(key), value) => {
                    Ok((key.as_str(), value))
                }
                _ => Err(Diagnostic::new(
                    DiagnosticCode::ProcessConfigLiteralRequired,
                    "defineProcess config requires static properties without spread",
                    None,
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let field = |name: &str| {
            entries
                .iter()
                .find_map(|(key, value)| (*key == name).then_some(*value))
        };
        let mut seen_fields = BTreeSet::new();
        if entries.iter().any(|(key, _)| {
            !matches!(*key, "name" | "signals" | "run") || !seen_fields.insert(*key)
        }) {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::ProcessConfigFieldUnsupported,
                "defineProcess accepts only name, signals, and run",
                "drop the extra key; anything else the process needs arrives as a `run` parameter",
                None,
            ));
        }
        let Some(Expr::String(process_name)) = field("name") else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessNameLiteralRequired,
                "defineProcess.name must be a string literal",
                None,
            ));
        };
        let signals = match field("signals") {
            None => Vec::new(),
            Some(Expr::Object(signals)) => signals
                .iter()
                .map(|property| match property {
                    ObjectProperty::KeyValue(PropertyKey::Static(name), _) => {
                        Ok(ProcessSignalDecl {
                            name: name.as_str().into(),
                            ty: TypeExpr::Any,
                        })
                    }
                    _ => Err(Diagnostic::new(
                        DiagnosticCode::ProcessSignalsLiteralRequired,
                        "defineProcess.signals requires static properties",
                        None,
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => {
                return Err(Diagnostic::new(
                    DiagnosticCode::ProcessSignalsLiteralRequired,
                    "defineProcess.signals must be an object literal",
                    None,
                ));
            }
        };
        let Some(Expr::Function(run)) = field("run") else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessRunLiteralRequired,
                "defineProcess.run must be a function literal",
                None,
            ));
        };
        if !run.is_async {
            return Err(Diagnostic::with_repair(
                DiagnosticCode::AsyncUnsupported,
                "defineProcess.run must be async",
                "write it as `run: async (...) => { ... }`",
                None,
            ));
        }
        if self
            .declarations
            .iter()
            .any(|declaration| matches!(declaration, Declaration::Process(process) if process.name.as_str() == process_name))
        {
            return Err(Diagnostic::new(
                DiagnosticCode::DuplicateBinding,
                format!("duplicate process name `{process_name}`"),
                None,
            ));
        }

        let closure = self.with_process(|lowerer| lowerer.lower_function(run, None))?;
        let function = match &closure {
            LashExpr::Function(function) => function.as_ref(),
            LashExpr::BuiltinCall { args, .. } => {
                let [LashExpr::Function(function), ..] = args.as_slice() else {
                    unreachable!("closure intrinsic contains a function")
                };
                function
            }
            _ => unreachable!("run lowering returns a function"),
        };
        if !function.captures.is_empty() {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessCaptureUnsupported,
                "defineProcess.run must receive durable inputs as parameters",
                None,
            ));
        }
        let params = function
            .params
            .iter()
            .map(|name| ProcessParam {
                name: name.clone(),
                ty: TypeExpr::Any,
            })
            .collect::<Vec<_>>();
        let call_args = function
            .params
            .iter()
            .map(|name| LashExpr::Variable(name.clone()))
            .collect();
        let failure_name = format!("{GENERATED_BINDING_PREFIX}process_error");
        self.declarations.push(Declaration::Process(ProcessDecl {
            name: process_name.as_str().into(),
            params,
            signals,
            return_ty: Some(TypeExpr::Any),
            label: None,
            body: LashExpr::Try(Box::new(TryExpr {
                body: Box::new(LashExpr::Finish(Box::new(LashExpr::Call {
                    function: Box::new(closure),
                    args: call_args,
                }))),
                catch: Some(CatchClause {
                    binding: failure_name.as_str().into(),
                    body: Box::new(LashExpr::Fail(Box::new(LashExpr::Variable(
                        failure_name.as_str().into(),
                    )))),
                }),
                finally: None,
            })),
        }));
        self.set_role(
            binding_name,
            BindingRole::ProcessDefinition(process_name.clone()),
        )?;
        Ok(LashExpr::ProcessRef {
            process: process_name.as_str().into(),
        })
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
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnknownBinding,
                        format!("unknown binding `{name}`"),
                        None,
                    ));
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
            Expr::Ident(name) => Ok((self.resolve(name)?, Vec::new())),
            Expr::Member { object, property } => {
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
        if matches!(object, Expr::Ident(name) if name == "globalThis" && !self.has_binding(name)) {
            return match property {
                MemberProperty::Field(field)
                    if !matches!(field.as_str(), "undefined" | "NaN" | "Infinity") =>
                {
                    Ok(if self.has_binding(field) {
                        LashExpr::Variable(field.as_str().into())
                    } else {
                        LashExpr::Undefined
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
        if let Expr::Ident(owner) = object
            && is_known_runtime_global(owner)
            && !self.has_binding(owner)
        {
            let name = match property {
                MemberProperty::Field(field) => field.as_str(),
                MemberProperty::Index(_) => "computed property",
            };
            let constant = match (owner.as_str(), name) {
                ("Number", "EPSILON") => Some(f64::EPSILON),
                ("Number", "MIN_SAFE_INTEGER") => Some(-9_007_199_254_740_991.0),
                ("Number", "MAX_SAFE_INTEGER") => Some(9_007_199_254_740_991.0),
                ("Number", "MAX_VALUE") => Some(f64::MAX),
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
        if let MemberProperty::Field(field) = property
            && let Some(mut path) = module_path(object)
            && path.first().is_some_and(|root| root == "trigger")
            && !self.has_binding("trigger")
        {
            path.push(field.clone());
            return Ok(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                path.into_iter().map(Into::into).collect(),
            )));
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

fn is_define_process_call(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Call { callee, .. }
            if matches!(callee.as_ref(), Expr::Ident(name) if name == "defineProcess")
    )
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

    // Report the cycle with the names the author wrote, not the mangled ones.
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
