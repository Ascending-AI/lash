use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    AssignPathStep, AssignTarget, CatchClause, Expr as LashExpr, FunctionExpr, JavaScriptBinaryOp,
    JavaScriptLogicalOp, JavaScriptUnaryOp, Program as LashProgram, TryExpr,
};

use crate::adapter::{
    self, AssignTarget as TsAssignTarget, BinaryOp, Expr, Function, FunctionBody, LogicalOp,
    MemberProperty, Stmt, UnaryOp, VarKind,
};
use crate::{Diagnostic, DiagnosticCode};

/// Every binding the lowerer generates carries this prefix, which the dialect
/// reserves so a source identifier can never collide with one.
const GENERATED_BINDING_PREFIX: &str = "__typescript_";

pub(crate) fn lower(program: &adapter::Program) -> Result<LashProgram, Diagnostic> {
    let mut lowerer = Lowerer::default();
    lowerer.scopes.push(Scope::default());
    let expressions = lowerer.lower_statements(&program.statements, true)?;
    Ok(LashProgram::block(expressions))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingKind {
    Const,
    Let,
    Function,
    Parameter,
    Catch,
}

impl BindingKind {
    fn mutable(self) -> bool {
        self == Self::Let
    }
}

#[derive(Clone, Debug)]
struct Binding {
    internal: String,
    kind: BindingKind,
    initialized: bool,
    owner_function: usize,
}

#[derive(Clone, Debug, Default)]
struct Scope {
    bindings: BTreeMap<String, Binding>,
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
    definition: FunctionExpr,
}

/// A binding whose assignment is emitted once every name it captures holds a
/// value.
struct PendingBinding {
    internal: String,
    captures: BTreeSet<String>,
    assignment: LashExpr,
}

#[derive(Default)]
struct Lowerer {
    scopes: Vec<Scope>,
    functions: Vec<FunctionContext>,
    next_binding: usize,
    next_function: usize,
    loop_depth: usize,
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
                let function = self.lower_function(function, Some(binding.internal.clone()))?;
                let LashExpr::Function(definition) = function else {
                    unreachable!("function lowering returns a function expression")
                };
                pending.push(PendingFunction {
                    internal: binding.internal.clone(),
                    captures: definition
                        .captures
                        .iter()
                        .map(|capture| capture.as_str().to_string())
                        .collect(),
                    definition: *definition,
                });
            }
        }
        self.allow_uninitialized_declaration_capture = previous_capture_mode;
        let mut pending = self.route_mutual_recursion_through_frames(pending);

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
        let mut output = Vec::new();
        for statement in statements {
            flush_ready(&mut pending, &mut available, &mut output);
            match statement {
                Stmt::Function { .. } => {}
                Stmt::Var { declarations, .. } => {
                    output.extend(self.lower_stmt(statement)?);
                    for declaration in declarations {
                        available.insert(self.binding(&declaration.name)?.internal.clone());
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

    /// Hoisted function declarations may reference each other cyclically, which
    /// no emission order can satisfy while closures capture by value. Each
    /// cycle gets one generated frame record: every member captures the record
    /// instead of its peers and rebinds those peers from it on entry, so the
    /// peer values are read when the member runs rather than when it is built.
    fn route_mutual_recursion_through_frames(
        &mut self,
        pending: Vec<PendingFunction>,
    ) -> Vec<PendingBinding> {
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
        let mut frame_of = vec![None; pending.len()];
        let mut frames = Vec::new();
        for component in strongly_connected_components(&edges) {
            if component.len() < 2 {
                continue;
            }
            let frame = format!("{GENERATED_BINDING_PREFIX}{}_frame", self.next_binding);
            self.next_binding += 1;
            let peers = component
                .iter()
                .map(|member| pending[*member].internal.clone())
                .collect::<BTreeSet<_>>();
            for member in &component {
                frame_of[*member] = Some(frames.len());
            }
            frames.push((frame, peers));
        }

        let mut bindings = Vec::with_capacity(pending.len() + frames.len());
        for (frame, _) in &frames {
            bindings.push(PendingBinding {
                internal: frame.clone(),
                captures: BTreeSet::new(),
                assignment: LashExpr::Assign {
                    target: AssignTarget::variable(frame.as_str().into()),
                    expr: Box::new(LashExpr::Record(Vec::new())),
                },
            });
        }
        for (index, function) in pending.into_iter().enumerate() {
            let Some((frame, peers)) = frame_of[index].map(|frame| &frames[frame]) else {
                bindings.push(PendingBinding {
                    internal: function.internal.clone(),
                    captures: function.captures,
                    assignment: LashExpr::Assign {
                        target: AssignTarget::variable(function.internal.into()),
                        expr: Box::new(LashExpr::Function(Box::new(function.definition))),
                    },
                });
                continue;
            };
            bindings.push(frame_bound_member(function, frame, peers));
        }
        bindings
    }

    fn predeclare(&mut self, statements: &[Stmt], root: bool) -> Result<(), Diagnostic> {
        for statement in statements {
            match statement {
                Stmt::Var { kind, declarations } => {
                    for declaration in declarations {
                        self.declare(
                            &declaration.name,
                            match kind {
                                VarKind::Const => BindingKind::Const,
                                VarKind::Let => BindingKind::Let,
                            },
                            false,
                            root,
                        )?;
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
        if name.starts_with(GENERATED_BINDING_PREFIX) {
            return Err(Diagnostic::new(
                DiagnosticCode::ReservedIdentifier,
                format!(
                    "`{name}` is reserved: identifiers starting with `{GENERATED_BINDING_PREFIX}` name the lowerer's generated bindings"
                ),
                None,
            ));
        }
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
            },
        );
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
            if binding.kind.mutable() {
                return Err(Diagnostic::new(
                    DiagnosticCode::MutableCaptureUnsupported,
                    format!(
                        "mutable binding `{name}` cannot be captured until live lexical cells are available"
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
                    if *kind == VarKind::Const && declaration.init.is_none() {
                        return Err(Diagnostic::new(
                            DiagnosticCode::MissingInitializer,
                            format!("const `{}` requires an initializer", declaration.name),
                            None,
                        ));
                    }
                    let value = declaration
                        .init
                        .as_ref()
                        .map(|expr| self.lower_expr(expr))
                        .transpose()?
                        .unwrap_or(LashExpr::Undefined);
                    let target = self.binding(&declaration.name)?.internal.clone();
                    self.initialize(&declaration.name);
                    output.push(LashExpr::Assign {
                        target: AssignTarget::variable(target.into()),
                        expr: Box::new(value),
                    });
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
                self.loop_depth += 1;
                let body = self.lower_stmt_block(body)?;
                self.loop_depth -= 1;
                vec![LashExpr::While {
                    condition: Box::new(self.lower_expr(test)?),
                    body: Box::new(body),
                }]
            }
            Stmt::Break => {
                if self.loop_depth == 0 {
                    return Err(Diagnostic::new(
                        DiagnosticCode::LoopControlOutsideLoop,
                        "break is only valid in a loop",
                        None,
                    ));
                }
                vec![LashExpr::Break]
            }
            Stmt::Continue => {
                if self.loop_depth == 0 {
                    return Err(Diagnostic::new(
                        DiagnosticCode::LoopControlOutsideLoop,
                        "continue is only valid in a loop",
                        None,
                    ));
                }
                vec![LashExpr::Continue]
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
                        self.declare(&catch.binding, BindingKind::Catch, true, false)?;
                        let binding = self.binding(&catch.binding)?.internal.clone();
                        let body = LashExpr::Block(self.lower_statements(&catch.body, false)?);
                        self.scopes.pop();
                        Ok(CatchClause {
                            binding: binding.into(),
                            body: Box::new(body),
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
        let outer_loop_depth = std::mem::take(&mut self.loop_depth);
        self.next_function += 1;
        let id = self.next_function;
        self.functions.push(FunctionContext {
            id,
            ..FunctionContext::default()
        });
        self.scopes.push(Scope::default());
        if let (Some(source_name), Some(internal)) = (&function.name, &internal_name) {
            self.scopes.last_mut().unwrap().bindings.insert(
                source_name.clone(),
                Binding {
                    internal: internal.clone(),
                    kind: BindingKind::Function,
                    initialized: true,
                    owner_function: id,
                },
            );
        }
        let mut params = Vec::with_capacity(function.params.len());
        for param in &function.params {
            self.declare(param, BindingKind::Parameter, true, true)?;
            params.push(self.binding(param)?.internal.clone().into());
        }
        let body = match &function.body {
            FunctionBody::Expression(expr) => LashExpr::Return(Box::new(self.lower_expr(expr)?)),
            FunctionBody::Block(statements) => {
                let mut body = self.lower_statements(statements, true)?;
                body.push(LashExpr::Undefined);
                LashExpr::Block(body)
            }
        };
        self.scopes.pop();
        let context = self.functions.pop().expect("function context exists");
        self.loop_depth = outer_loop_depth;
        Ok(LashExpr::Function(Box::new(FunctionExpr {
            name: internal_name.map(Into::into),
            params,
            captures: context.captures.into_iter().map(Into::into).collect(),
            body: Box::new(body),
        })))
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<LashExpr, Diagnostic> {
        Ok(match expr {
            Expr::Undefined => LashExpr::Undefined,
            Expr::Null => LashExpr::Null,
            Expr::Bool(value) => LashExpr::Bool(*value),
            Expr::Number(value) => LashExpr::Number(*value),
            Expr::String(value) => LashExpr::String(value.as_str().into()),
            Expr::Ident(name) if name == "undefined" && !self.has_binding(name) => {
                LashExpr::Undefined
            }
            Expr::Ident(name) if name == "NaN" && !self.has_binding(name) => {
                LashExpr::Number(f64::NAN)
            }
            Expr::Ident(name) if name == "Infinity" && !self.has_binding(name) => {
                LashExpr::Number(f64::INFINITY)
            }
            Expr::Ident(name) => LashExpr::Variable(self.resolve(name)?.into()),
            Expr::Array(items) => LashExpr::List(
                items
                    .iter()
                    .map(|item| self.lower_expr(item))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Object(entries) => LashExpr::Record(
                entries
                    .iter()
                    .map(|(name, value)| Ok((name.as_str().into(), self.lower_expr(value)?)))
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            Expr::Assign { target, value } => {
                let target = self.lower_assign_target(target)?;
                let value = self.lower_expr(value)?;
                LashExpr::Assign {
                    target,
                    expr: Box::new(value),
                }
            }
            Expr::Member { object, property } => self.lower_member(object, property)?,
            Expr::Unary { op, value } => match op {
                UnaryOp::Void => {
                    LashExpr::Block(vec![self.lower_expr(value)?, LashExpr::Undefined])
                }
                UnaryOp::Plus => js_unary(JavaScriptUnaryOp::Plus, self.lower_expr(value)?),
                UnaryOp::Minus => js_unary(JavaScriptUnaryOp::Negate, self.lower_expr(value)?),
                UnaryOp::Not => js_unary(JavaScriptUnaryOp::Not, self.lower_expr(value)?),
                UnaryOp::TypeOf if matches!(value.as_ref(), Expr::Ident(name) if !self.has_binding(name)) => {
                    LashExpr::String("undefined".into())
                }
                UnaryOp::TypeOf => js_unary(JavaScriptUnaryOp::TypeOf, self.lower_expr(value)?),
            },
            Expr::Binary { left, op, right } => LashExpr::JavaScriptBinary {
                left: Box::new(self.lower_expr(left)?),
                op: map_binary(*op),
                right: Box::new(self.lower_expr(right)?),
            },
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
            _ => Err(Diagnostic::new(
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

    fn lower_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<LashExpr, Diagnostic> {
        if let Expr::Ident(name) = callee
            && !self.has_binding(name)
        {
            return match (name.as_str(), args) {
                ("finish", [value]) => Ok(LashExpr::Finish(Box::new(self.lower_expr(value)?))),
                ("print", [value]) => Ok(LashExpr::Print(Box::new(self.lower_expr(value)?))),
                ("finish" | "print", _) => Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    format!("{name} expects one argument"),
                    None,
                )),
                _ => Ok(LashExpr::Call {
                    function: Box::new(self.lower_expr(callee)?),
                    args: args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<_, _>>()?,
                }),
            };
        }
        if let Expr::Member {
            object,
            property: MemberProperty::Field(method),
        } = callee
        {
            if matches!(object.as_ref(), Expr::Ident(name) if name == "console") && method == "log"
            {
                if !self.has_binding("console") {
                    let mut lowered = args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter();
                    let joined = lowered.next().map_or_else(
                        || LashExpr::String("".into()),
                        |first| js_add(LashExpr::String("".into()), first),
                    );
                    let joined = lowered.fold(joined, |joined, value| {
                        js_add(js_add(joined, LashExpr::String(" ".into())), value)
                    });
                    return Ok(LashExpr::Print(Box::new(joined)));
                }
                if self.has_binding("console") {
                    return Ok(LashExpr::Call {
                        function: Box::new(self.lower_expr(callee)?),
                        args: args
                            .iter()
                            .map(|arg| self.lower_expr(arg))
                            .collect::<Result<_, _>>()?,
                    });
                }
            }
            let target = self.lower_expr(object)?;
            let lowered = args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<Vec<_>, _>>()?;
            let (builtin, builtin_args) = match (method.as_str(), lowered.as_slice()) {
                ("toUpperCase", []) => ("upper", vec![target]),
                ("toLowerCase", []) => ("lower", vec![target]),
                ("trim", []) => ("trim", vec![target]),
                ("startsWith", [arg]) => ("starts_with", vec![target, arg.clone()]),
                ("endsWith", [arg]) => ("ends_with", vec![target, arg.clone()]),
                ("includes", [arg]) => ("contains", vec![target, arg.clone()]),
                ("split", []) => ("__typescript_split", vec![target, LashExpr::Undefined]),
                ("split", [arg]) => ("__typescript_split", vec![target, arg.clone()]),
                ("join", []) => (
                    "__typescript_join",
                    vec![target, LashExpr::String(",".into())],
                ),
                ("join", [arg]) => ("__typescript_join", vec![target, arg.clone()]),
                _ => {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("method `{method}` is not in the accepted dialect"),
                        None,
                    ));
                }
            };
            return Ok(LashExpr::BuiltinCall {
                name: builtin.into(),
                args: builtin_args,
            });
        }
        Ok(LashExpr::Call {
            function: Box::new(self.lower_expr(callee)?),
            args: args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// Rebuild one member of a mutually recursive group against its frame record.
///
/// The member captures the record in place of its peers and opens its body by
/// reading each peer out of the record into the member's own frame slot, so the
/// body — and every closure nested in it — keeps referring to the peers by the
/// same name it already lowered.
fn frame_bound_member(
    function: PendingFunction,
    frame: &str,
    peers: &BTreeSet<String>,
) -> PendingBinding {
    let PendingFunction {
        internal,
        captures,
        mut definition,
    } = function;
    let bound_peers = captures.intersection(peers).cloned().collect::<Vec<_>>();
    let captures = captures
        .difference(peers)
        .cloned()
        .chain(std::iter::once(frame.to_string()))
        .collect::<BTreeSet<_>>();
    definition.captures = captures.iter().map(|capture| capture.into()).collect();
    let mut body = bound_peers
        .into_iter()
        .map(|peer| LashExpr::Assign {
            target: AssignTarget::variable(peer.as_str().into()),
            expr: Box::new(frame_field(frame, &peer)),
        })
        .collect::<Vec<_>>();
    body.push(*definition.body);
    definition.body = Box::new(LashExpr::Block(body));
    PendingBinding {
        assignment: LashExpr::Block(vec![
            LashExpr::Assign {
                target: AssignTarget {
                    root: frame.into(),
                    steps: vec![AssignPathStep::Field(internal.as_str().into())],
                },
                expr: Box::new(LashExpr::Function(Box::new(definition))),
            },
            LashExpr::Assign {
                target: AssignTarget::variable(internal.as_str().into()),
                expr: Box::new(frame_field(frame, &internal)),
            },
        ]),
        internal,
        captures,
    }
}

fn frame_field(frame: &str, field: &str) -> LashExpr {
    LashExpr::Field {
        target: Box::new(LashExpr::Variable(frame.into())),
        field: field.into(),
    }
}

/// Kosaraju's algorithm over the capture graph, iterative so that a deeply
/// chained set of declarations cannot exhaust the native stack.
fn strongly_connected_components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut reversed = vec![Vec::new(); edges.len()];
    for (from, targets) in edges.iter().enumerate() {
        for to in targets {
            reversed[*to].push(from);
        }
    }

    let mut order = Vec::with_capacity(edges.len());
    let mut visited = vec![false; edges.len()];
    for start in 0..edges.len() {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0usize)];
        while let Some((node, next)) = stack.pop() {
            match edges[node].get(next) {
                Some(target) => {
                    stack.push((node, next + 1));
                    if !visited[*target] {
                        visited[*target] = true;
                        stack.push((*target, 0));
                    }
                }
                None => order.push(node),
            }
        }
    }

    let mut components = Vec::new();
    let mut assigned = vec![false; edges.len()];
    for start in order.into_iter().rev() {
        if assigned[start] {
            continue;
        }
        assigned[start] = true;
        let mut component = vec![start];
        let mut stack = vec![start];
        while let Some(node) = stack.pop() {
            for target in &reversed[node] {
                if !assigned[*target] {
                    assigned[*target] = true;
                    component.push(*target);
                    stack.push(*target);
                }
            }
        }
        component.sort_unstable();
        components.push(component);
    }
    components
}

fn js_unary(op: JavaScriptUnaryOp, expr: LashExpr) -> LashExpr {
    LashExpr::JavaScriptUnary {
        op,
        expr: Box::new(expr),
    }
}

fn js_add(left: LashExpr, right: LashExpr) -> LashExpr {
    LashExpr::JavaScriptBinary {
        left: Box::new(left),
        op: JavaScriptBinaryOp::Add,
        right: Box::new(right),
    }
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
    }
}
