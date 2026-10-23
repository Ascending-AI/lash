//! A body as Rust over the machine's plan context: each form the body
//! evaluates becomes the context call of the same name, in the body's
//! order, into the registers the body's own evaluation would use.
//! Control flow is Rust's; every value stays in the machine.
//!
//! A body that can reach guest code or the host compiles to a resumable
//! segment (FIG-6709). Each action of the body that may run guest code or a
//! body in a frame, or that performs an effect (FIG-6916), is a callback
//! site: the plan calls or performs through the context there, which may
//! yield. The code takes an entry: 0 runs from the start, and
//! `k + 1` runs on after site `k`, its frame's state restored into the
//! plan's variables and registers. Resuming, every form before the site is
//! skipped (`r` is the entry still sought; it is 0 once the site is
//! reached), and each branch and loop goes the way that leads to the site.
//! What a site records is the body frame's coordinate there: the action's
//! site path, the operands its statement holds by their site paths, the
//! variables in scope and the loops it stands in. Paths are numbered as the
//! machine's lowering numbers them, so they name the body's own nodes.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    ActKind, Action, AssignOrder, Atom, Callee, Expr, Literal, Member, Place, Rhs, Stmt,
};

use lash_vocabulary::{FunctionId, Name};

use crate::library::Library;

mod plan;
pub use plan::emit;

/// Keep NUL separate from following digits: `\0` would look like an octal
/// escape in packed text tables, even though Rust interprets it as NUL.
pub(crate) fn text_literal(text: &str) -> String {
    let mut literal = String::from("\"");
    for (index, part) in text.split('\0').enumerate() {
        if index != 0 {
            literal.push_str(r"\u{0}");
        }
        let quoted = format!("{part:?}");
        literal.push_str(&quoted[1..quoted.len() - 1]);
    }
    literal.push('"');
    literal
}

/// Why a body is not compiled: the first form the compiler does not
/// perform as the body does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported(pub String);

/// A compiled body: its plan's constant and its code.
pub struct Plan {
    pub item: String,
    pub code: String,
    /// How many callback sites it has: 0 for a closed body.
    pub sites: usize,
}

#[derive(Clone, Copy)]
enum Var {
    Param(usize),
    Slot(u32),
}

/// A loop being emitted: the scopes its body's departures leave, and
/// whether a `continue` or `break` was emitted for it.
struct Loop {
    scopes: usize,
    level: usize,
    continues: bool,
}

/// A loop a callback site may stand in.
#[derive(Clone, Copy)]
enum Enclosing {
    For { walked: u32 },
    While,
}

struct Emitter<'a> {
    library: &'a Library,
    /// The bodies whose plans answer at once: they reach no guest code.
    answering: &'a BTreeSet<FunctionId>,
    /// The body's own identity, which `brand.built()` answers.
    function: FunctionId,
    /// Whether the body compiles to a resumable segment.
    segment: bool,
    callees: Vec<FunctionId>,
    literals: Vec<String>,
    fields: Vec<String>,
    records: Vec<Vec<u32>>,
    slots: Vec<(String, Vec<u32>)>,
    closures: Vec<Vec<u32>>,
    stmt_paths: BTreeMap<*const Stmt, Vec<u32>>,
    expr_paths: BTreeMap<*const Expr, Vec<u32>>,
    /// Loop depth at each Rust closure used for protected control flow.
    protected: Vec<(usize, usize)>,
    adopted: BTreeMap<usize, u32>,
    /// Each open block's variables, innermost last; `None` for a name an
    /// expression `let` scoped away.
    scopes: Vec<Vec<(Name, Var)>>,
    loops: Vec<Loop>,
    out: String,
    indent: usize,
    used: u32,
    /// The path of the node being emitted, as the machine's lowering
    /// numbers it: a statement's under its block's, a child's under its
    /// parent's.
    path: Vec<u32>,
    /// The operands evaluated so far that an action reached now would
    /// hold: each by the depth of its parent, its child index and its
    /// register, as the lowering keeps them.
    live: Vec<(usize, u32, u32)>,
    /// The path length of the statement being emitted.
    stmt_depth: usize,
    /// Whether the forms being emitted are skipped while resuming: they
    /// stand in a statement that holds a callback site.
    guard: bool,
    /// Each callback site's `PlanSite`, as Rust.
    sites: Vec<String>,
    /// The loops the node being emitted stands in, outermost first.
    enclosing: Vec<Enclosing>,
    /// How many `for` loops and counted `while` loops enclose it.
    fors: u32,
    whiles: u32,
    /// How many branch tests have been named.
    tests: u32,
}

/// A loop's label, which its departures name: a kernel `break` or
/// `continue` leaves the innermost loop, and a Rust one names it so that
/// it may stand inside a labelled block.
fn label(level: usize) -> String {
    format!("'l{level}")
}

/// Whether `block` leaves by its end only never: a statement in it always
/// departs. What follows that statement never runs and is not emitted.
fn departs_block(block: &[Stmt]) -> bool {
    block.iter().any(departs)
}

/// Whether control always leaves by `stmt` (not as a tail).
fn departs(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return { .. } | Stmt::Throw { .. } | Stmt::Break | Stmt::Continue => true,
        Stmt::If {
            then_block,
            else_block,
            ..
        } => !else_block.is_empty() && departs_block(then_block) && departs_block(else_block),
        _ => false,
    }
}

/// The statements of `block` that can run: up to its first departing one.
fn live_stmts(block: &[Stmt]) -> &[Stmt] {
    let end = block
        .iter()
        .position(departs)
        .map_or(block.len(), |at| at + 1);
    &block[..end]
}

/// `entries` as a Rust pattern of resume entries, or `None` when empty.
fn entries(first: usize, count: u32) -> Option<String> {
    match count {
        0 => None,
        1 => Some(format!("{}", first + 1)),
        _ => Some(format!("{}..={}", first + 1, first + count as usize)),
    }
}

impl Emitter<'_> {
    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    /// A form the body performs: skipped while resuming inside a
    /// statement that holds a callback site.
    fn op(&mut self, text: &str) {
        if self.guard {
            self.line(&format!("if r == 0 {{ {text} }}"));
        } else {
            self.line(text);
        }
    }

    fn reg(&mut self, at: u32) -> u32 {
        self.used = self.used.max(at + 1);
        at
    }

    fn slot(&mut self) -> u32 {
        let slot = self.slots.len() as u32;
        self.slots.push((String::new(), Vec::new()));
        slot
    }

    fn named_slot(&mut self, name: &Name, path: Vec<u32>) -> u32 {
        let slot = self.slot();
        self.slots[slot as usize] = (name.to_string(), path);
        slot
    }

    fn child<T>(&mut self, index: u32, emit: impl FnOnce(&mut Self) -> T) -> T {
        self.path.push(index);
        let emitted = emit(self);
        self.path.pop();
        emitted
    }

    /// Whether a call of `function` answers at once: it has no body, or a
    /// plan that reaches no guest code.
    fn answers(&self, function: &FunctionId) -> bool {
        self.library
            .definition(function)
            .is_some_and(|definition| definition.body().is_none())
            || self.answering.contains(function)
    }

    fn callee(&mut self, function: &FunctionId) -> Result<u32, Unsupported> {
        let definition = self
            .library
            .definition(function)
            .ok_or_else(|| Unsupported(format!("calls {function}, which is not registered")))?;
        if !self.segment && !self.answers(function) {
            return Err(Unsupported(format!(
                "calls {}, which no plan answers",
                definition.name
            )));
        }
        if let Some(at) = self.callees.iter().position(|callee| callee == function) {
            return Ok(at as u32);
        }
        self.callees.push(*function);
        Ok(self.callees.len() as u32 - 1)
    }

    fn literal(&mut self, literal: &Literal) -> Result<u32, Unsupported> {
        let spelled = match literal {
            Literal::Null => "p::PlanLiteral::Null".to_string(),
            Literal::Absent => "p::PlanLiteral::Absent".to_string(),
            Literal::Bool(value) => format!("p::PlanLiteral::Bool({value})"),
            Literal::Int(value) => format!("p::PlanLiteral::Int({:?})", value.to_string()),
            Literal::Float(value) => {
                format!("p::PlanLiteral::Float({:#018x})", value.get().to_bits())
            }
            Literal::Text(text) => format!("p::PlanLiteral::Text({})", text_literal(text)),
            Literal::Bytes(bytes) => format!("p::PlanLiteral::Bytes(&{:?})", bytes.as_slice()),
            Literal::Function(name) => {
                return Err(Unsupported(format!("names the declared function {name}")));
            }
        };
        if let Some(at) = self.literals.iter().position(|known| *known == spelled) {
            return Ok(at as u32);
        }
        self.literals.push(spelled);
        Ok(self.literals.len() as u32 - 1)
    }

    fn field(&mut self, name: &str) -> u32 {
        if let Some(at) = self.fields.iter().position(|known| known == name) {
            return at as u32;
        }
        self.fields.push(name.to_string());
        self.fields.len() as u32 - 1
    }

    fn resolve(&self, name: &Name) -> Result<Var, Unsupported> {
        for scope in self.scopes.iter().rev() {
            for (declared, var) in scope.iter().rev() {
                if declared == name {
                    return Ok(*var);
                }
            }
        }
        Err(Unsupported(format!("reads `{name}`, which no scope binds")))
    }

    fn declare(&mut self, name: &Name, var: Var) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.push((name.clone(), var));
        }
    }

    /// Unbinds what the innermost `depth` scopes declared, as leaving
    /// their blocks empties their slots.
    fn unbind_scopes(&mut self, from: usize) {
        let slots: Vec<u32> = self.scopes[from..]
            .iter()
            .rev()
            .flat_map(|scope| {
                scope.iter().rev().filter_map(|(_, var)| match var {
                    Var::Slot(slot) => Some(*slot),
                    Var::Param(_) => None,
                })
            })
            .collect();
        for slot in slots {
            self.op(&format!("ctx.unbind({slot});"));
        }
    }

    fn read(&mut self, name: &Name, dst: u32) -> Result<(), Unsupported> {
        let dst = self.reg(dst);
        match self.resolve(name)? {
            Var::Param(index) => match self.adopted.get(&index) {
                Some(slot) => self.op(&format!("ctx.local({slot}, {dst})?;")),
                None => self.op(&format!("ctx.param({index}, {dst})?;")),
            },
            Var::Slot(slot) => self.op(&format!("ctx.local({slot}, {dst})?;")),
        }
        Ok(())
    }

    // Callback sites.

    /// How many callback sites evaluating `expr` can reach.
    fn count_expr(&self, expr: &Expr) -> u32 {
        let all = |exprs: &[Expr]| exprs.iter().map(|expr| self.count_expr(expr)).sum::<u32>();
        match expr {
            Expr::Literal(_) | Expr::Variable(_) | Expr::Closure(_) => 0,
            Expr::Tuple(items) | Expr::List(items) | Expr::Set(items) => all(items),
            Expr::Map(entries) => entries
                .iter()
                .map(|entry| self.count_expr(&entry.key) + self.count_expr(&entry.value))
                .sum(),
            Expr::Record(entries) => entries
                .iter()
                .map(|entry| self.count_expr(&entry.value))
                .sum(),
            Expr::Member(member) => match member.as_ref() {
                Member::Field { target, .. } => self.count_expr(target),
                Member::Index { target, index } => self.count_expr(target) + self.count_expr(index),
            },
            Expr::Call { function, args } => u32::from(!self.answers(function)) + all(args),
            Expr::Act(act) => {
                let own = match &act.kind {
                    ActKind::Call(Callee::Local(function)) => !self.answers(function),
                    ActKind::Call(_) | ActKind::Perform { .. } => true,
                    _ => false,
                };
                u32::from(own) + all(&act.args)
            }
            Expr::If { cond, then, r#else } => {
                self.count_expr(cond) + self.count_expr(then) + self.count_expr(r#else)
            }
            Expr::Let { value, body, .. } => self.count_expr(value) + self.count_expr(body),
        }
    }

    fn count_rhs(&self, rhs: &Rhs) -> u32 {
        match rhs {
            Rhs::Expr(expr) => self.count_expr(expr),
            Rhs::Action(Action::Call {
                callee: Callee::Local(function),
                ..
            }) => u32::from(!self.answers(function)),
            Rhs::Action(Action::Call { .. } | Action::Perform { .. }) => 1,
            Rhs::Action(_) => 0,
        }
    }

    fn count_member(&self, member: &Member) -> u32 {
        match member {
            Member::Field { target, .. } => self.count_expr(target),
            Member::Index { target, index } => self.count_expr(target) + self.count_expr(index),
        }
    }

    /// How many callback sites `block` holds where it can run.
    fn count_block(&self, block: &[Stmt]) -> u32 {
        live_stmts(block)
            .iter()
            .map(|stmt| self.count_stmt(stmt))
            .sum()
    }

    fn count_stmt(&self, stmt: &Stmt) -> u32 {
        match stmt {
            Stmt::Let { value, .. } => self.count_rhs(value),
            Stmt::Assign { place, value, .. } => {
                let place = match place {
                    Place::Variable(_) => 0,
                    Place::Member(member) => self.count_member(member),
                };
                place + self.count_rhs(value)
            }
            Stmt::Remove { member } => self.count_member(member),
            Stmt::Do { value } | Stmt::Return { value } | Stmt::Throw { value } => {
                self.count_expr(value)
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.count_expr(condition)
                    + self.count_block(then_block)
                    + self.count_block(else_block)
            }
            Stmt::While {
                condition,
                body,
                else_block,
                ..
            } => self.count_expr(condition) + self.count_block(body) + self.count_block(else_block),
            Stmt::For {
                iterable,
                body,
                else_block,
                ..
            } => self.count_expr(iterable) + self.count_block(body) + self.count_block(else_block),
            Stmt::Try(scope) => {
                self.count_block(&scope.body)
                    + scope
                        .catch
                        .as_ref()
                        .map_or(0, |catch| self.count_block(&catch.body))
                    + scope
                        .finally
                        .as_ref()
                        .map_or(0, |finally| self.count_block(finally))
            }
            _ => 0,
        }
    }

    /// The operands an action reached now holds, by their site paths.
    fn roles(&self) -> Vec<(Vec<u32>, u32)> {
        self.live
            .iter()
            .map(|&(depth, index, reg)| {
                let mut path = self.path[..depth].to_vec();
                path.push(index);
                (path, reg)
            })
            .collect()
    }

    /// Records a callback site at the node being emitted: an action that
    /// `call`s, its value to `dst`, holding `roles`. Gives its index.
    fn site(&mut self, call: String, roles: &[(Vec<u32>, u32)], dst: u32) -> usize {
        let mut bound: Vec<u32> = self
            .scopes
            .iter()
            .flatten()
            .filter_map(|(_, var)| match var {
                Var::Slot(slot) => Some(*slot),
                Var::Param(_) => None,
            })
            .chain(self.adopted.values().copied())
            .collect();
        bound.sort_unstable();
        bound.dedup();
        let roles: Vec<String> = roles
            .iter()
            .map(|(path, reg)| format!("p::PlanRole {{ path: &{path:?}, reg: {reg} }}"))
            .collect();
        let loops: Vec<String> = self
            .enclosing
            .iter()
            .map(|enclosing| match enclosing {
                Enclosing::For { walked } => format!("p::PlanLoop::For {{ walked: {walked} }}"),
                Enclosing::While => "p::PlanLoop::While".to_string(),
            })
            .collect();
        self.sites.push(format!(
            "p::PlanSite {{\n            path: &{:?},\n            call: {call},\n            dst: {dst},\n            roles: &[{}],\n            bound: &{bound:?},\n            loops: &[{}],\n        }}",
            self.path,
            roles.join(", "),
            loops.join(", "),
        ));
        self.sites.len() - 1
    }

    /// The call of a callback site: done where the run reaches it, the
    /// site itself where the run resumes after it.
    fn site_call(&mut self, site: usize, call: &str) {
        let entry = site + 1;
        self.line(&format!(
            "if r == {entry} {{ r = 0; }} else if r == 0 {{ {call} }}"
        ));
    }

    /// A call of library `function` with the `count` arguments from `at`,
    /// its value to `at`, holding `roles` when it is a callback site.
    /// Whether `function` is the machine's `brand.built` (`K-LIB-010`).
    fn brand_built(&self, function: &FunctionId) -> bool {
        self.library.definition(function).is_some_and(|definition| {
            definition.name.as_str() == "brand.built" && definition.body().is_none()
        })
    }

    fn library_call(
        &mut self,
        function: &FunctionId,
        roles: &[(Vec<u32>, u32)],
        at: u32,
        count: usize,
    ) -> Result<(), Unsupported> {
        // The machine answers `brand.built` only where it is called by name
        // in an expression; any other call of it raises, as the body does.
        if self.brand_built(function) {
            return Err(Unsupported("calls brand.built through an action".into()));
        }
        let callee = self.callee(function)?;
        if self.answers(function) {
            self.op(&format!("ctx.call({callee}, {at}, {count})?;"));
        } else {
            let site = self.site(format!("p::SiteCall::Invoke({callee})"), roles, at);
            self.site_call(
                site,
                &format!("ctx.invoke({site}, {callee}, {at}, {count})?;"),
            );
        }
        Ok(())
    }

    /// A branch test into a fresh Rust variable: the condition's value
    /// where the run reaches it, and resuming, whether the branch holds
    /// the sought site (its entries `then`).
    fn branch(&mut self, cond: u32, then: Option<String>) -> String {
        let test = format!("t{}", self.tests);
        self.tests += 1;
        let resumed = then.map_or("false".to_string(), |then| format!("matches!(r, {then})"));
        self.line(&format!(
            "let {test} = if r == 0 {{ ctx.test({cond}, p::Test::If)? }} else {{ {resumed} }};"
        ));
        test
    }

    // Expressions.

    /// Each expression into the registers from `dst` on, in order, as
    /// children 0, 1, ... of the node being emitted.
    fn exprs(&mut self, exprs: &[Expr], dst: u32) -> Result<u32, Unsupported> {
        for (index, expr) in exprs.iter().enumerate() {
            self.expr_at(index as u32, expr, dst + index as u32)?;
        }
        Ok(exprs.len() as u32)
    }

    /// Child `index` of the node being emitted, into `reg`: from then on
    /// held by any action reached before its parent ends.
    fn expr_at(&mut self, index: u32, expr: &Expr, reg: u32) -> Result<(), Unsupported> {
        let depth = self.path.len();
        self.child(index, |emitter| emitter.expr(expr, reg))?;
        self.live.push((depth, index, reg));
        Ok(())
    }

    /// A member's target into `at` and its index into the register after,
    /// its first child having index `first`; gives its field and the index
    /// after its last child.
    fn member(
        &mut self,
        member: &Member,
        first: u32,
        at: u32,
    ) -> Result<(Option<u32>, u32), Unsupported> {
        match member {
            Member::Field { target, field } => {
                self.expr_at(first, target, at)?;
                Ok((Some(self.field(field)), first + 1))
            }
            Member::Index { target, index } => {
                self.expr_at(first, target, at)?;
                self.expr_at(first + 1, index, at + 1)?;
                Ok((None, first + 2))
            }
        }
    }

    fn expr(&mut self, expr: &Expr, dst: u32) -> Result<(), Unsupported> {
        // A part of a resuming statement that holds no callback site runs
        // whole or not at all.
        if self.guard && self.count_expr(expr) == 0 {
            self.line("if r == 0 {");
            self.indent += 1;
            self.guard = false;
            let emitted = self.expr_inner(expr, dst);
            self.guard = true;
            self.indent -= 1;
            self.line("}");
            return emitted;
        }
        self.expr_inner(expr, dst)
    }

    fn expr_inner(&mut self, expr: &Expr, dst: u32) -> Result<(), Unsupported> {
        let dst = self.reg(dst);
        let live = self.live.len();
        match expr {
            Expr::Literal(literal) => {
                let literal = self.literal(literal)?;
                self.op(&format!("ctx.literal({literal}, {dst})?;"));
            }
            Expr::Variable(name) => self.read(name, dst)?,
            Expr::Tuple(items) => {
                let len = self.exprs(items, dst)?;
                self.op(&format!("ctx.tuple({dst}, {len})?;"));
            }
            Expr::List(items) => {
                let len = self.exprs(items, dst)?;
                self.op(&format!("ctx.list({dst}, {len})?;"));
            }
            Expr::Set(items) => {
                let len = self.exprs(items, dst)?;
                self.op(&format!("ctx.set({dst}, {len})?;"));
            }
            Expr::Map(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    let at = 2 * index as u32;
                    self.expr_at(at, &entry.key, dst + at)?;
                    self.expr_at(at + 1, &entry.value, dst + at + 1)?;
                }
                let len = 2 * entries.len();
                self.op(&format!("ctx.map({dst}, {len})?;"));
            }
            Expr::Record(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    self.expr_at(index as u32, &entry.value, dst + index as u32)?;
                }
                let fields: Vec<u32> = entries
                    .iter()
                    .map(|entry| self.field(&entry.field))
                    .collect();
                let record = match self.records.iter().position(|known| *known == fields) {
                    Some(at) => at,
                    None => {
                        self.records.push(fields);
                        self.records.len() - 1
                    }
                };
                self.op(&format!("ctx.record({record}, {dst})?;"));
            }
            Expr::Member(member) => match self.member(member, 0, dst)?.0 {
                Some(field) => self.op(&format!("ctx.field({dst}, {field})?;")),
                None => self.op(&format!("ctx.index({dst})?;")),
            },
            // The body's own identity, as the machine's lowering answers it.
            Expr::Call { function, .. } if self.brand_built(function) => {
                let own =
                    Literal::Bytes(lash_kernel_doc::Bytes::new(&self.function.as_bytes()[..]));
                let literal = self.literal(&own)?;
                self.op(&format!("ctx.literal({literal}, {dst})?;"));
            }
            Expr::Call { function, args } => {
                let roles = self.roles();
                let len = self.exprs(args, dst)?;
                self.library_call(function, &roles, dst, len as usize)?;
            }
            Expr::Act(act) => match &act.kind {
                ActKind::Call(Callee::Local(function)) => {
                    let roles = self.roles();
                    let len = self.exprs(&act.args, dst)?;
                    self.library_call(function, &roles, dst, len as usize)?;
                }
                ActKind::Call(Callee::Value(name)) if self.segment => {
                    let roles = self.roles();
                    // The callee comes before the arguments (`K-EVAL-004`),
                    // held by the actions in them under the action's site.
                    self.read(name, dst)?;
                    let held = self.live.len();
                    let depth = self.path.len() - 1;
                    self.live.push((depth, self.path[depth], dst));
                    let len = self.exprs(&act.args, dst + 1)?;
                    self.live.truncate(held);
                    let site = self.site("p::SiteCall::Apply".into(), &roles, dst);
                    self.site_call(site, &format!("ctx.apply({site}, {dst}, {len})?;"));
                }
                // The body's frame waits in its `perform` here: the effect
                // and its result type are the body's (`K-EFF-001`).
                ActKind::Perform { .. } if self.segment => {
                    let roles = self.roles();
                    let len = self.exprs(&act.args, dst)?;
                    let site = self.site("p::SiteCall::Perform".into(), &roles, dst);
                    self.site_call(site, &format!("ctx.perform({site}, {dst}, {len})?;"));
                }
                _ => {
                    return Err(Unsupported(
                        "nests an action that is not a library call".into(),
                    ));
                }
            },
            Expr::If { cond, then, r#else } => {
                self.child(0, |emitter| emitter.expr(cond, dst))?;
                if self.guard {
                    let first = self.sites.len();
                    let test = self.branch(dst, entries(first, self.count_expr(then)));
                    self.line(&format!("if {test} {{"));
                } else {
                    self.line(&format!("if ctx.test({dst}, p::Test::If)? {{"));
                }
                self.indent += 1;
                self.child(1, |emitter| emitter.expr(then, dst))?;
                self.indent -= 1;
                self.line("} else {");
                self.indent += 1;
                self.child(2, |emitter| emitter.expr(r#else, dst))?;
                self.indent -= 1;
                self.line("}");
            }
            Expr::Let { name, value, body } => {
                self.child(0, |emitter| emitter.expr(value, dst))?;
                let slot =
                    self.named_slot(name, self.expr_paths[&std::ptr::from_ref(expr)].clone());
                self.op(&format!("ctx.bind({slot}, {dst})?;"));
                self.scopes.push(vec![(name.clone(), Var::Slot(slot))]);
                self.child(1, |emitter| emitter.expr(body, dst))?;
                self.scopes.pop();
                self.op(&format!("ctx.unbind({slot});"));
            }
            Expr::Closure(_) => {
                let at = self.closures.len();
                self.closures
                    .push(self.expr_paths[&std::ptr::from_ref(expr)].clone());
                self.op(&format!("ctx.closure({at}, {dst})?;"));
            }
        }
        self.live.truncate(live);
        Ok(())
    }

    fn atom(&mut self, atom: &Atom, dst: u32) -> Result<(), Unsupported> {
        match atom {
            Atom::Variable(name) => self.read(name, dst),
            Atom::Literal(literal) => {
                let dst = self.reg(dst);
                let literal = self.literal(literal)?;
                self.op(&format!("ctx.literal({literal}, {dst})?;"));
                Ok(())
            }
        }
    }

    /// A right-hand side into register 0, as child 0 of its statement.
    fn rhs(&mut self, rhs: &Rhs) -> Result<(), Unsupported> {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, 0),
            Rhs::Action(Action::Call {
                callee: Callee::Local(function),
                args,
            }) => {
                self.reg(0);
                for (index, atom) in args.iter().enumerate() {
                    self.atom(atom, index as u32)?;
                }
                self.library_call(function, &[], 0, args.len())
            }
            Rhs::Action(Action::Call {
                callee: Callee::Value(name),
                args,
            }) if self.segment => {
                self.read(name, 0)?;
                for (index, atom) in args.iter().enumerate() {
                    self.atom(atom, index as u32 + 1)?;
                }
                let len = args.len();
                let site = self.site("p::SiteCall::Apply".into(), &[], 0);
                self.site_call(site, &format!("ctx.apply({site}, 0, {len})?;"));
                Ok(())
            }
            Rhs::Action(Action::Perform { args, .. }) if self.segment => {
                self.reg(0);
                for (index, atom) in args.iter().enumerate() {
                    self.atom(atom, index as u32)?;
                }
                let len = args.len();
                let site = self.site("p::SiteCall::Perform".into(), &[], 0);
                self.site_call(site, &format!("ctx.perform({site}, 0, {len})?;"));
                Ok(())
            }
            Rhs::Action(_) => Err(Unsupported("acts other than by a library call".into())),
        }
    }

    // Statements.

    /// Emits `block`, with `bound` declared first. A tail block's
    /// fallthrough is the function's end. Gives whether control leaves
    /// the block other than by its end.
    fn block(
        &mut self,
        block: &[Stmt],
        bound: Vec<(Name, Var)>,
        tail: bool,
    ) -> Result<bool, Unsupported> {
        self.scopes.push(bound);
        // What follows a statement that always departs never runs.
        let block = live_stmts(block);
        for (index, stmt) in block.iter().enumerate() {
            let last = index + 1 == block.len();
            let departed = self.child(index as u32, |emitter| {
                emitter.guarded_stmt(stmt, tail && last)
            })?;
            if departed {
                self.scopes.pop();
                return Ok(true);
            }
        }
        if tail {
            // The body's end returns null (`K-FN-003`).
            self.line("ctx.begin()?;");
            self.line("Ok(Value::Null)");
            self.scopes.pop();
            return Ok(true);
        }
        let from = self.scopes.len() - 1;
        self.unbind_scopes(from);
        self.scopes.pop();
        Ok(false)
    }

    /// One statement of a block. Resuming, a statement that holds no
    /// callback site is skipped whole; one that holds one runs only when
    /// the sought site is in it.
    fn guarded_stmt(&mut self, stmt: &Stmt, tail: bool) -> Result<bool, Unsupported> {
        if !self.guard {
            return self.stmt(stmt, tail);
        }
        let first = self.sites.len();
        let wrapper = match entries(first, self.count_stmt(stmt)) {
            None => {
                self.guard = false;
                "if r == 0 {".to_string()
            }
            Some(sought) => format!("if r == 0 || matches!(r, {sought}) {{"),
        };
        self.line(&wrapper);
        self.indent += 1;
        let departed = self.stmt(stmt, tail);
        self.indent -= 1;
        self.line("}");
        self.guard = true;
        departed
    }

    /// Emits one statement, at the path of the node being emitted; gives
    /// whether control leaves by it.
    fn stmt(&mut self, stmt: &Stmt, tail: bool) -> Result<bool, Unsupported> {
        let outer = std::mem::replace(&mut self.stmt_depth, self.path.len());
        let live = self.live.len();
        let departed = self.stmt_inner(stmt, tail);
        self.live.truncate(live);
        self.stmt_depth = outer;
        departed
    }

    fn stmt_inner(&mut self, stmt: &Stmt, tail: bool) -> Result<bool, Unsupported> {
        self.op("ctx.begin()?;");
        match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                self.child(0, |emitter| emitter.rhs(value))?;
                let slot =
                    self.named_slot(name, self.stmt_paths[&std::ptr::from_ref(stmt)].clone());
                self.op(&format!("ctx.bind({slot}, 0)?;"));
                self.declare(name, Var::Slot(slot));
            }
            Stmt::Assign {
                place: Place::Variable(name),
                value,
                ..
            } => {
                self.child(0, |emitter| emitter.rhs(value))?;
                let slot = match self.resolve(name)? {
                    Var::Slot(slot) => slot,
                    Var::Param(index) => *self
                        .adopted
                        .get(&index)
                        .ok_or_else(|| Unsupported(format!("assigns `{name}` unadopted")))?,
                };
                self.op(&format!("ctx.assign({slot}, 0)?;"));
            }
            Stmt::Assign {
                place: Place::Member(member),
                value: Rhs::Expr(value),
                order,
            } => match order {
                // The right-hand side first, held while the place is
                // evaluated after it (`K-EVAL-003`).
                AssignOrder::ValueFirst => {
                    let value_index = match member {
                        Member::Field { .. } => 1,
                        Member::Index { .. } => 2,
                    };
                    self.child(value_index, |emitter| emitter.expr(value, 0))?;
                    self.op("ctx.pin(0)?;");
                    self.live.push((self.stmt_depth, value_index, 0));
                    match self.member(member, 0, 1)?.0 {
                        Some(field) => self.op(&format!("ctx.set_field(1, {field}, 0)?;")),
                        None => self.op("ctx.set_index(1, 0)?;"),
                    }
                }
                AssignOrder::PlaceFirst => {
                    let (field, next) = self.member(member, 0, 0)?;
                    let src = next;
                    self.child(next, |emitter| emitter.expr(value, src))?;
                    match field {
                        Some(field) => self.op(&format!("ctx.set_field(0, {field}, {src})?;")),
                        None => self.op(&format!("ctx.set_index(0, {src})?;")),
                    }
                }
            },
            // An action's value comes first, held while the place is
            // evaluated after it (`K-FORM-005`, `K-FORM-006`).
            Stmt::Assign {
                place: Place::Member(member),
                value: value @ Rhs::Action(_),
                ..
            } => {
                let value_index = match member {
                    Member::Field { .. } => 1,
                    Member::Index { .. } => 2,
                };
                self.child(value_index, |emitter| emitter.rhs(value))?;
                self.op("ctx.pin(0)?;");
                self.live.push((self.stmt_depth, value_index, 0));
                match self.member(member, 0, 1)?.0 {
                    Some(field) => self.op(&format!("ctx.set_field(1, {field}, 0)?;")),
                    None => self.op("ctx.set_index(1, 0)?;"),
                }
            }
            Stmt::Remove { member } => match self.member(member, 0, 0)?.0 {
                Some(field) => self.op(&format!("ctx.remove_field(0, {field})?;")),
                None => self.op("ctx.remove_index(0)?;"),
            },
            Stmt::Do { value } => self.child(0, |emitter| emitter.expr(value, 0))?,
            Stmt::Return { value } => {
                self.child(0, |emitter| emitter.expr(value, 0))?;
                if !self.protected.is_empty() {
                    self.op("return Ok(Departure::Return(ctx.take(0)));");
                } else if tail {
                    self.line("Ok(ctx.take(0))");
                } else {
                    self.op("return Ok(ctx.take(0));");
                }
                return Ok(true);
            }
            Stmt::Throw { value } => {
                self.child(0, |emitter| emitter.expr(value, 0))?;
                if tail {
                    self.line("Err(p::PlanExit::Raise(ctx.take(0)))");
                } else {
                    self.op("return Err(p::PlanExit::Raise(ctx.take(0)));");
                }
                return Ok(true);
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.child(0, |emitter| emitter.expr(condition, 0))?;
                if self.guard {
                    let first = self.sites.len();
                    let test = self.branch(0, entries(first, self.count_block(then_block)));
                    self.line(&format!("if {test} {{"));
                } else {
                    self.line("if ctx.test(0, p::Test::If)? {");
                }
                self.indent += 1;
                let then = self.child(1, |emitter| emitter.block(then_block, Vec::new(), tail))?;
                self.indent -= 1;
                if tail || !else_block.is_empty() {
                    self.line("} else {");
                    self.indent += 1;
                    let otherwise =
                        self.child(2, |emitter| emitter.block(else_block, Vec::new(), tail))?;
                    self.indent -= 1;
                    self.line("}");
                    return Ok(then && otherwise);
                }
                self.line("}");
            }
            Stmt::While {
                metered,
                condition,
                body,
                else_block,
            } => {
                if self.guard {
                    return self.resumable_while(*metered, condition, body, else_block);
                }
                return self.looped(*metered, None, body, else_block, |emitter| {
                    emitter.child(0, |emitter| emitter.expr(condition, 0))?;
                    Ok("ctx.test(0, p::Test::While)?".to_string())
                });
            }
            Stmt::For {
                metered,
                binding,
                iterable,
                via: None,
                body,
                else_block,
            } => {
                self.child(0, |emitter| emitter.expr(iterable, 0))?;
                if self.guard {
                    return self.resumable_for(*metered, binding, body, else_block);
                }
                let walked = self.slot();
                let level = self.loops.len();
                self.line(&format!("let c{level} = ctx.open(0, {walked})?;"));
                let mut declared = self.stmt_paths[&std::ptr::from_ref(stmt)].clone();
                declared.push(1);
                let bound = self.named_slot(binding, declared);
                return self.looped(*metered, Some((binding, bound)), body, else_block, |_| {
                    Ok(format!("ctx.next(c{level}, 0)?"))
                });
            }
            Stmt::Break | Stmt::Continue => {
                let Some(innermost) = self.loops.last_mut() else {
                    return Err(Unsupported("departs from no loop".into()));
                };
                let (scopes, level) = (innermost.scopes, innermost.level);
                let continues = matches!(stmt, Stmt::Continue);
                innermost.continues |= continues;
                let crossing = self
                    .protected
                    .last()
                    .filter(|(depth, _)| level < *depth)
                    .copied();
                self.unbind_scopes(crossing.map_or(scopes, |(_, scopes)| scopes));
                let label = label(level);
                if crossing.is_some() {
                    let departure = if continues { "Continue" } else { "Break" };
                    self.op(&format!("return Ok(Departure::{departure});"));
                } else if continues {
                    self.op(&format!("continue {label};"));
                } else {
                    self.op(&format!("break {label};"));
                }
                return Ok(true);
            }
            Stmt::For { .. } => return Err(Unsupported("iterates through a protocol".into())),
            Stmt::Try(scope) => {
                // A segment stands in a `try`'s body or its handler as the
                // body's frame does; a `finally` it does not enter.
                if self.guard
                    && scope
                        .finally
                        .as_ref()
                        .is_some_and(|finally| self.count_block(finally) != 0)
                {
                    return Err(Unsupported("has a callback site in a `finally`".into()));
                }
                let path = self.stmt_paths[&std::ptr::from_ref(stmt)].clone();
                self.line("{");
                self.indent += 1;
                let held = self.slot();
                self.child(0, |emitter| {
                    emitter.protected_block(&scope.body, Vec::new(), held)
                })?;
                if let Some(catch) = &scope.catch {
                    // Resuming in the handler, the body raised before.
                    let first = self.sites.len();
                    match entries(first, self.count_block(&catch.body)) {
                        Some(sought) if self.guard => self.line(&format!(
                            "if matches!(r, {sought}) || matches!(outcome, Err(p::PlanExit::Raise(_))) {{"
                        )),
                        _ => self.line("if matches!(outcome, Err(p::PlanExit::Raise(_))) {"),
                    }
                    self.indent += 1;
                    let mut declared = path.clone();
                    declared.push(1);
                    let slot = self.named_slot(&catch.binding, declared);
                    self.line("if let Err(p::PlanExit::Raise(value)) = outcome {");
                    self.line(&format!("    ctx.retain(value, {slot})?;"));
                    self.line("}");
                    self.line("outcome = {");
                    self.indent += 1;
                    self.child(1, |emitter| {
                        emitter.protected_block(
                            &catch.body,
                            vec![(catch.binding.clone(), Var::Slot(slot))],
                            held,
                        )
                    })?;
                    self.line("outcome");
                    self.indent -= 1;
                    self.line("};");
                    self.indent -= 1;
                    self.line("}");
                }
                if let Some(finally) = &scope.finally {
                    // Halts bypass handlers and cleanup. Only guest departures
                    // run finally, whose own departure replaces the pending one.
                    self.line("if matches!(outcome, Err(p::PlanExit::Halt)) { return Err(p::PlanExit::Halt); }");
                    let cleanup_held = self.slot();
                    self.line("let cleanup = {");
                    self.indent += 1;
                    let at = 1 + u32::from(scope.catch.is_some());
                    self.child(at, |emitter| {
                        emitter.protected_block(finally, Vec::new(), cleanup_held)
                    })?;
                    self.line("outcome");
                    self.indent -= 1;
                    self.line("};");
                    self.line("if !matches!(cleanup, Ok(Departure::Next)) { outcome = cleanup; }");
                    self.line(&format!("ctx.unbind({cleanup_held});"));
                }
                self.line(&format!("ctx.unbind({held});"));
                self.line("match outcome {");
                self.indent += 1;
                self.line("Ok(Departure::Next) => {},");
                self.line("Err(exit) => return Err(exit),");
                if self.protected.is_empty() {
                    self.line("Ok(Departure::Return(value)) => return Ok(value),");
                } else {
                    self.line(
                        "Ok(Departure::Return(value)) => return Ok(Departure::Return(value)),",
                    );
                }
                for (variant, keyword) in [("Break", "break"), ("Continue", "continue")] {
                    let action = match self.loops.last() {
                        Some(loop_)
                            if self
                                .protected
                                .last()
                                .is_none_or(|(depth, _)| loop_.level >= *depth) =>
                        {
                            format!("{keyword} {}", label(loop_.level))
                        }
                        Some(_) => format!("return Ok(Departure::{variant})"),
                        None => "unreachable!(\"a loop departure needs a loop\")".to_string(),
                    };
                    self.line(&format!("Ok(Departure::{variant}) => {{"));
                    self.indent += 1;
                    if let Some(loop_) = self.loops.last() {
                        let from = self
                            .protected
                            .last()
                            .filter(|(depth, _)| loop_.level < *depth)
                            .map_or(loop_.scopes, |(_, scopes)| *scopes);
                        self.unbind_scopes(from);
                    }
                    self.line(&format!("{action};"));
                    self.indent -= 1;
                    self.line("},");
                }
                self.indent -= 1;
                self.line("}");
                self.indent -= 1;
                self.line("}");
            }
            Stmt::ReturnVia { .. } => return Err(Unsupported("returns through a policy".into())),
            Stmt::Print { .. } | Stmt::Finish { .. } | Stmt::Fail { .. } => {
                return Err(Unsupported("prints or ends the run".into()));
            }
        }
        Ok(false)
    }

    /// A handler body executes in a Rust closure so `?` only leaves that
    /// body. Canonical payloads live in a machine slot across every finally
    /// statement; the Rust `outcome` is control, never the only value root.
    fn protected_block(
        &mut self,
        body: &[Stmt],
        bound: Vec<(Name, Var)>,
        held: u32,
    ) -> Result<(), Unsupported> {
        // Resumed inside, the loops the block opened are open again: its
        // scope is the loops around it.
        if self.guard {
            self.line(&format!("let cursor_scope = {};", self.fors));
        } else {
            self.line("let cursor_scope = ctx.scope();");
        }
        self.line(
            "let mut outcome = (|ctx: &mut dyn p::PlanContext| -> Result<Departure, p::PlanExit> {",
        );
        self.indent += 1;
        let first = self.slots.len();
        let bound_slots: Vec<u32> = bound
            .iter()
            .filter_map(|(_, var)| {
                if let Var::Slot(slot) = var {
                    Some(*slot)
                } else {
                    None
                }
            })
            .collect();
        self.protected.push((self.loops.len(), self.scopes.len()));
        let departs = self.block(body, bound, false)?;
        self.protected.pop();
        // A segment's statements run only from their entry: past a
        // departure, a resume elsewhere falls through.
        if !departs || self.guard {
            self.line("Ok(Departure::Next)");
        }
        self.indent -= 1;
        self.line("})(ctx);");
        if self.guard {
            // A yield leaves the plan; the frame keeps the block's state.
            self.line(
                "if matches!(outcome, Err(p::PlanExit::Yield)) { return Err(p::PlanExit::Yield); }",
            );
        }
        self.line("outcome = match outcome {");
        self.indent += 1;
        self.line(&format!(
            "Ok(Departure::Return(value)) => Ok(Departure::Return(ctx.retain(value, {held})?)),"
        ));
        self.line(&format!(
            "Err(p::PlanExit::Raise(value)) => Err(p::PlanExit::Raise(ctx.retain(value, {held})?)),"
        ));
        self.line("other => other,");
        self.indent -= 1;
        self.line("};");
        for slot in bound_slots
            .into_iter()
            .chain(first as u32..self.slots.len() as u32)
        {
            self.line(&format!("ctx.unbind({slot});"));
        }
        self.line("ctx.unwind(cursor_scope);");
        Ok(())
    }

    /// A `while` or a `for` whose header the caller began: each test
    /// charges the loop's declared units, then `test` gives whether it
    /// goes on. A `for` binds its element first.
    fn looped(
        &mut self,
        metered: u64,
        binding: Option<(&Name, u32)>,
        body: &[Stmt],
        else_block: &[Stmt],
        test: impl FnOnce(&mut Self) -> Result<String, Unsupported>,
    ) -> Result<bool, Unsupported> {
        let level = self.loops.len();
        let label = label(level);
        let exhausted = format!("ended{level}");
        let outer = std::mem::take(&mut self.out);
        let indent = std::mem::replace(&mut self.indent, 1);
        if metered != 0 {
            self.line(&format!("ctx.meter({metered})?;"));
        }
        let goes_on = test(self)?;
        self.line(&format!("if !{goes_on} {{"));
        if !else_block.is_empty() {
            self.line(&format!("    {exhausted} = true;"));
        }
        self.line(&format!("    break {label};"));
        self.line("}");
        let mut bound = Vec::new();
        if let Some((name, slot)) = binding {
            self.line(&format!("ctx.bind({slot}, 0)?;"));
            bound.push((name.clone(), Var::Slot(slot)));
        }
        self.loops.push(Loop {
            scopes: self.scopes.len(),
            level,
            continues: false,
        });
        let departs = self.child(1, |emitter| emitter.block(body, bound, false))?;
        let continues = self
            .loops
            .pop()
            .is_some_and(|innermost| innermost.continues);
        if !departs {
            // The next test runs in a step of its own.
            self.line("ctx.begin()?;");
        }
        let inner = std::mem::replace(&mut self.out, outer);
        self.indent = indent;
        if !else_block.is_empty() {
            self.line(&format!("let mut {exhausted} = false;"));
        }
        // A body that always leaves the loop runs at most once: a labelled
        // block, where Rust would see a loop that never loops.
        if departs && !continues {
            self.line(&format!("{label}: {{"));
        } else {
            self.line(&format!("{label}: loop {{"));
        }
        for line in inner.lines() {
            if !line.is_empty() {
                self.out.push_str(&"    ".repeat(indent));
            }
            self.out.push_str(line);
            self.out.push('\n');
        }
        self.line("}");
        if binding.is_some() {
            self.line(&format!("ctx.close(c{level});"));
        }
        if !else_block.is_empty() {
            self.line(&format!("if {exhausted} {{"));
            self.indent += 1;
            self.child(2, |emitter| emitter.block(else_block, Vec::new(), false))?;
            self.indent -= 1;
            self.line("}");
        }
        Ok(false)
    }

    /// The head of a resumable loop: its declared charge where the run
    /// reaches it, or, resuming, leaving it when the sought site is not
    /// in it (its entries `inner`).
    fn resumable_head(&mut self, metered: u64, label: &str, inner: Option<String>) {
        let leave = match inner {
            Some(inner) => format!("!matches!(r, {inner})"),
            None => "true".to_string(),
        };
        if metered != 0 {
            self.line(&format!(
                "if r == 0 {{ ctx.meter({metered})?; }} else if {leave} {{ break {label}; }}"
            ));
        } else {
            self.line(&format!("if r != 0 && {leave} {{ break {label}; }}"));
        }
    }

    /// A loop's body and `else` once its head is emitted, closed by
    /// `close` after the loop.
    fn resumable_rest(
        &mut self,
        level: usize,
        bound: Vec<(Name, Var)>,
        body: &[Stmt],
        else_block: &[Stmt],
        close: &str,
    ) -> Result<bool, Unsupported> {
        self.loops.push(Loop {
            scopes: self.scopes.len(),
            level,
            continues: false,
        });
        self.child(1, |emitter| emitter.block(body, bound, false))?;
        self.loops.pop();
        // The next test runs in a step of its own.
        self.op("ctx.begin()?;");
        self.indent -= 1;
        self.line("}");
        self.line(close);
        if !else_block.is_empty() {
            let first = self.sites.len();
            let resumed = entries(first, self.count_block(else_block))
                .map_or(String::new(), |sought| format!(" || matches!(r, {sought})"));
            self.line(&format!("if ended{level}{resumed} {{"));
            self.indent += 1;
            self.child(2, |emitter| emitter.block(else_block, Vec::new(), false))?;
            self.indent -= 1;
            self.line("}");
        }
        Ok(false)
    }

    /// A `while` that holds a callback site: its passes are counted, as
    /// the body's frame counts them.
    fn resumable_while(
        &mut self,
        metered: u64,
        condition: &Expr,
        body: &[Stmt],
        else_block: &[Stmt],
    ) -> Result<bool, Unsupported> {
        let level = self.loops.len();
        let label = label(level);
        let first = self.sites.len();
        let inner = entries(first, self.count_expr(condition) + self.count_block(body));
        if !else_block.is_empty() {
            self.line(&format!("let mut ended{level} = false;"));
        }
        self.op("ctx.enter_while();");
        let depth = self.whiles;
        self.whiles += 1;
        self.enclosing.push(Enclosing::While);
        self.line(&format!("{label}: loop {{"));
        self.indent += 1;
        self.resumable_head(metered, &label, inner);
        self.child(0, |emitter| emitter.expr(condition, 0))?;
        let ended = if else_block.is_empty() {
            String::new()
        } else {
            format!("ended{level} = true; ")
        };
        self.line(&format!(
            "if r == 0 {{ if !ctx.test(0, p::Test::While)? {{ {ended}break {label}; }} ctx.pass_while(); }}"
        ));
        let rest = self.resumable_rest(
            level,
            Vec::new(),
            body,
            &[],
            &format!("ctx.leave_while({depth});"),
        );
        self.enclosing.pop();
        self.whiles = depth;
        rest?;
        if !else_block.is_empty() {
            let first = self.sites.len();
            let resumed = entries(first, self.count_block(else_block))
                .map_or(String::new(), |sought| format!(" || matches!(r, {sought})"));
            self.line(&format!("if ended{level}{resumed} {{"));
            self.indent += 1;
            self.child(2, |emitter| emitter.block(else_block, Vec::new(), false))?;
            self.indent -= 1;
            self.line("}");
        }
        Ok(false)
    }

    /// A `for` that holds a callback site, its iterable in register 0.
    fn resumable_for(
        &mut self,
        metered: u64,
        binding: &Name,
        body: &[Stmt],
        else_block: &[Stmt],
    ) -> Result<bool, Unsupported> {
        let level = self.loops.len();
        let label = label(level);
        let walked = self.slot();
        let mut declared = self.path.clone();
        declared.push(1);
        let bound = self.named_slot(binding, declared);
        // Resuming inside it, the loop's cursor is the frame's.
        let open = self.fors;
        self.line(&format!(
            "let c{level} = if r == 0 {{ ctx.open(0, {walked})? }} else {{ p::Cursor({open}) }};"
        ));
        if !else_block.is_empty() {
            self.line(&format!("let mut ended{level} = false;"));
        }
        let first = self.sites.len();
        let inner = entries(first, self.count_block(body));
        self.fors += 1;
        self.enclosing.push(Enclosing::For { walked });
        self.line(&format!("{label}: loop {{"));
        self.indent += 1;
        let leave = match inner {
            Some(inner) => format!("!matches!(r, {inner})"),
            None => "true".to_string(),
        };
        let meter = if metered == 0 {
            String::new()
        } else {
            format!("ctx.meter({metered})?; ")
        };
        let ended = if else_block.is_empty() {
            String::new()
        } else {
            format!("ended{level} = true; ")
        };
        self.line(&format!(
            "if r == 0 {{ {meter}if !ctx.next(c{level}, 0)? {{ {ended}break {label}; }} ctx.bind({bound}, 0)?; }} else if {leave} {{ break {label}; }}"
        ));
        let rest = self.resumable_rest(
            level,
            vec![(binding.clone(), Var::Slot(bound))],
            body,
            else_block,
            &format!("ctx.close(c{level});"),
        );
        self.enclosing.pop();
        self.fors -= 1;
        rest
    }
}
