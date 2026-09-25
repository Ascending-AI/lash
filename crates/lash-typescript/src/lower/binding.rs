//! The scope stack's data: what a binding is, and what the lowerer knows
//! about it.
//!
//! Every per-binding fact belongs here rather than in a side table on the
//! lowerer. A table keyed by name outlives the scope that declared the
//! binding, and internal names are unique only where a binding is visibly
//! shadowed — so sibling scopes shared keys and a dead binding's facts
//! changed how a live one lowered.

use std::collections::{BTreeMap, BTreeSet};

use lashlang::is_javascript_builtin_global;

use super::captures::{BindingId, SlotKey};
use super::{
    BinaryOp, CallArg, Expr, Function, FunctionBody, MemberProperty, Pattern, Stmt, TsAssignTarget,
    VarKind, is_reserved_name, pattern_names, reserved_identifier,
};
use crate::{Diagnostic, DiagnosticCode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BindingKind {
    Const,
    Let,
    Var,
    Function,
    Parameter,
    Catch,
}

/// The exotic iterables whose `forEach` and `entries`/`keys`/`values` are the
/// collection's own, not the array methods of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IterableKind {
    Map,
    Set,
    UrlSearchParams,
}

impl IterableKind {
    pub(super) fn from_constructor(constructor: &str) -> Option<Self> {
        match constructor {
            "Map" => Some(Self::Map),
            "Set" => Some(Self::Set),
            "URLSearchParams" => Some(Self::UrlSearchParams),
            _ => None,
        }
    }
}

/// What a binding *is*, beyond the name it holds.
///
/// Every one of these facts is per-binding, so it lives on the binding and
/// dies with the scope that declared it. Held in a side table keyed by name
/// they outlived their binding and leaked across sibling scopes, which is
/// exactly the shadowing case the internal names are only unique under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BindingRole {
    /// An ordinary value. Everything the lowerer has no extra fact about.
    Plain,
    /// An async function bound to a name, which must be awaited at its call.
    AsyncHelper,
    /// A session-global handle from an earlier cell's process start, which
    /// `await` resolves to the process result.
    ProcessHandle,
    /// A collection whose iteration protocol is its own, not an array's.
    ExoticIterable(IterableKind),
    /// A session slot only a `globalThis.name` write creates: a global object
    /// property in ECMA-262, absent until a write runs, so `typeof name`
    /// reads it live rather than faulting on the empty slot.
    GlobalProperty,
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    /// The binding's key in the capture ledger.
    pub(super) id: BindingId,
    pub(super) internal: String,
    pub(super) kind: BindingKind,
    pub(super) initialized: bool,
    pub(super) owner_function: usize,
    pub(super) role: BindingRole,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Scope {
    pub(super) bindings: BTreeMap<String, Binding>,
}

/// The scope stack's operations: declaring a binding, learning what it holds,
/// and resolving a read against it.
///
/// These live beside the per-binding data they walk rather than in `lower`'s
/// entry module, which is at its line budget.
impl super::Lowerer {
    pub(super) fn declare(
        &mut self,
        name: &str,
        kind: BindingKind,
        initialized: bool,
        preserve_name: bool,
    ) -> Result<(), Diagnostic> {
        let owner_function = self.current_function();
        // A binding declared in a block of the cell's top level ends with its
        // block (ECMA-262 lexical scoping), so it never becomes a session
        // global. A function frame's locals are never globals to begin with.
        let block_private = owner_function == 0 && self.scopes.len() > self.root_scope_depth;
        // `undefined`, `NaN` and `Infinity` are the reserved *value*
        // identifiers: a bare read of one lowers to its literal, so a
        // session-global slot of that name would shadow the literal (and tsc
        // refuses the top-level redeclare — TS2397/TS2403/TS2451). Any nested
        // scope — a function, a block, a `catch` — may bind the name, as Node
        // and tsc accept.
        if owner_function == 0 && !block_private && matches!(name, "undefined" | "NaN" | "Infinity")
        {
            return Err(Diagnostic::new(
                DiagnosticCode::ReservedIdentifier,
                format!(
                    "`{name}` is a reserved TypeScript value identifier and cannot be shadowed"
                ),
                None,
            ));
        }
        if is_reserved_name(name) {
            return Err(reserved_identifier(name));
        }
        // Mangling exists to stop an inner scope from overwriting an outer slot
        // of the same name. Where nothing of that name is visible there is
        // nothing to protect, and a mangled root-level binding would be a
        // private slot, so the author's binding would not survive the cell as
        // a session global; keep the author's name in that case. A top-level
        // block binding shares the root frame with the session globals, so
        // where the cell addresses `globalThis.name` its slot is mangled too:
        // the root slot spelled `name` is the session global, whatever block
        // is open. Elsewhere it keeps the authored spelling the workflow-graph
        // lens prints back.
        let shares_a_global_slot = block_private && self.global_this_names.contains(name);
        // A function's parameter expressions run before its body declares
        // anything, so a default can capture an outer binding the body then
        // redeclares (ECMA-262 gives such a body its own variable
        // environment). The frame holds the captured binding under its
        // internal name, so the body's binding of the same name takes a
        // generated slot rather than sharing that one.
        let captured_by_frame = owner_function != 0
            && self
                .functions
                .last()
                .is_some_and(|function| function.captures.contains(name));
        let preserve_name = !shares_a_global_slot
            && !captured_by_frame
            && (preserve_name || !self.has_binding(name));
        if self
            .scopes
            .last()
            .is_some_and(|scope| scope.bindings.contains_key(name))
        {
            return Err(Diagnostic::new(
                DiagnosticCode::DuplicateBinding,
                format!("duplicate lexical binding `{name}`"),
                None,
            ));
        }
        let internal = if preserve_name {
            name.to_string()
        } else {
            self.generated_binding(name)
        };
        if block_private {
            self.private_bindings.insert(internal.clone());
        }
        let id = self.declare_in_ledger(kind, (owner_function, internal.clone()));
        #[expect(
            clippy::expect_used,
            reason = "the lowerer pushes the program root scope before any declaration and never pops past it"
        )]
        let scope = self.scopes.last_mut().expect("a scope is always active");
        scope.bindings.insert(
            name.to_string(),
            Binding {
                id,
                internal,
                kind,
                initialized,
                owner_function,
                role: BindingRole::Plain,
            },
        );
        Ok(())
    }

    /// The role is learned from the initializer, so it is always set after the
    /// declaration that a lexical scope hoists — the same resolution `binding`
    /// performs, against the same scope stack, so the fact lands on the
    /// binding the reads will find and dies when its scope pops.
    pub(super) fn set_role(&mut self, name: &str, role: BindingRole) -> Result<(), Diagnostic> {
        let span = self.current_span;
        if !self.has_binding(name) {
            return Err(self.unknown_binding(name, span));
        }
        let Some(binding) = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.bindings.get_mut(name))
        else {
            unreachable!("the binding was found above")
        };
        binding.role = role;
        Ok(())
    }

    pub(super) fn clear_process_handle_role(&mut self, name: &str) -> Result<(), Diagnostic> {
        let span = self.current_span;
        if !self.has_binding(name) {
            return Err(self.unknown_binding(name, span));
        }
        let Some(binding) = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.bindings.get_mut(name))
        else {
            unreachable!("the binding was found above")
        };
        if binding.role == BindingRole::ProcessHandle {
            binding.role = BindingRole::Plain;
        }
        Ok(())
    }

    pub(super) fn binding(&self, name: &str) -> Result<&Binding, Diagnostic> {
        let span = self.current_span;
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .ok_or_else(|| self.unknown_binding(name, span))
    }

    pub(super) fn resolve(&mut self, name: &str) -> Result<String, Diagnostic> {
        let span = self.current_span;
        let Some(binding) = self
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.bindings.get(name))
            .cloned()
        else {
            return Err(self.unknown_binding(name, span));
        };
        let current_function = self.current_function();
        if current_function == binding.owner_function && !binding.initialized {
            return Err(Diagnostic::new(
                DiagnosticCode::TemporalDeadZone,
                format!("`{name}` is read before initialization"),
                span,
            ));
        }
        if current_function != binding.owner_function {
            if !binding.initialized && !self.allow_uninitialized_declaration_capture {
                return Err(Diagnostic::new(
                    DiagnosticCode::TemporalDeadZone,
                    format!(
                        "captured binding `{name}` is not initialized when the closure is created"
                    ),
                    span,
                ));
            }
            self.capture(&binding);
        }
        Ok(binding.internal)
    }

    /// Captures `binding`, which an enclosing frame owns, into every closure
    /// between that frame and the current one.
    pub(super) fn capture(&mut self, binding: &Binding) {
        let first_capturing_function = self
            .functions
            .iter()
            .position(|function| function.id == binding.owner_function)
            .map_or(0, |owner| owner + 1);
        for function in &mut self.functions[first_capturing_function..] {
            function.captures.insert(binding.internal.clone());
        }
        // The outermost capturing closure is the one the owning frame
        // creates, so its creation is when the value is copied; the closures
        // inside it copy from that copy.
        let creation = self.functions[first_capturing_function].creation.clone();
        self.capture_ledger.capture(binding.id, creation);
    }

    /// The body's own `var` of a parameter's name. In a function whose
    /// parameters contain expressions it is a new binding in a generated slot
    /// that starts with the parameter's value, replacing the parameter for the
    /// body (ECMA-262 FunctionDeclarationInstantiation step 28); otherwise it
    /// is the parameter itself, and `None`.
    pub(super) fn separate_parameter_var(
        &mut self,
        name: &str,
        parameter: &str,
    ) -> Option<lashlang::Expr> {
        if !self
            .functions
            .last()
            .is_some_and(|function| function.separate_var_environment)
        {
            return None;
        }
        let owner_function = self.current_function();
        let internal = self.generated_binding(name);
        let id = self.declare_in_ledger(BindingKind::Var, (owner_function, internal.clone()));
        let binding = Binding {
            id,
            internal: internal.clone(),
            kind: BindingKind::Var,
            initialized: true,
            owner_function,
            role: BindingRole::Plain,
        };
        let value =
            self.binding_initial_value(&binding, lashlang::Expr::Variable(parameter.into()));
        #[expect(
            clippy::expect_used,
            reason = "the function body's scope declared the parameter being replaced"
        )]
        self.scopes
            .last_mut()
            .expect("a scope is always active")
            .bindings
            .insert(name.to_string(), binding);
        Some(lashlang::Expr::Assign {
            target: lashlang::AssignTarget::variable(internal.into()),
            expr: Box::new(value),
        })
    }

    /// Registers a binding, which lives in `slot`, with the capture ledger. A
    /// `var` (and an enum, which is one) belongs to its whole function frame,
    /// so no loop makes it fresh; every other binding is fresh in each loop
    /// around its declaration.
    pub(super) fn declare_in_ledger(&mut self, kind: BindingKind, slot: SlotKey) -> BindingId {
        let loops = match kind {
            BindingKind::Var => Vec::new(),
            _ => self.position.loops.clone(),
        };
        self.capture_ledger.declare(slot, loops)
    }

    /// Whether `binding` lives in a binding cell: its slot is one the capture
    /// ledger boxed (FIG-3707), and it is not a top-level session slot, which
    /// closures reach live through the session instead.
    pub(super) fn is_cell(&self, binding: &Binding) -> bool {
        self.cells
            .contains(&(binding.owner_function, binding.internal.clone()))
            && !self.is_session_slot(binding)
    }

    /// Whether `binding` is a top-level session slot: owned by the cell's root
    /// frame and not one of its private slots.
    pub(super) fn is_session_slot(&self, binding: &Binding) -> bool {
        binding.owner_function == 0 && !self.private_bindings.contains(&binding.internal)
    }

    /// Records an assignment to `binding` at the current point of its frame.
    pub(super) fn record_write(&mut self, binding: BindingId) {
        let site = self.capture_ledger.site(&self.position.loops);
        self.capture_ledger.write(binding, site);
    }

    /// Records an assignment's store to `name` at the point it happens: after
    /// its value, which may create a closure over `name` that the store then
    /// makes stale. Resolving the target recorded the write at the point the
    /// reference is taken, ahead of the value.
    pub(super) fn record_store(&mut self, name: &str) -> Result<(), Diagnostic> {
        let binding = self.binding(name)?.id;
        self.record_write(binding);
        Ok(())
    }

    /// Records a `globalThis.name` write or delete, which lands on the root
    /// slot `name`. At the root that is an ordinary write; inside a function
    /// it runs whenever the function is called.
    pub(super) fn record_global_write(&mut self, name: &str) {
        let Some(binding) = self
            .scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.bindings.values())
            .find(|binding| binding.owner_function == 0 && binding.internal == name)
            .map(|binding| binding.id)
        else {
            return;
        };
        if self.current_function() == 0 {
            self.record_write(binding);
        } else {
            self.capture_ledger.write_anytime(binding);
        }
    }

    pub(super) fn initialize(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(binding) = scope.bindings.get_mut(name) {
                binding.initialized = true;
                return;
            }
        }
    }
}

/// The `var` names a function body (or the script) declares at any depth,
/// which the body's frame hoists.
pub(super) fn function_var_names(statements: &[Stmt]) -> Vec<String> {
    fn visit(statement: &Stmt, names: &mut Vec<String>) {
        match statement {
            Stmt::Spanned(_, stmt) | Stmt::Labeled { stmt, .. } => visit(stmt, names),
            Stmt::Enum { name, .. } => names.push(name.clone()),
            Stmt::Var {
                kind: VarKind::Var,
                declarations,
            } => {
                for declaration in declarations {
                    pattern_names(&declaration.pattern, names);
                }
            }
            Stmt::Block(statements) => statements.iter().for_each(|stmt| visit(stmt, names)),
            Stmt::If {
                consequent,
                alternate,
                ..
            } => {
                visit(consequent, names);
                if let Some(alternate) = alternate {
                    visit(alternate, names);
                }
            }
            // A `var` loop head declares the enclosing function's (or the
            // script's) one binding, which every iteration assigns.
            Stmt::ForOf {
                pattern,
                kind: Some(VarKind::Var),
                body,
                ..
            }
            | Stmt::ForIn {
                pattern,
                kind: Some(VarKind::Var),
                body,
                ..
            } => {
                pattern_names(pattern, names);
                visit(body, names);
            }
            // So does a classic `for` head's `var`.
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    visit(init, names);
                }
                visit(body, names);
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::ForOf { body, .. }
            | Stmt::ForIn { body, .. } => visit(body, names),
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    case.consequent.iter().for_each(|stmt| visit(stmt, names));
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                body.iter().for_each(|stmt| visit(stmt, names));
                if let Some(catch) = catch {
                    catch.body.iter().for_each(|stmt| visit(stmt, names));
                }
                if let Some(finally) = finally {
                    finally.iter().for_each(|stmt| visit(stmt, names));
                }
            }
            Stmt::Function { .. }
            | Stmt::Empty
            | Stmt::Expr(_)
            | Stmt::Return(_)
            | Stmt::Break
            | Stmt::Continue
            | Stmt::Throw(_)
            | Stmt::Var { .. } => {}
        }
    }
    let mut names = Vec::new();
    statements.iter().for_each(|stmt| visit(stmt, &mut names));
    names.sort();
    names.dedup();
    names
}

/// Every name the program addresses as `globalThis.name`: read, written,
/// deleted, or tested with `"name" in globalThis`, at any depth.
pub(super) fn global_this_names(statements: &[Stmt]) -> BTreeSet<String> {
    global_this_accesses(statements, false)
}

/// Every name the program writes as `globalThis.name`, at any depth.
pub(super) fn global_this_writes(statements: &[Stmt]) -> BTreeSet<String> {
    global_this_accesses(statements, true)
}

fn global_this_accesses(statements: &[Stmt], writes_only: bool) -> BTreeSet<String> {
    fn global_member<'a>(object: &'a Expr, property: &'a MemberProperty) -> Option<&'a str> {
        match (object, property) {
            (Expr::Ident(root, _), MemberProperty::Field(field)) if root == "globalThis" => {
                Some(field.as_str())
            }
            _ => None,
        }
    }
    fn visit(expression: &Expr, writes_only: bool, names: &mut BTreeSet<String>) {
        let named = match expression {
            Expr::Assign {
                target: TsAssignTarget::Member { object, property },
                ..
            }
            | Expr::Update {
                target: TsAssignTarget::Member { object, property },
                ..
            } => global_member(object, property),
            _ if writes_only => None,
            Expr::Member {
                object, property, ..
            }
            | Expr::Delete { object, property } => global_member(object, property),
            Expr::Binary {
                left,
                op: BinaryOp::In,
                right,
            } => match (left.as_ref(), right.as_ref()) {
                (Expr::String(name), Expr::Ident(root, _)) if root == "globalThis" => {
                    Some(name.as_str())
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(name) = named {
            names.insert(name.to_string());
        }
        for child in expression.children() {
            visit(child, writes_only, names);
        }
    }
    let mut names = BTreeSet::new();
    for statement in statements {
        for expression in statement.child_expressions() {
            visit(expression, writes_only, &mut names);
        }
    }
    names
}

/// Every advertised built-in global the program writes as a bare name, at
/// any depth — `Object = x`, `Math += y`, `Number++`, `for (JSON of xs)`,
/// `{ n: Number } = now`, `[Map] = ms`. The entry point binds each as a
/// session slot ahead of lowering, seeded with the built-in object the name
/// answered before any write, so the write has ECMA's global property to
/// land on.
pub(super) fn builtin_global_writes(statements: &[Stmt]) -> BTreeSet<String> {
    // `eval` reads as a built-in like the others, but strict code may never
    // write it: the target check reports the early SyntaxError, so no slot
    // may stand ready to take one.
    fn assignable(name: &str) -> bool {
        is_javascript_builtin_global(name) && name != "eval"
    }
    fn collect_pattern_names(pattern: &Pattern, names: &mut BTreeSet<String>) {
        let mut declared = Vec::new();
        pattern_names(pattern, &mut declared);
        names.extend(declared.into_iter().filter(|name| assignable(name)));
    }
    fn collect_target(target: &TsAssignTarget, names: &mut BTreeSet<String>) {
        match target {
            TsAssignTarget::Ident(name) | TsAssignTarget::ParenIdent(name) if assignable(name) => {
                names.insert(name.clone());
            }
            TsAssignTarget::Pattern(pattern) => collect_pattern_names(pattern, names),
            _ => {}
        }
    }
    fn visit_expression(expression: &Expr, names: &mut BTreeSet<String>) {
        match expression {
            Expr::Assign { target, .. } | Expr::Update { target, .. } => {
                collect_target(target, names);
            }
            // A nested function's body needs the statement walk, not the
            // flattened expression stream, or a loop target inside it is
            // missed.
            Expr::Function(function) => {
                for parameter in &function.params {
                    for expression in parameter.child_expressions() {
                        visit_expression(expression, names);
                    }
                }
                match &function.body {
                    FunctionBody::Block(statements) => {
                        for statement in statements {
                            visit_statement(statement, names);
                        }
                    }
                    FunctionBody::Expression(expression) => {
                        visit_expression(expression, names);
                    }
                }
                return;
            }
            _ => {}
        }
        for child in expression.children() {
            visit_expression(child, names);
        }
    }
    fn visit_statement(statement: &Stmt, names: &mut BTreeSet<String>) {
        match statement {
            Stmt::Spanned(_, statement)
            | Stmt::Labeled {
                stmt: statement, ..
            } => visit_statement(statement, names),
            Stmt::Expr(expression) | Stmt::Throw(expression) => visit_expression(expression, names),
            Stmt::Return(expression) => {
                if let Some(expression) = expression {
                    visit_expression(expression, names);
                }
            }
            Stmt::Block(statements) => {
                statements
                    .iter()
                    .for_each(|statement| visit_statement(statement, names));
            }
            Stmt::Var { declarations, .. } => {
                for declaration in declarations {
                    if let Some(initializer) = &declaration.init {
                        visit_expression(initializer, names);
                    }
                    for expression in declaration.pattern.child_expressions() {
                        visit_expression(expression, names);
                    }
                }
            }
            Stmt::Enum { members, .. } => {
                members
                    .iter()
                    .for_each(|member| visit_expression(&member.value, names));
            }
            Stmt::If {
                test,
                consequent,
                alternate,
            } => {
                visit_expression(test, names);
                visit_statement(consequent, names);
                if let Some(alternate) = alternate {
                    visit_statement(alternate, names);
                }
            }
            Stmt::While { test, body } => {
                visit_expression(test, names);
                visit_statement(body, names);
            }
            Stmt::DoWhile { body, test, .. } => {
                visit_statement(body, names);
                visit_expression(test, names);
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                if let Some(init) = init {
                    visit_statement(init, names);
                }
                if let Some(test) = test {
                    visit_expression(test, names);
                }
                if let Some(update) = update {
                    visit_expression(update, names);
                }
                visit_statement(body, names);
            }
            // A declaration-free loop target is an assignment: `for (Object
            // of xs)` writes the global property each iteration, where `for
            // (let Object of xs)` declares a shadow.
            Stmt::ForOf {
                pattern,
                iterable,
                body,
                kind,
            }
            | Stmt::ForIn {
                pattern,
                object: iterable,
                body,
                kind,
            } => {
                if kind.is_none() {
                    collect_pattern_names(pattern, names);
                }
                for expression in pattern.child_expressions() {
                    visit_expression(expression, names);
                }
                visit_expression(iterable, names);
                visit_statement(body, names);
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                visit_expression(discriminant, names);
                for case in cases {
                    if let Some(test) = &case.test {
                        visit_expression(test, names);
                    }
                    for statement in &case.consequent {
                        visit_statement(statement, names);
                    }
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                for statement in body {
                    visit_statement(statement, names);
                }
                if let Some(catch) = catch {
                    // The catch binding declares, it does not assign.
                    for expression in catch.binding.iter().flat_map(Pattern::child_expressions) {
                        visit_expression(expression, names);
                    }
                    for statement in &catch.body {
                        visit_statement(statement, names);
                    }
                }
                for statement in finally.iter().flatten() {
                    visit_statement(statement, names);
                }
            }
            Stmt::Function { function, .. } => {
                for parameter in &function.params {
                    for expression in parameter.child_expressions() {
                        visit_expression(expression, names);
                    }
                }
                match &function.body {
                    FunctionBody::Block(statements) => {
                        for statement in statements {
                            visit_statement(statement, names);
                        }
                    }
                    FunctionBody::Expression(expression) => {
                        visit_expression(expression, names);
                    }
                }
            }
            Stmt::Empty | Stmt::Break | Stmt::Continue => {}
        }
    }
    let mut names = BTreeSet::new();
    for statement in statements {
        visit_statement(statement, &mut names);
    }
    names
}

/// The names the program's root statements declare lexically — a `let`,
/// `const`, `function` or `enum` at the cell's top level. One shadows the
/// global property of the same name, so no other binding may take it. A
/// `var` is deliberately absent: it is the global property itself.
pub(super) fn root_declaration_names(statements: &[Stmt]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for statement in statements {
        match statement.unlabeled() {
            Stmt::Var { kind, declarations } if !matches!(kind, VarKind::Var) => {
                for declaration in declarations {
                    let mut declared = Vec::new();
                    pattern_names(&declaration.pattern, &mut declared);
                    names.extend(declared);
                }
            }
            Stmt::Function { name, .. } | Stmt::Enum { name, .. } => {
                names.insert(name.clone());
            }
            _ => {}
        }
    }
    names
}

/// The source-level names this program *calls*.
///
/// A `const`-bound async arrow that is never a callee and never handed to a
/// callback method is a process-literal candidate (FIG-2997): the linker lifts
/// it where a `Process` slot asks for it. An arrow that is called — directly,
/// or by an array-callback method that would invoke it through the binding —
/// keeps today's async-helper shape, because a process value is not callable.
pub(super) fn called_binding_names(statements: &[Stmt]) -> BTreeSet<String> {
    let mut called = BTreeSet::new();
    for statement in statements {
        collect_statement_called_names(statement, &mut called);
    }
    called
}

fn collect_statement_called_names(statement: &Stmt, called: &mut BTreeSet<String>) {
    match statement {
        Stmt::Spanned(_, stmt) | Stmt::Labeled { stmt, .. } => {
            collect_statement_called_names(stmt, called)
        }
        Stmt::Empty | Stmt::Break | Stmt::Continue => {}
        Stmt::Function { function, .. } => match &function.body {
            FunctionBody::Block(statements) => {
                for statement in statements {
                    collect_statement_called_names(statement, called);
                }
            }
            FunctionBody::Expression(expression) => {
                collect_expression_called_names(expression, called);
            }
        },
        Stmt::Expr(expression) | Stmt::Throw(expression) => {
            collect_expression_called_names(expression, called);
        }
        Stmt::Return(expression) => {
            if let Some(expression) = expression {
                collect_expression_called_names(expression, called);
            }
        }
        Stmt::Block(statements) => {
            for statement in statements {
                collect_statement_called_names(statement, called);
            }
        }
        Stmt::Var { declarations, .. } => {
            for declaration in declarations {
                if let Some(initializer) = &declaration.init {
                    collect_expression_called_names(initializer, called);
                }
                for expression in declaration.pattern.child_expressions() {
                    collect_expression_called_names(expression, called);
                }
            }
        }
        Stmt::Enum { members, .. } => {
            for member in members {
                collect_expression_called_names(&member.value, called);
            }
        }
        Stmt::If {
            test,
            consequent,
            alternate,
        } => {
            collect_expression_called_names(test, called);
            collect_statement_called_names(consequent, called);
            if let Some(alternate) = alternate {
                collect_statement_called_names(alternate, called);
            }
        }
        Stmt::While { test, body } => {
            collect_expression_called_names(test, called);
            collect_statement_called_names(body, called);
        }
        Stmt::DoWhile { body, test, .. } => {
            collect_expression_called_names(test, called);
            collect_statement_called_names(body, called);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            if let Some(init) = init {
                collect_statement_called_names(init, called);
            }
            if let Some(test) = test {
                collect_expression_called_names(test, called);
            }
            if let Some(update) = update {
                collect_expression_called_names(update, called);
            }
            collect_statement_called_names(body, called);
        }
        Stmt::ForOf {
            pattern,
            iterable,
            body,
            ..
        }
        | Stmt::ForIn {
            pattern,
            object: iterable,
            body,
            ..
        } => {
            collect_expression_called_names(iterable, called);
            for expression in pattern.child_expressions() {
                collect_expression_called_names(expression, called);
            }
            collect_statement_called_names(body, called);
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            collect_expression_called_names(discriminant, called);
            for case in cases {
                if let Some(test) = &case.test {
                    collect_expression_called_names(test, called);
                }
                for statement in &case.consequent {
                    collect_statement_called_names(statement, called);
                }
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for statement in body {
                collect_statement_called_names(statement, called);
            }
            if let Some(catch) = catch {
                for expression in catch.binding.iter().flat_map(Pattern::child_expressions) {
                    collect_expression_called_names(expression, called);
                }
                for statement in &catch.body {
                    collect_statement_called_names(statement, called);
                }
            }
            if let Some(finally) = finally {
                for statement in finally {
                    collect_statement_called_names(statement, called);
                }
            }
        }
    }
}

fn collect_expression_called_names(expression: &Expr, called: &mut BTreeSet<String>) {
    match expression {
        Expr::Function(_) => {}
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name, _) = callee.as_ref() {
                called.insert(name.clone());
            }
            collect_expression_called_names(callee, called);
            let callback_invocation = matches!(
                callee.as_ref(),
                Expr::Member {
                    property: MemberProperty::Field(method),
                    ..
                } if is_callback_method(method)
            );
            for argument in args {
                let value = match argument {
                    CallArg::Value(value) => value,
                    CallArg::Spread(value) => value,
                };
                if callback_invocation && let Expr::Ident(name, _) = value {
                    called.insert(name.clone());
                }
                collect_expression_called_names(value, called);
            }
        }
        expression => {
            for child in expression.children() {
                collect_expression_called_names(child, called);
            }
        }
    }
}

fn is_callback_method(method: &str) -> bool {
    matches!(
        method,
        "map"
            | "filter"
            | "reduce"
            | "reduceRight"
            | "find"
            | "findIndex"
            | "findLast"
            | "findLastIndex"
            | "some"
            | "every"
            | "forEach"
            | "flatMap"
            | "sort"
            | "toSorted"
    )
}

impl super::Lowerer {
    /// The binding whose generated slot is `internal`, across the scope stack.
    ///
    /// Process-literal capture diagnostics need the *source* of each capture:
    /// its visible spelling, kind (mutable or not), and hold-class. Generated
    /// slot names are unique across the stack precisely where a binding is
    /// locally shadowed, so the reverse lookup answers one binding at most.
    /// Marks the binding the current scope declares under `internal`.
    pub(super) fn set_local_initialized(&mut self, internal: &str, initialized: bool) {
        if let Some(binding) = self.scopes.last_mut().and_then(|scope| {
            scope
                .bindings
                .values_mut()
                .find(|binding| binding.internal.as_str() == internal)
        }) {
            binding.initialized = initialized;
        }
    }

    pub(super) fn binding_by_internal(&self, internal: &str) -> std::option::Option<&Binding> {
        self.scopes
            .iter()
            .rev()
            .flat_map(|scope| scope.bindings.values())
            .find(|binding| binding.internal.as_str() == internal)
    }
}

/// Whether anything in the function — parameter defaults, the body, and
/// nested arrows, which bind `arguments` lexically — names the arguments
/// object the implicit `arguments` binding materializes. The scan reads
/// through nested non-arrow functions too: one that mentions `arguments`
/// binds its own, and the extra snapshot the outer binding costs here is
/// cheaper than a boundary-exact walk.
pub(super) fn function_uses_arguments(function: &Function) -> bool {
    fn uses(expr: &Expr) -> bool {
        matches!(expr, Expr::Ident(name, _) if name == "arguments")
    }
    function
        .params
        .iter()
        .flat_map(Pattern::child_expressions)
        .any(uses)
        || match &function.body {
            FunctionBody::Block(statements) => statements
                .iter()
                .flat_map(Stmt::child_expressions)
                .any(uses),
            FunctionBody::Expression(expression) => uses(expression),
        }
}

/// Whether the function's own scope declares `arguments` outright — as a
/// parameter or a hoisted `var`/`function`/`enum` name — so the implicit
/// binding must not also declare it.
pub(super) fn function_declares_arguments(function: &Function) -> bool {
    let parameter_shadows = function.params.iter().any(|pattern| {
        let mut names = Vec::new();
        pattern_names(pattern, &mut names);
        names.iter().any(|name| name == "arguments")
    });
    parameter_shadows
        || match &function.body {
            FunctionBody::Block(statements) => function_var_names(statements)
                .iter()
                .any(|name| name == "arguments"),
            FunctionBody::Expression(_) => false,
        }
}
