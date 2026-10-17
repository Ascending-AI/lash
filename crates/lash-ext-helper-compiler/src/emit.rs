//! A closed body as Rust over the machine's plan context: each form the
//! body evaluates becomes the context call of the same name, in the
//! body's order, into the registers the body's own evaluation would use.
//! Control flow is Rust's; every value stays in the machine.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use lash_kernel_doc::{
    ActKind, Action, AssignOrder, Atom, Callee, Expr, FunctionDefinition, FunctionId, Literal,
    Member, Name, Place, Rhs, Stmt,
};

use crate::library::Library;

/// Why a body is not compiled: the first form the compiler does not
/// perform as the body does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported(pub String);

/// A compiled body: its plan's static and its code.
pub struct Plan {
    pub item: String,
    pub code: String,
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

struct Emitter<'a> {
    library: &'a Library,
    planned: &'a BTreeSet<FunctionId>,
    callees: Vec<FunctionId>,
    literals: Vec<String>,
    fields: Vec<String>,
    records: Vec<Vec<u32>>,
    slots: u32,
    adopted: BTreeMap<usize, u32>,
    /// Each open block's variables, innermost last; `None` for a name an
    /// expression `let` scoped away.
    scopes: Vec<Vec<(Name, Var)>>,
    loops: Vec<Loop>,
    out: String,
    indent: usize,
    used: u32,
}

/// A loop's label, which its departures name: a kernel `break` or
/// `continue` leaves the innermost loop, and a Rust one names it so that
/// it may stand inside a labelled block.
fn label(level: usize) -> String {
    format!("'l{level}")
}

impl Emitter<'_> {
    fn line(&mut self, text: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn reg(&mut self, at: u32) -> u32 {
        self.used = self.used.max(at + 1);
        at
    }

    fn slot(&mut self) -> u32 {
        self.slots += 1;
        self.slots - 1
    }

    fn callee(&mut self, function: &FunctionId) -> Result<u32, Unsupported> {
        let definition = self
            .library
            .definition(function)
            .ok_or_else(|| Unsupported(format!("calls {function}, which is not registered")))?;
        if definition.body().is_some() && !self.planned.contains(function) {
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
            Literal::Text(text) => format!("p::PlanLiteral::Text({text:?})"),
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
            self.line(&format!("ctx.unbind({slot});"));
        }
    }

    fn read(&mut self, name: &Name, dst: u32) -> Result<(), Unsupported> {
        let dst = self.reg(dst);
        match self.resolve(name)? {
            Var::Param(index) => match self.adopted.get(&index) {
                Some(slot) => self.line(&format!("ctx.local({slot}, {dst})?;")),
                None => self.line(&format!("ctx.param({index}, {dst})?;")),
            },
            Var::Slot(slot) => self.line(&format!("ctx.local({slot}, {dst})?;")),
        }
        Ok(())
    }

    fn exprs(&mut self, exprs: &[Expr], dst: u32) -> Result<u32, Unsupported> {
        for (index, expr) in exprs.iter().enumerate() {
            self.expr(expr, dst + index as u32)?;
        }
        Ok(exprs.len() as u32)
    }

    fn member(&mut self, member: &Member, at: u32) -> Result<Option<u32>, Unsupported> {
        match member {
            Member::Field { target, field } => {
                self.expr(target, at)?;
                Ok(Some(self.field(field)))
            }
            Member::Index { target, index } => {
                self.expr(target, at)?;
                self.expr(index, at + 1)?;
                Ok(None)
            }
        }
    }

    fn expr(&mut self, expr: &Expr, dst: u32) -> Result<(), Unsupported> {
        let dst = self.reg(dst);
        match expr {
            Expr::Literal(literal) => {
                let literal = self.literal(literal)?;
                self.line(&format!("ctx.literal({literal}, {dst})?;"));
            }
            Expr::Variable(name) => self.read(name, dst)?,
            Expr::Tuple(items) => {
                let len = self.exprs(items, dst)?;
                self.line(&format!("ctx.tuple({dst}, {len})?;"));
            }
            Expr::List(items) => {
                let len = self.exprs(items, dst)?;
                self.line(&format!("ctx.list({dst}, {len})?;"));
            }
            Expr::Set(items) => {
                let len = self.exprs(items, dst)?;
                self.line(&format!("ctx.set({dst}, {len})?;"));
            }
            Expr::Map(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    let at = dst + 2 * index as u32;
                    self.expr(&entry.key, at)?;
                    self.expr(&entry.value, at + 1)?;
                }
                let len = 2 * entries.len();
                self.line(&format!("ctx.map({dst}, {len})?;"));
            }
            Expr::Record(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    self.expr(&entry.value, dst + index as u32)?;
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
                self.line(&format!("ctx.record({record}, {dst})?;"));
            }
            Expr::Member(member) => match self.member(member, dst)? {
                Some(field) => self.line(&format!("ctx.field({dst}, {field})?;")),
                None => self.line(&format!("ctx.index({dst})?;")),
            },
            Expr::Call { function, args } => {
                let callee = self.callee(function)?;
                let len = self.exprs(args, dst)?;
                self.line(&format!("ctx.call({callee}, {dst}, {len})?;"));
            }
            Expr::Act(act) => match &act.kind {
                ActKind::Call(Callee::Library(function)) => {
                    let callee = self.callee(function)?;
                    let len = self.exprs(&act.args, dst)?;
                    self.line(&format!("ctx.call({callee}, {dst}, {len})?;"));
                }
                _ => {
                    return Err(Unsupported(
                        "nests an action that is not a library call".into(),
                    ));
                }
            },
            Expr::If { cond, then, r#else } => {
                self.expr(cond, dst)?;
                self.line(&format!("if ctx.test({dst}, p::Test::If)? {{"));
                self.indent += 1;
                self.expr(then, dst)?;
                self.indent -= 1;
                self.line("} else {");
                self.indent += 1;
                self.expr(r#else, dst)?;
                self.indent -= 1;
                self.line("}");
            }
            Expr::Let { name, value, body } => {
                self.expr(value, dst)?;
                let slot = self.slot();
                self.line(&format!("ctx.bind({slot}, {dst})?;"));
                self.scopes.push(vec![(name.clone(), Var::Slot(slot))]);
                self.expr(body, dst)?;
                self.scopes.pop();
                self.line(&format!("ctx.unbind({slot});"));
            }
            Expr::Closure(_) => return Err(Unsupported("makes a closure".into())),
            Expr::Clock | Expr::Random | Expr::Read(_) => {
                return Err(Unsupported("reads the host".into()));
            }
        }
        Ok(())
    }

    fn atom(&mut self, atom: &Atom, dst: u32) -> Result<(), Unsupported> {
        match atom {
            Atom::Variable(name) => self.read(name, dst),
            Atom::Literal(literal) => {
                let dst = self.reg(dst);
                let literal = self.literal(literal)?;
                self.line(&format!("ctx.literal({literal}, {dst})?;"));
                Ok(())
            }
        }
    }

    /// A right-hand side into register 0.
    fn rhs(&mut self, rhs: &Rhs) -> Result<(), Unsupported> {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, 0),
            Rhs::Action(Action::Call {
                callee: Callee::Library(function),
                args,
            }) => {
                let callee = self.callee(function)?;
                self.reg(0);
                for (index, atom) in args.iter().enumerate() {
                    self.atom(atom, index as u32)?;
                }
                let len = args.len();
                self.line(&format!("ctx.call({callee}, 0, {len})?;"));
                Ok(())
            }
            Rhs::Action(_) => Err(Unsupported("acts other than by a library call".into())),
        }
    }

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
        let end = block
            .iter()
            .position(|stmt| {
                matches!(
                    stmt,
                    Stmt::Return { .. } | Stmt::Throw { .. } | Stmt::Break | Stmt::Continue
                )
            })
            .map_or(block.len(), |at| at + 1);
        let block = &block[..end];
        for (index, stmt) in block.iter().enumerate() {
            let last = index + 1 == block.len();
            if self.stmt(stmt, tail && last)? {
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

    /// Emits one statement; gives whether control leaves by it.
    fn stmt(&mut self, stmt: &Stmt, tail: bool) -> Result<bool, Unsupported> {
        self.line("ctx.begin()?;");
        match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side does not see the variable it binds.
                self.rhs(value)?;
                let slot = self.slot();
                self.line(&format!("ctx.bind({slot}, 0)?;"));
                self.declare(name, Var::Slot(slot));
            }
            Stmt::Assign {
                place: Place::Variable(name),
                value,
                ..
            } => {
                self.rhs(value)?;
                let slot = match self.resolve(name)? {
                    Var::Slot(slot) => slot,
                    Var::Param(index) => *self
                        .adopted
                        .get(&index)
                        .ok_or_else(|| Unsupported(format!("assigns `{name}` unadopted")))?,
                };
                self.line(&format!("ctx.assign({slot}, 0)?;"));
            }
            Stmt::Assign {
                place: Place::Member(member),
                value: Rhs::Expr(value),
                order,
            } => match order {
                // The right-hand side first, held while the place is
                // evaluated after it (`K-EVAL-003`).
                AssignOrder::ValueFirst => {
                    self.expr(value, 0)?;
                    self.line("ctx.pin(0)?;");
                    match self.member(member, 1)? {
                        Some(field) => self.line(&format!("ctx.set_field(1, {field}, 0)?;")),
                        None => self.line("ctx.set_index(1, 0)?;"),
                    }
                }
                AssignOrder::PlaceFirst => {
                    let field = self.member(member, 0)?;
                    let src = if field.is_some() { 1 } else { 2 };
                    self.expr(value, src)?;
                    match field {
                        Some(field) => self.line(&format!("ctx.set_field(0, {field}, {src})?;")),
                        None => self.line(&format!("ctx.set_index(0, {src})?;")),
                    }
                }
            },
            Stmt::Assign { .. } => {
                return Err(Unsupported("assigns a member by an action".into()));
            }
            Stmt::Remove { member } => match self.member(member, 0)? {
                Some(field) => self.line(&format!("ctx.remove_field(0, {field})?;")),
                None => self.line("ctx.remove_index(0)?;"),
            },
            Stmt::Do { value } => self.expr(value, 0)?,
            Stmt::Return { value } => {
                self.expr(value, 0)?;
                if tail {
                    self.line("Ok(ctx.take(0))");
                } else {
                    self.line("return Ok(ctx.take(0));");
                }
                return Ok(true);
            }
            Stmt::Throw { value } => {
                self.expr(value, 0)?;
                if tail {
                    self.line("Err(p::PlanExit::Raise(ctx.take(0)))");
                } else {
                    self.line("return Err(p::PlanExit::Raise(ctx.take(0)));");
                }
                return Ok(true);
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.expr(condition, 0)?;
                self.line("if ctx.test(0, p::Test::If)? {");
                self.indent += 1;
                let then = self.block(then_block, Vec::new(), tail)?;
                self.indent -= 1;
                if tail || !else_block.is_empty() {
                    self.line("} else {");
                    self.indent += 1;
                    let otherwise = self.block(else_block, Vec::new(), tail)?;
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
                return self.looped(*metered, None, body, else_block, |emitter| {
                    emitter.expr(condition, 0)?;
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
                self.expr(iterable, 0)?;
                let walked = self.slot();
                let level = self.loops.len();
                self.line(&format!("let c{level} = ctx.open(0, {walked})?;"));
                let bound = self.slot();
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
                self.unbind_scopes(scopes);
                let label = label(level);
                if continues {
                    self.line(&format!("continue {label};"));
                } else {
                    self.line(&format!("break {label};"));
                }
                return Ok(true);
            }
            Stmt::For { .. } => return Err(Unsupported("iterates through a protocol".into())),
            Stmt::Try(_) => return Err(Unsupported("has a `try`".into())),
            Stmt::ReturnVia { .. } => return Err(Unsupported("returns through a policy".into())),
            Stmt::Print { .. } | Stmt::Finish { .. } | Stmt::Fail { .. } => {
                return Err(Unsupported("prints or ends the run".into()));
            }
        }
        Ok(false)
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
        let departs = self.block(body, bound, false)?;
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
            self.block(else_block, Vec::new(), false)?;
            self.indent -= 1;
            self.line("}");
        }
        Ok(false)
    }
}

/// Assigned parameters: a body that writes a parameter holds it as a
/// variable from its entry.
fn assigned(block: &[Stmt], names: &mut BTreeSet<Name>) {
    for stmt in block {
        match stmt {
            Stmt::Assign {
                place: Place::Variable(name),
                ..
            } => {
                names.insert(name.clone());
            }
            Stmt::If {
                then_block,
                else_block,
                ..
            } => {
                assigned(then_block, names);
                assigned(else_block, names);
            }
            Stmt::For {
                body, else_block, ..
            }
            | Stmt::While {
                body, else_block, ..
            } => {
                assigned(body, names);
                assigned(else_block, names);
            }
            Stmt::Try(scope) => {
                assigned(&scope.body, names);
                if let Some(catch) = &scope.catch {
                    assigned(&catch.body, names);
                }
                if let Some(finally) = &scope.finally {
                    assigned(finally, names);
                }
            }
            _ => {}
        }
    }
}

/// Compiles `definition`'s body, whose identity is `function`, to the
/// plan named `ident`. Every body it calls must be in `planned`.
///
/// # Errors
///
/// The first form the compiler does not perform as the body does.
pub fn emit(
    library: &Library,
    planned: &BTreeSet<FunctionId>,
    function: FunctionId,
    definition: &FunctionDefinition,
    ident: &str,
) -> Result<Plan, Unsupported> {
    let body = definition
        .body()
        .ok_or_else(|| Unsupported("has no body".into()))?;
    let params: Vec<Name> = definition
        .signature
        .params
        .iter()
        .map(|param| param.name.clone())
        .collect();
    let mut emitter = Emitter {
        library,
        planned,
        callees: Vec::new(),
        literals: Vec::new(),
        fields: Vec::new(),
        records: Vec::new(),
        slots: 0,
        adopted: BTreeMap::new(),
        scopes: Vec::new(),
        loops: Vec::new(),
        out: String::new(),
        indent: 1,
        used: 0,
    };
    let mut written = BTreeSet::new();
    assigned(&body.block, &mut written);
    let mut bound = Vec::new();
    for (index, name) in params.iter().enumerate() {
        if written.contains(name) {
            let slot = emitter.slot();
            emitter.adopted.insert(index, slot);
            emitter.line(&format!("ctx.adopt({index}, {slot})?;"));
        }
        bound.push((name.clone(), Var::Param(index)));
    }
    emitter.block(&body.block, bound, true)?;
    let registers = emitter.used.max(1);
    let upper = ident.to_uppercase();
    let mut item = String::new();
    let _ = writeln!(item, "static {upper}: p::HelperPlan = p::HelperPlan {{");
    let _ = writeln!(item, "    name: {:?},", definition.name.as_str());
    let _ = writeln!(
        item,
        "    function: p::function({:?}),",
        function.to_string()
    );
    item.push_str("    callees: &[");
    for callee in &emitter.callees {
        let _ = write!(item, "\n        p::function({:?}),", callee.to_string());
    }
    item.push_str(if emitter.callees.is_empty() {
        "],\n"
    } else {
        "\n    ],\n"
    });
    item.push_str("    literals: &[");
    for literal in &emitter.literals {
        let _ = write!(item, "\n        {literal},");
    }
    item.push_str(if emitter.literals.is_empty() {
        "],\n"
    } else {
        "\n    ],\n"
    });
    item.push_str("    fields: &[");
    for field in &emitter.fields {
        let _ = write!(item, "{field:?}, ");
    }
    item.push_str("],\n");
    item.push_str("    records: &[");
    for record in &emitter.records {
        let fields: Vec<String> = record.iter().map(u32::to_string).collect();
        let _ = write!(item, "&[{}], ", fields.join(", "));
    }
    item.push_str("],\n");
    let _ = writeln!(item, "    registers: {registers},");
    let _ = writeln!(item, "    slots: {},", emitter.slots);
    let _ = writeln!(item, "    run: {ident},");
    item.push_str("};\n");
    let mut code = String::new();
    let _ = writeln!(code, "/// `{}`.", definition.name);
    let _ = writeln!(
        code,
        "fn {ident}(ctx: &mut dyn p::PlanContext) -> Result<Value, p::PlanExit> {{"
    );
    code.push_str(&emitter.out);
    code.push_str("}\n");
    Ok(Plan { item, code })
}
