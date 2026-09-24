//! The scope stack's data: what a binding is, and what the lowerer knows
//! about it.
//!
//! Every per-binding fact belongs here rather than in a side table on the
//! lowerer. A table keyed by name outlives the scope that declared the
//! binding, and internal names are unique only where a binding is visibly
//! shadowed — so sibling scopes shared keys and a dead binding's facts
//! changed how a live one lowered.

use std::collections::{BTreeMap, BTreeSet};

use super::captures::BindingId;
use super::{
    BinaryOp, CallArg, Expr, FunctionBody, MemberProperty, Pattern, Stmt, TsAssignTarget,
    is_reserved_name, reserved_identifier,
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
        if matches!(name, "undefined" | "NaN" | "Infinity") {
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
        let owner_function = self.current_function();
        // A binding declared in a block of the cell's top level ends with its
        // block (ECMA-262 lexical scoping), so it never becomes a session
        // global. A function frame's locals are never globals to begin with.
        let block_private = owner_function == 0 && self.scopes.len() > self.root_scope_depth;
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
        let preserve_name = !shares_a_global_slot && (preserve_name || !self.has_binding(name));
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
        let id = self.declare_in_ledger(name, kind);
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
            let first_capturing_function = self
                .functions
                .iter()
                .position(|function| function.id == binding.owner_function)
                .map_or(0, |owner| owner + 1);
            for function in &mut self.functions[first_capturing_function..] {
                function.captures.insert(binding.internal.clone());
            }
            // The outermost capturing closure is the one the owning frame
            // creates, so its creation is when the value is copied; the
            // closures inside it copy from that copy.
            let creation = self.functions[first_capturing_function].creation.clone();
            self.capture_ledger.capture(binding.id, creation, span);
        }
        Ok(binding.internal)
    }

    /// Registers a binding with the capture ledger. A `var` (and an enum,
    /// which is one) belongs to its whole function frame, so no loop makes it
    /// fresh; every other binding is fresh in each loop around its
    /// declaration.
    pub(super) fn declare_in_ledger(&mut self, name: &str, kind: BindingKind) -> BindingId {
        let loops = match kind {
            BindingKind::Var => Vec::new(),
            _ => self.position.loops.clone(),
        };
        self.capture_ledger.declare(name, loops)
    }

    /// Records an assignment to `binding` at the current point of its frame.
    pub(super) fn record_write(&mut self, binding: BindingId) {
        let site = self.capture_ledger.site(&self.position.loops);
        self.capture_ledger.write(binding, site);
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
