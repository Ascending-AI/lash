//! A generator of admissible IR programs (FIG-5578).
//!
//! Programs are built as IR, never as source text, so what the generator
//! covers is the IR and not what a dialect's printer can spell: closures,
//! try/catch/finally, throws, scoped regions, computed assignment targets,
//! declared, inline and nested processes, loops with break and continue,
//! structural roles and host tool calls.
//!
//! A program is a function of a tape of choices. Each construct reads the
//! choices it needs, and a spent tape answers the first choice of every
//! question, which is always the smallest construct: a shorter or smaller
//! tape is a smaller program, so shrinking a tape shrinks the program it
//! spells. The generator keeps a typed scope as it goes, which is what makes
//! every program link: a name is read only where a binder reaches it and an
//! operation is applied only to a value of a type that admits it.

use std::collections::{BTreeMap, BTreeSet};

use lash_vm::testing::ast_builders as b;
use lash_vm::{
    AstString, AttributeAssignParts, AttributeWrite, CoercingBinaryOp, CoercingUnaryOp,
    Declaration, Expr, FunctionExpr, MethodKey, OperandLogicalOp, ProcessLiteralExpr,
    ProcessWrapperParts, Program, StructuralRole, TypeExpr, TypeField, UpdateOperator,
};

/// The host environment generated programs link against: the test catalogue
/// with labels enabled and one host descriptor constructor.
#[expect(
    clippy::expect_used,
    reason = "fixture catalogue registers one constructor into a catalogue that has none, per the message"
)]
pub fn environment() -> lash_vm::LashVmHostEnvironment {
    let mut environment = lash_vm::testing::harness::labeled_test_environment();
    environment
        .resources
        .add_value_constructor(
            ["timer", "Schedule"],
            TypeExpr::Object(vec![TypeField {
                name: "expr".into(),
                ty: TypeExpr::Str,
                optional: false,
            }]),
            TypeExpr::Ref("timer.Schedule".into()),
        )
        .expect("the value constructor is unique");
    environment
}

/// The most statements one generated program holds, over all its bodies.
const STATEMENT_BUDGET: usize = 28;

/// The deepest a generated body nests inside other bodies.
const MAX_BODY_DEPTH: usize = 3;

struct Tape<'t> {
    words: &'t [u16],
    at: usize,
}

impl Tape<'_> {
    /// One of `options` choices: the first once the tape is spent.
    fn pick(&mut self, options: usize) -> usize {
        let word = self.words.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        usize::from(word) % options.max(1)
    }

    /// True for one choice in `of`, and never once the tape is spent.
    fn rarely(&mut self, of: usize) -> bool {
        self.pick(of) == of - 1
    }
}

/// What the generator knows a variable holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Num,
    Str,
    Bool,
    /// A list of numbers.
    List,
    /// A record `{ count: Num, items: List, name: Str }`.
    Rec,
    Any,
    /// A function value of one parameter.
    Closure,
    /// A record with a `base` number and a `run` method of one parameter.
    Object,
    /// A process handle.
    Handle,
    /// A name bound straight to a process literal.
    Process,
}

/// The names visible in one body, and what kind of body it is.
#[derive(Clone)]
struct Ctx {
    vars: Vec<(String, Ty)>,
    /// Names the generator never assigns again: captures and hidden
    /// arguments copy the value a name had, so the name keeps it.
    frozen: BTreeSet<String>,
    /// Numbers bound once and never assigned again, which a process literal
    /// may take as a hidden argument.
    seeds: Vec<String>,
    in_loop: bool,
    /// Inside a function body, where `return` is legal.
    in_function: bool,
    /// Directly inside a process body, where `fail` is legal.
    in_process: bool,
    /// Whether the body may perform effects: a function value's may not.
    effects: bool,
    /// Inside a process, which has no console: `print` is main's.
    process: bool,
    depth: usize,
    literal_depth: usize,
}

impl Ctx {
    fn main() -> Self {
        Self {
            vars: Vec::new(),
            frozen: BTreeSet::new(),
            seeds: Vec::new(),
            in_loop: false,
            in_function: false,
            in_process: false,
            effects: true,
            process: false,
            depth: 0,
            literal_depth: 0,
        }
    }

    fn of(&self, ty: Ty) -> Vec<&str> {
        self.vars
            .iter()
            .filter(|(_, bound)| *bound == ty)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    fn bind(&mut self, name: &str, ty: Ty) {
        self.vars.retain(|(bound, _)| bound != name);
        self.vars.push((name.to_string(), ty));
    }

    /// A body nested in this one: it sees what this one sees, and what it
    /// binds stays inside it.
    fn nested(&self) -> Self {
        let mut nested = self.clone();
        nested.depth += 1;
        nested
    }

    fn looping(&self) -> Self {
        let mut nested = self.nested();
        nested.in_loop = true;
        nested
    }

    /// The body of a function value: it sees its own names only.
    fn function(&self, vars: Vec<(String, Ty)>) -> Self {
        Self {
            vars,
            frozen: BTreeSet::new(),
            seeds: Vec::new(),
            in_loop: false,
            in_function: true,
            in_process: false,
            effects: false,
            process: self.process,
            depth: self.depth + 1,
            literal_depth: self.literal_depth,
        }
    }
}

struct Generator<'t> {
    tape: Tape<'t>,
    names: usize,
    budget: usize,
    declarations: Vec<Declaration>,
    private: BTreeSet<AstString>,
    functions: Vec<String>,
    processes: Vec<String>,
    /// The prefix of every name the generator invents, so two generators
    /// never invent the same one.
    stem: String,
    /// Whether the generator may add declarations to the program.
    declares: bool,
}

/// The program `words` spells.
pub fn program(words: &[u16]) -> Program {
    let mut generator = Generator::new(words, "v");
    let mut ctx = Ctx::main();
    let mut main = generator.body(&mut ctx, 5);
    let value = generator.any(&ctx, 1);
    main.push(b::finish(value));
    Program {
        declarations: generator.declarations,
        main: Expr::Block(main),
        private_bindings: generator.private,
        spans: BTreeMap::new(),
    }
}

/// A statement that reads nothing it does not bind, with the names it binds
/// drawn from `stem`: one an edit can insert anywhere.
pub fn closed_statement(words: &[u16], stem: &str) -> Expr {
    let mut generator = Generator::new(words, stem);
    // Declarations belong to the program a statement joins, so a closed
    // statement uses none.
    generator.declares = false;
    generator.budget = 6;
    let mut ctx = Ctx::main();
    ctx.depth = MAX_BODY_DEPTH - 1;
    let mut statements = Vec::new();
    while statements.is_empty() {
        statements = generator.closed(&mut ctx);
    }
    match statements.len() {
        1 => statements.remove(0),
        _ => b::role(StructuralRole::Scope, b::block(statements)),
    }
}

/// A pure expression that reads no variable.
pub fn closed_expression(words: &[u16]) -> Expr {
    Generator::new(words, "x").any(&Ctx::main(), 2)
}

impl<'t> Generator<'t> {
    fn new(words: &'t [u16], stem: &str) -> Self {
        Self {
            tape: Tape { words, at: 0 },
            names: 0,
            budget: STATEMENT_BUDGET,
            declarations: Vec::new(),
            private: BTreeSet::new(),
            functions: Vec::new(),
            processes: Vec::new(),
            stem: stem.to_string(),
            declares: true,
        }
    }

    fn fresh(&mut self) -> String {
        self.names += 1;
        format!("{}{}", self.stem, self.names)
    }

    /// A front end's own slot: a private binding of `main`.
    fn slot(&mut self, role: &str) -> AstString {
        self.names += 1;
        let name = AstString::from(format!("__{}_{role}{}", self.stem, self.names));
        self.private.insert(name.clone());
        name
    }

    fn one_of(&mut self, names: &[&str]) -> Option<String> {
        (!names.is_empty()).then(|| names[self.tape.pick(names.len())].to_string())
    }

    // -----------------------------------------------------------------
    //  Expressions
    // -----------------------------------------------------------------

    fn num(&mut self, ctx: &Ctx, depth: usize) -> Expr {
        match self.tape.pick(if depth == 0 { 2 } else { 9 }) {
            0 => b::num([0.0, 1.0, 2.0, 3.0, -1.0, 0.5][self.tape.pick(6)]),
            1 => match self.one_of(&ctx.of(Ty::Num)) {
                Some(name) => b::var(&name),
                None => b::num(1.0),
            },
            2 => {
                let op = [
                    CoercingBinaryOp::Add,
                    CoercingBinaryOp::Subtract,
                    CoercingBinaryOp::Multiply,
                    CoercingBinaryOp::Remainder,
                ][self.tape.pick(4)];
                b::binary(self.num(ctx, depth - 1), op, self.num(ctx, depth - 1))
            }
            3 => b::unary(CoercingUnaryOp::Negate, self.num(ctx, depth - 1)),
            4 => b::builtin("len", vec![self.list(ctx, depth - 1)]),
            5 => match self.one_of(&ctx.of(Ty::Rec)) {
                Some(name) => b::field(b::var(&name), "count"),
                None => b::num(2.0),
            },
            6 => match self.one_of(&ctx.of(Ty::Object)) {
                Some(name) => b::field(b::var(&name), "base"),
                None => b::num(3.0),
            },
            7 => b::unary(CoercingUnaryOp::Plus, self.text(ctx, depth - 1)),
            _ => b::binary(
                self.num(ctx, depth - 1),
                CoercingBinaryOp::BitOr,
                self.num(ctx, depth - 1),
            ),
        }
    }

    fn text(&mut self, ctx: &Ctx, depth: usize) -> Expr {
        match self.tape.pick(if depth == 0 { 2 } else { 6 }) {
            0 => b::string(["", "a", "status", "line\nbreak"][self.tape.pick(4)]),
            1 => match self.one_of(&ctx.of(Ty::Str)) {
                Some(name) => b::var(&name),
                None => b::string("text"),
            },
            2 => b::binary(
                self.text(ctx, depth - 1),
                CoercingBinaryOp::Add,
                self.text(ctx, depth - 1),
            ),
            3 => b::unary(CoercingUnaryOp::ToString, self.num(ctx, depth - 1)),
            4 => b::unary(CoercingUnaryOp::TypeOf, self.any(ctx, depth - 1)),
            _ => match self.one_of(&ctx.of(Ty::Rec)) {
                Some(name) => b::field(b::var(&name), "name"),
                None => b::string("named"),
            },
        }
    }

    fn truth(&mut self, ctx: &Ctx, depth: usize) -> Expr {
        match self.tape.pick(if depth == 0 { 2 } else { 7 }) {
            0 => b::bool_lit(self.tape.pick(2) == 1),
            1 => match self.one_of(&ctx.of(Ty::Bool)) {
                Some(name) => b::var(&name),
                None => b::bool_lit(true),
            },
            2 => {
                let op = [
                    CoercingBinaryOp::Less,
                    CoercingBinaryOp::GreaterEqual,
                    CoercingBinaryOp::StrictEqual,
                    CoercingBinaryOp::StrictNotEqual,
                ][self.tape.pick(4)];
                b::binary(self.num(ctx, depth - 1), op, self.num(ctx, depth - 1))
            }
            3 => b::unary(CoercingUnaryOp::Not, self.truth(ctx, depth - 1)),
            4 => b::logical(
                self.truth(ctx, depth - 1),
                OperandLogicalOp::And,
                self.truth(ctx, depth - 1),
            ),
            5 => b::logical(
                self.truth(ctx, depth - 1),
                OperandLogicalOp::Or,
                self.truth(ctx, depth - 1),
            ),
            _ => b::binary(
                self.text(ctx, depth - 1),
                CoercingBinaryOp::StrictEqual,
                self.text(ctx, depth - 1),
            ),
        }
    }

    fn list(&mut self, ctx: &Ctx, depth: usize) -> Expr {
        match self.tape.pick(if depth == 0 { 2 } else { 3 }) {
            0 => {
                let length = self.tape.pick(4);
                b::list((0..length).map(|_| self.num(ctx, depth)).collect())
            }
            1 => match self.one_of(&ctx.of(Ty::List)) {
                Some(name) => b::var(&name),
                None => b::list(vec![b::num(1.0), b::num(2.0)]),
            },
            _ => match self.one_of(&ctx.of(Ty::Rec)) {
                Some(name) => b::field(b::var(&name), "items"),
                None => b::list(Vec::new()),
            },
        }
    }

    fn rec(&mut self, ctx: &Ctx, depth: usize) -> Expr {
        match (self.tape.pick(2), self.one_of(&ctx.of(Ty::Rec))) {
            (1, Some(name)) => b::var(&name),
            _ => {
                let depth = depth.saturating_sub(1);
                b::record(vec![
                    ("count", self.num(ctx, depth)),
                    ("items", self.list(ctx, depth)),
                    ("name", self.text(ctx, depth)),
                ])
            }
        }
    }

    fn any(&mut self, ctx: &Ctx, depth: usize) -> Expr {
        match self.tape.pick(if depth == 0 { 4 } else { 12 }) {
            0 => b::null(),
            1 => self.num(ctx, 0),
            2 => self.text(ctx, 0),
            3 => match self.one_of(&ctx.of(Ty::Any)) {
                Some(name) => b::var(&name),
                None => Expr::Absent,
            },
            4 => self.truth(ctx, depth - 1),
            5 => self.list(ctx, depth - 1),
            6 => self.rec(ctx, depth - 1),
            7 => b::logical(
                self.any(ctx, depth - 1),
                OperandLogicalOp::NullishCoalesce,
                self.any(ctx, depth - 1),
            ),
            8 => b::if_else(
                self.truth(ctx, depth - 1),
                self.any(ctx, depth - 1),
                self.any(ctx, depth - 1),
            ),
            9 => b::index(self.list(ctx, depth - 1), self.num(ctx, depth - 1)),
            10 => self.num(ctx, depth),
            _ => self.text(ctx, depth),
        }
    }

    fn typed(&mut self, ctx: &Ctx, ty: Ty, depth: usize) -> Expr {
        match ty {
            Ty::Num => self.num(ctx, depth),
            Ty::Str => self.text(ctx, depth),
            Ty::Bool => self.truth(ctx, depth),
            Ty::List => self.list(ctx, depth),
            Ty::Rec => self.rec(ctx, depth),
            _ => self.any(ctx, depth),
        }
    }

    /// A function value of one parameter that reads `captures`.
    fn closure(&mut self, ctx: &Ctx, receiver: Option<&AstString>) -> Expr {
        let param = self.fresh();
        let captured = self.one_of(&ctx.of(Ty::Num));
        let mut vars = vec![(param.clone(), Ty::Num)];
        vars.extend(captured.iter().map(|name| (name.clone(), Ty::Num)));
        let mut inner = ctx.function(vars);
        let mut value = self.num(&inner, 2);
        if let Some(receiver) = receiver {
            value = b::binary(
                b::field(b::var(receiver), "base"),
                CoercingBinaryOp::Add,
                value,
            );
        }
        let body = match self.tape.pick(3) {
            0 => value,
            1 => b::block(vec![Expr::FunctionReturn(Box::new(value))]),
            _ => {
                let mut statements = self.pure_body(&mut inner, 2);
                statements.push(Expr::FunctionReturn(Box::new(value)));
                statements.push(Expr::Absent);
                b::role(StructuralRole::Completion, b::block(statements))
            }
        };
        Expr::Function(Box::new(FunctionExpr {
            name: None,
            js_name: (self.tape.pick(2) == 1).then(|| "named".into()),
            receiver: receiver.cloned(),
            params: vec![param.into()],
            captures: captured.into_iter().map(Into::into).collect(),
            body: Box::new(body),
        }))
    }

    /// Shows `value`: on the console in `main`, through the host in a
    /// process.
    fn say(&mut self, ctx: &Ctx, value: Expr) -> Expr {
        if ctx.process {
            self.tool("echo", value)
        } else {
            b::print(value)
        }
    }

    /// An index a write through a list accepts: its first element, or the
    /// one past its last.
    fn writable_index(&mut self, list: Expr) -> Expr {
        let length = b::builtin("len", vec![list]);
        match self.tape.pick(3) {
            0 => b::num(0.0),
            1 => length.clone(),
            _ => b::binary(length.clone(), CoercingBinaryOp::Subtract, length),
        }
    }

    /// `await tools.<operation>({ value })`, in either order the IR spells a
    /// settled call.
    fn tool(&mut self, operation: &str, value: Expr) -> Expr {
        let call = b::receiver_call(
            b::resource(&["tools"]),
            operation,
            vec![b::record(vec![("value", value)])],
        );
        if self.tape.pick(2) == 0 {
            b::await_expr(b::unwrap(call))
        } else {
            b::unwrap(b::await_expr(call))
        }
    }

    // -----------------------------------------------------------------
    //  Bodies
    // -----------------------------------------------------------------

    /// Between one and `most` statements.
    fn body(&mut self, ctx: &mut Ctx, most: usize) -> Vec<Expr> {
        let count = 1 + self.tape.pick(most);
        let mut statements = Vec::new();
        for _ in 0..count {
            if self.budget == 0 {
                break;
            }
            self.budget -= 1;
            statements.extend(self.statement(ctx));
        }
        statements
    }

    /// Statements with no effect, for a function value's body.
    fn pure_body(&mut self, ctx: &mut Ctx, most: usize) -> Vec<Expr> {
        let count = self.tape.pick(most + 1);
        let mut statements = Vec::new();
        for _ in 0..count {
            if self.budget == 0 {
                break;
            }
            self.budget -= 1;
            statements.extend(self.pure_statement(ctx));
        }
        statements
    }

    /// A body as the IR spells one: a block, or a statement list closed by a
    /// completion value.
    fn spelled(&mut self, statements: Vec<Expr>) -> Expr {
        if self.tape.pick(2) == 0 {
            b::block(statements)
        } else {
            let mut statements = statements;
            statements.push(Expr::Absent);
            b::role(StructuralRole::Completion, b::block(statements))
        }
    }

    fn statement(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        if !ctx.effects {
            return self.pure_statement(ctx);
        }
        let nest = ctx.depth < MAX_BODY_DEPTH && self.budget > 0;
        let kinds = if nest { 34 } else { 18 };
        let kind = self.tape.pick(kinds);
        if !self.declares && matches!(kind, 10 | 11 | 16 | 29 | 30) {
            return self.pure_statement(ctx);
        }
        match kind {
            0..=4 => self.pure_statement(ctx),
            5 => {
                let name = self.fresh();
                let value = self.any(ctx, 2);
                let call = self.tool("echo", value);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(&name, call)]
            }
            6 => {
                let value = self.any(ctx, 1);
                let call = self.tool("echo", value);
                let call = match self.tape.pick(3) {
                    0 => call,
                    1 => b::labelled(b::label("Notify", None), call),
                    _ => b::labelled(b::label("Record", Some("keeps the value")), call),
                };
                vec![call]
            }
            7 => {
                let value = self.any(ctx, 2);
                vec![self.say(ctx, value)]
            }
            8 => vec![b::sleep_for(b::num(1.0))],
            9 => {
                let name = self.fresh();
                let input = b::record(vec![("expr", self.text(ctx, 1))]);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(
                    &name,
                    b::host_descriptor("timer.Schedule", input),
                )]
            }
            10 => self.start(ctx),
            11 => match self.one_of(&ctx.of(Ty::Handle)) {
                Some(handle) => {
                    let name = self.fresh();
                    ctx.bind(&name, Ty::Any);
                    vec![b::assign(&name, b::await_expr(b::var(&handle)))]
                }
                None => self.start(ctx),
            },
            12 => {
                let name = self.fresh();
                let label = b::label("Fetch", Some("asks the host"));
                let value = self.text(ctx, 1);
                let call = self.tool("echo", value);
                ctx.bind(&name, Ty::Any);
                vec![b::labelled(label, b::assign(&name, call))]
            }
            13 if ctx.in_loop => {
                let control = if self.tape.pick(2) == 0 {
                    Expr::Break
                } else {
                    Expr::Continue
                };
                let condition = self.truth(ctx, 2);
                vec![b::if_else(
                    condition,
                    b::block(vec![control]),
                    b::block(Vec::new()),
                )]
            }
            14 if ctx.in_process => {
                let condition = self.truth(ctx, 1);
                let reason = self.text(ctx, 1);
                vec![b::if_else(
                    condition,
                    b::block(vec![b::fail(reason)]),
                    b::block(Vec::new()),
                )]
            }
            15 if self.tape.rarely(3) => {
                // A failure nothing handles: the run stops here, with the
                // value thrown or the host's refusal.
                if self.tape.pick(2) == 0 {
                    let value = self.any(ctx, 1);
                    vec![Expr::Throw(Box::new(value))]
                } else {
                    vec![self.tool("err", b::null())]
                }
            }
            16 => {
                let name = self.fresh();
                let process = self.process(ctx);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(&name, b::process_ref(&process))]
            }
            13..=17 => self.pure_statement(ctx),
            18..=19 => self.branch(ctx),
            20..=21 => self.for_loop(ctx),
            22 => self.while_loop(ctx),
            23..=24 => self.try_region(ctx),
            25 => {
                let mut inner = ctx.nested();
                let statements = self.body(&mut inner, 3);
                vec![b::role(StructuralRole::Scope, b::block(statements))]
            }
            26 => {
                let mut inner = ctx.nested();
                let mut statements = self.body(&mut inner, 2);
                statements.push(self.any(ctx, 1));
                vec![b::role(StructuralRole::Completion, b::block(statements))]
            }
            27 => {
                let mut inner = ctx.nested();
                let statements = self.body(&mut inner, 2);
                vec![b::block(statements)]
            }
            28 => {
                let name = self.fresh();
                let mut inner = ctx.nested();
                let mut statements = self.body(&mut inner, 2);
                statements.push(self.any(ctx, 1));
                ctx.bind(&name, Ty::Any);
                vec![b::assign(
                    &name,
                    b::role(StructuralRole::Scope, b::block(statements)),
                )]
            }
            29..=30 => self.literal(ctx),
            31 => {
                let name = self.fresh();
                let condition = self.truth(ctx, 2);
                let then = self.any(ctx, 1);
                let otherwise = self.any(ctx, 1);
                let call = self.tool("echo", then);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(&name, b::if_else(condition, call, otherwise))]
            }
            32 => {
                let name = self.fresh();
                let caught = self.fresh();
                let value = self.any(ctx, 1);
                let operation = ["echo", "err"][self.tape.pick(2)];
                let call = self.tool(operation, value);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(
                    &name,
                    b::try_expr(call, Some(b::catch(&caught, b::var(&caught))), None),
                )]
            }
            _ => self.branch(ctx),
        }
    }

    /// A statement that performs no effect and so may sit in a function
    /// value's body.
    fn pure_statement(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        match self.tape.pick(20) {
            0 => {
                let name = self.fresh();
                let value = self.num(ctx, 2);
                ctx.bind(&name, Ty::Num);
                vec![b::assign(&name, value)]
            }
            1 => {
                let ty = [Ty::Str, Ty::Bool, Ty::List, Ty::Rec, Ty::Any][self.tape.pick(5)];
                let name = self.fresh();
                let value = self.typed(ctx, ty, 2);
                ctx.bind(&name, ty);
                vec![b::assign(&name, value)]
            }
            2 => {
                // Rebinding a name with a value of the type it has.
                let candidates = ctx
                    .vars
                    .iter()
                    .filter(|(name, ty)| {
                        !ctx.frozen.contains(name)
                            && matches!(ty, Ty::Num | Ty::Str | Ty::Bool | Ty::List | Ty::Rec)
                    })
                    .map(|(name, ty)| (name.clone(), *ty))
                    .collect::<Vec<_>>();
                if candidates.is_empty() {
                    return self.pure_statement(ctx);
                }
                let (name, ty) = candidates[self.tape.pick(candidates.len())].clone();
                let value = self.typed(ctx, ty, 2);
                vec![b::assign(&name, value)]
            }
            3 => {
                // A write through a path, with a field step.
                let mut statements = Vec::new();
                let record = self.need(ctx, Ty::Rec, &mut statements);
                let value = self.num(ctx, 2);
                statements.push(b::assign_path(&record, vec![b::field_step("count")], value));
                statements
            }
            4 => {
                // A computed assignment target.
                let mut statements = Vec::new();
                let list = self.need(ctx, Ty::List, &mut statements);
                let index = self.writable_index(b::var(&list));
                let value = self.num(ctx, 1);
                statements.push(b::assign_path(&list, vec![b::index_step(index)], value));
                statements
            }
            5 => {
                // A computed target below a field.
                let mut statements = Vec::new();
                let record = self.need(ctx, Ty::Rec, &mut statements);
                let index = self.writable_index(b::field(b::var(&record), "items"));
                let value = self.num(ctx, 1);
                statements.push(b::assign_path(
                    &record,
                    vec![b::field_step("items"), b::index_step(index)],
                    value,
                ));
                statements
            }
            6..=7 => self.member_assignment(ctx),
            8 => {
                let name = self.fresh();
                let closure = self.closure(ctx, None);
                ctx.bind(&name, Ty::Closure);
                ctx.frozen.insert(name.clone());
                vec![b::assign(&name, closure)]
            }
            9 => {
                let mut statements = Vec::new();
                let closure = self.need(ctx, Ty::Closure, &mut statements);
                let name = self.fresh();
                let argument = self.num(ctx, 1);
                ctx.bind(&name, Ty::Any);
                statements.push(b::assign(&name, b::call(b::var(&closure), vec![argument])));
                statements
            }
            10 => {
                let mut statements = Vec::new();
                let object = self.need(ctx, Ty::Object, &mut statements);
                let name = self.fresh();
                let argument = self.num(ctx, 1);
                let method = if self.tape.pick(2) == 0 {
                    MethodKey::Field("run".into())
                } else {
                    MethodKey::Index(Box::new(b::string("run")))
                };
                ctx.bind(&name, Ty::Any);
                statements.push(b::assign(
                    &name,
                    Expr::MethodCall {
                        receiver: Box::new(b::var(&object)),
                        method,
                        args: vec![argument],
                    },
                ));
                statements
            }
            11 => {
                let mut statements = Vec::new();
                let object = self.need(ctx, Ty::Object, &mut statements);
                let name = self.fresh();
                let argument = self.num(ctx, 1);
                ctx.bind(&name, Ty::Any);
                statements.push(b::assign(
                    &name,
                    Expr::ThisCall {
                        this: Box::new(b::var(&object)),
                        function: Box::new(b::field(b::var(&object), "run")),
                        args: vec![argument],
                    },
                ));
                statements
            }
            12 => {
                let name = self.fresh();
                let items = self.list(ctx, 1);
                let function = self.closure(ctx, None);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(
                    &name,
                    Expr::Map {
                        items: Box::new(items),
                        function: Box::new(function),
                    },
                )]
            }
            13 if self.declares => {
                let function = self.function();
                let name = self.fresh();
                let argument = self.num(ctx, 1);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(
                    &name,
                    b::function_call(&function, vec![argument]),
                )]
            }
            14 => {
                let name = self.fresh();
                let call = match self.tape.pick(3) {
                    0 => b::builtin("keys", vec![self.rec(ctx, 1)]),
                    1 => b::builtin("to_string", vec![self.num(ctx, 1)]),
                    _ => b::builtin("values", vec![self.rec(ctx, 1)]),
                };
                ctx.bind(&name, Ty::Any);
                vec![b::assign(&name, call)]
            }
            15 => self.transform(ctx),
            16 => {
                // A display role over a block that binds its input and then
                // picks a result.
                let name = self.fresh();
                let input = self.slot("input");
                let value = self.any(ctx, 1);
                let then = self.num(ctx, 1);
                let otherwise = self.num(ctx, 1);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(
                    &name,
                    b::role(
                        StructuralRole::JsonTraversal,
                        b::block(vec![
                            b::assign(&input, value),
                            b::if_else(b::var(&input), then, otherwise),
                        ]),
                    ),
                )]
            }
            17 if ctx.in_function && self.tape.rarely(3) => {
                let condition = self.truth(ctx, 1);
                let value = self.num(ctx, 1);
                vec![b::if_else(
                    condition,
                    b::block(vec![Expr::FunctionReturn(Box::new(value))]),
                    b::block(Vec::new()),
                )]
            }
            18 if ctx.depth < MAX_BODY_DEPTH => {
                // A pure loop: it binds nothing a later statement reads.
                let element = self.fresh();
                let iterable = self.list(ctx, 1);
                let mut inner = ctx.looping();
                inner.bind(&element, Ty::Num);
                let total = self.fresh();
                let value = self.num(&inner, 2);
                vec![b::for_in(
                    &element,
                    iterable,
                    b::block(vec![b::assign(&total, value)]),
                )]
            }
            _ => {
                let name = self.fresh();
                let value = self.any(ctx, 2);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(&name, value)]
            }
        }
    }

    /// The name of a variable of `ty`: one in scope, or one a statement
    /// pushed onto `statements` binds.
    fn need(&mut self, ctx: &mut Ctx, ty: Ty, statements: &mut Vec<Expr>) -> String {
        if let Some(name) = self.one_of(&ctx.of(ty)) {
            return name;
        }
        let name = self.fresh();
        let value = match ty {
            Ty::Closure => self.closure(ctx, None),
            Ty::Object => {
                let receiver = self.slot("this");
                let run = self.closure(ctx, Some(&receiver));
                b::record(vec![("base", b::num(10.0)), ("run", run)])
            }
            other => self.typed(ctx, other, 1),
        };
        if matches!(ty, Ty::Closure | Ty::Object) {
            ctx.frozen.insert(name.clone());
        }
        ctx.bind(&name, ty);
        statements.push(b::assign(&name, value));
        name
    }

    /// `record.count = value`, `record.count += value` and their computed
    /// forms, as the role that pins the base before it evaluates the value.
    fn member_assignment(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let mut statements = Vec::new();
        let record = self.need(ctx, Ty::Rec, &mut statements);
        let base = self.slot("base");
        let result = self.slot("result");
        let step = if self.tape.pick(2) == 1 {
            AttributeWrite::Index {
                key: self.slot("key"),
                index: b::string("count"),
            }
        } else {
            AttributeWrite::Field(AstString::from("count"))
        };
        let operand = self.num(ctx, 1);
        let value = if self.tape.pick(2) == 0 {
            operand
        } else {
            let operator = [
                UpdateOperator::Add,
                UpdateOperator::Subtract,
                UpdateOperator::Multiply,
            ][self.tape.pick(3)];
            AttributeAssignParts::update_value(&base, &step, operator, operand)
        };
        statements.push(AttributeAssignParts::build(
            base,
            step,
            result,
            b::var(&record),
            value,
        ));
        statements
    }

    /// A callback-driven transform: it binds its receiver and its callback,
    /// then a driver that captures both, then runs the driver.
    fn transform(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let name = self.fresh();
        let receiver = self.slot("receiver");
        let callback = self.slot("callback");
        let driver = self.slot("driver");
        let items = self.list(ctx, 1);
        let function = self.closure(ctx, None);
        ctx.bind(&name, Ty::Any);
        vec![b::assign(
            &name,
            b::role(
                StructuralRole::CollectionTransform {
                    operation: "map".into(),
                },
                b::block(vec![
                    b::assign(&receiver, items),
                    b::assign(&callback, function),
                    b::assign(
                        &driver,
                        b::closure(
                            None,
                            &[],
                            &[receiver.as_str(), callback.as_str()],
                            Expr::Map {
                                items: Box::new(b::var(&receiver)),
                                function: Box::new(b::var(&callback)),
                            },
                        ),
                    ),
                    b::call(b::var(&driver), Vec::new()),
                ]),
            ),
        )]
    }

    fn branch(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let condition = self.truth(ctx, 2);
        let mut then_ctx = ctx.nested();
        let then = self.body(&mut then_ctx, 3);
        let then = self.spelled(then);
        let otherwise = match self.tape.pick(3) {
            0 => b::block(Vec::new()),
            1 => {
                let mut else_ctx = ctx.nested();
                let statements = self.body(&mut else_ctx, 3);
                self.spelled(statements)
            }
            _ => {
                // An `else if`: a block holding one branch.
                let mut else_ctx = ctx.nested();
                b::block(self.branch(&mut else_ctx))
            }
        };
        vec![b::if_else(condition, then, otherwise)]
    }

    fn for_loop(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let iterable = self.list(ctx, 1);
        let element = self.fresh();
        let mut inner = ctx.looping();
        if self.tape.pick(3) == 0 {
            // The front end's own element slot, bound into the authored name
            // before each body.
            let slot = self.slot("element");
            inner.bind(&element, Ty::Num);
            let statements = self.body(&mut inner, 3);
            let body = self.spelled(statements);
            return vec![Expr::For {
                binding: slot.clone(),
                authored_binding: Some(element.as_str().into()),
                iterable: Box::new(iterable),
                bind: Some(Box::new(b::block(vec![b::assign(&element, b::var(&slot))]))),
                body: Box::new(body),
            }];
        }
        inner.bind(&element, Ty::Num);
        let statements = self.body(&mut inner, 3);
        let body = self.spelled(statements);
        vec![b::for_in(&element, iterable, body)]
    }

    /// A loop a counter bounds: the counter advances before anything in the
    /// body can `continue`.
    fn while_loop(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let counter = self.fresh();
        ctx.bind(&counter, Ty::Num);
        ctx.frozen.insert(counter.clone());
        let bound = 1 + self.tape.pick(3);
        let mut inner = ctx.looping();
        let mut statements = vec![b::assign(
            &counter,
            b::binary(b::var(&counter), CoercingBinaryOp::Add, b::num(1.0)),
        )];
        statements.extend(self.body(&mut inner, 3));
        let body = self.spelled(statements);
        vec![
            b::assign(&counter, b::num(0.0)),
            b::while_loop(
                b::binary(
                    b::var(&counter),
                    CoercingBinaryOp::Less,
                    b::num(bound as f64),
                ),
                body,
            ),
        ]
    }

    fn try_region(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let mut body_ctx = ctx.nested();
        let mut statements = self.body(&mut body_ctx, 3);
        let handled = self.tape.pick(4) != 0;
        if handled {
            match self.tape.pick(3) {
                0 => {}
                1 => {
                    let value = self.any(&body_ctx, 1);
                    statements.push(Expr::Throw(Box::new(value)));
                }
                _ => statements.push(self.tool("err", b::string("refused"))),
            }
        }
        let body = self.spelled(statements);
        let catch = handled.then(|| {
            let binding = self.fresh();
            let mut catch_ctx = ctx.nested();
            catch_ctx.bind(&binding, Ty::Any);
            let mut statements = vec![self.say(&catch_ctx, b::var(&binding))];
            if self.tape.pick(2) == 1 {
                statements.extend(self.body(&mut catch_ctx, 2));
            }
            b::catch(&binding, b::block(statements))
        });
        let finally = (!handled || self.tape.pick(2) == 1).then(|| {
            let mut finally_ctx = ctx.nested();
            b::block(self.body(&mut finally_ctx, 2))
        });
        vec![b::try_expr(body, catch, finally)]
    }

    // -----------------------------------------------------------------
    //  Declarations and processes
    // -----------------------------------------------------------------

    /// The name of a declared pure function of one parameter.
    fn function(&mut self) -> String {
        if !self.functions.is_empty() && self.tape.pick(2) == 0 {
            return self.functions[self.tape.pick(self.functions.len())].clone();
        }
        let name = format!("{}_fn{}", self.stem, self.functions.len());
        let ctx = Ctx::main().function(vec![("value".to_string(), Ty::Num)]);
        let value = self.num(&ctx, 2);
        let body = match self.tape.pick(3) {
            0 => value,
            1 => b::block(vec![Expr::FunctionReturn(Box::new(value))]),
            _ => b::block(vec![
                b::if_else(
                    b::binary(b::var("value"), CoercingBinaryOp::Less, b::num(0.0)),
                    b::block(vec![Expr::FunctionReturn(Box::new(b::num(0.0)))]),
                    b::block(Vec::new()),
                ),
                Expr::FunctionReturn(Box::new(value)),
            ]),
        };
        self.declarations.push(b::function_decl(
            &name,
            vec![b::function_param("value", TypeExpr::Any)],
            TypeExpr::Any,
            body,
        ));
        self.functions.push(name.clone());
        name
    }

    /// The body of a process over one `value` parameter and `hidden`
    /// arguments, inside the failure wrapper or without one.
    fn process_body(&mut self, ctx: &Ctx, hidden: &[String], wrapped: bool) -> Expr {
        let mut vars = vec![("value".to_string(), Ty::Any)];
        vars.extend(hidden.iter().map(|name| (name.clone(), Ty::Any)));
        let mut inner = Ctx {
            vars,
            frozen: hidden.iter().cloned().collect(),
            seeds: Vec::new(),
            in_loop: false,
            in_function: wrapped,
            in_process: !wrapped,
            effects: true,
            process: true,
            depth: ctx.depth + 1,
            literal_depth: ctx.literal_depth + 1,
        };
        let mut statements = self.body(&mut inner, 3);
        let value = self.any(&inner, 1);
        if !wrapped {
            statements.push(b::finish(value));
            return b::block(statements);
        }
        statements.push(Expr::FunctionReturn(Box::new(value)));
        statements.push(Expr::Absent);
        ProcessWrapperParts::build(
            FunctionExpr {
                name: None,
                js_name: None,
                receiver: None,
                params: vec!["value".into()],
                captures: hidden.iter().map(|name| name.as_str().into()).collect(),
                body: Box::new(b::role(StructuralRole::Completion, b::block(statements))),
            },
            None,
            vec![b::var("value")],
            "__process_error".into(),
        )
    }

    /// The name of a declared process of one `value` parameter.
    fn process(&mut self, ctx: &Ctx) -> String {
        if !self.processes.is_empty() && (self.tape.pick(2) == 0 || ctx.literal_depth > 0) {
            return self.processes[self.tape.pick(self.processes.len())].clone();
        }
        let name = format!("{}_worker{}", self.stem, self.processes.len());
        // Declared first, so a process its own body starts is another one.
        self.processes.push(name.clone());
        let wrapped = self.tape.pick(2) == 1;
        let body = self.process_body(&Ctx::main(), &[], wrapped);
        let label = (self.tape.pick(3) == 0).then(|| b::label("Worker", Some("does the work")));
        self.declarations
            .push(Declaration::Process(lash_vm::ProcessDecl {
                name: name.as_str().into(),
                params: vec![b::param("value", TypeExpr::Any)],
                return_ty: None,
                label,
                origin: lash_vm::ProcessOrigin::Declared,
                body,
            }));
        name
    }

    /// An inline process body, with the hidden argument it reads when the
    /// scope has a number to give it.
    fn process_literal(&mut self, ctx: &mut Ctx) -> Expr {
        let hidden = match self.tape.pick(2) {
            0 => Vec::new(),
            _ => {
                let seeds = ctx.seeds.iter().map(String::as_str).collect::<Vec<_>>();
                self.one_of(&seeds).into_iter().collect()
            }
        };
        let body = self.process_body(ctx, &hidden, true);
        Expr::ProcessLiteral(Box::new(ProcessLiteralExpr {
            params: vec![b::param("value", TypeExpr::Any)],
            hidden_args: hidden
                .iter()
                .map(|name| b::param(name, TypeExpr::Any))
                .collect(),
            return_ty: None,
            body: Box::new(body),
        }))
    }

    /// Binds a name to an inline process, after a number its body may take
    /// as a hidden argument.
    fn literal(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        if ctx.literal_depth >= 2 {
            return self.start(ctx);
        }
        let mut statements = Vec::new();
        if self.tape.pick(2) == 1 {
            let seed = self.fresh();
            let value = self.num(ctx, 1);
            ctx.bind(&seed, Ty::Num);
            ctx.frozen.insert(seed.clone());
            ctx.seeds.push(seed.clone());
            statements.push(b::assign(&seed, value));
        }
        let name = self.fresh();
        let literal = self.process_literal(ctx);
        ctx.bind(&name, Ty::Process);
        ctx.frozen.insert(name.clone());
        statements.push(b::assign(&name, literal));
        statements
    }

    /// Starts a process (a declared one, one a name was bound to, or an
    /// inline one) and binds its handle.
    fn start(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        let definition = match (self.tape.pick(3), self.one_of(&ctx.of(Ty::Process))) {
            (0, Some(name)) => b::var(&name),
            (1, _) if ctx.literal_depth < 2 && ctx.depth < MAX_BODY_DEPTH => {
                self.process_literal(ctx)
            }
            _ => b::process_ref(&self.process(ctx)),
        };
        let handle = self.fresh();
        let value = self.any(ctx, 1);
        ctx.bind(&handle, Ty::Handle);
        vec![b::assign(
            &handle,
            b::module_call(
                &["processes"],
                "start",
                vec![b::record(vec![
                    ("definition", definition),
                    ("args", b::record(vec![("value", value)])),
                ])],
            ),
        )]
    }

    /// A statement for [`closed_statement`]: any statement that uses no
    /// declaration.
    fn closed(&mut self, ctx: &mut Ctx) -> Vec<Expr> {
        match self.tape.pick(8) {
            0 => {
                let name = self.fresh();
                let value = self.num(ctx, 1);
                ctx.bind(&name, Ty::Num);
                vec![b::assign(&name, value)]
            }
            1 => {
                let value = self.any(ctx, 1);
                vec![b::print(value)]
            }
            2 => {
                let name = self.fresh();
                let value = self.any(ctx, 1);
                let call = self.tool("echo", value);
                ctx.bind(&name, Ty::Any);
                vec![b::assign(&name, call)]
            }
            3 => {
                ctx.depth = MAX_BODY_DEPTH - 1;
                self.branch(ctx)
            }
            4 => {
                ctx.depth = MAX_BODY_DEPTH - 1;
                self.for_loop(ctx)
            }
            5 => {
                ctx.depth = MAX_BODY_DEPTH - 1;
                self.try_region(ctx)
            }
            6 => self.member_assignment(ctx),
            _ => {
                let name = self.fresh();
                let closure = self.closure(ctx, None);
                ctx.bind(&name, Ty::Closure);
                vec![b::assign(&name, closure)]
            }
        }
    }
}
