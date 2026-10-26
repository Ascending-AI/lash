//! Writes kernel text.

use std::collections::BTreeMap;

use crate::ast::{
    ActKind, Action, AssignOrder, Atom, Block, Callee, Expr, JoinMode, Literal, Member, Place, Rhs,
    Stmt,
};
use lash_kernel_value::type_text::write_quoted;

use crate::document::Document;
use crate::function::{Formula, FunctionDefinition, Operand};

use crate::function_value::{CallablePresentation, FunctionRepresentation, StartConvention};

use crate::number::NumberPolicy;
use lash_kernel_value::type_text::is_keyword;
use {
    lash_kernel_value::names::FunctionId, lash_kernel_value::names::FunctionName,
    lash_kernel_value::names::Name, lash_kernel_value::names::is_identifier,
};
use {lash_kernel_value::types::Signature, lash_kernel_value::types::Type};

/// Writes a document as kernel text. [`super::parse_document`] reads the
/// text back to an equal document.
pub fn print_document(document: &Document) -> String {
    print_document_with_ranges(document).0
}

/// Canonical text and exact byte ranges of every statement, including closure
/// bodies. Locations describe these printed bytes and this document only.
pub fn print_document_with_ranges(
    document: &Document,
) -> (String, BTreeMap<crate::Site, crate::Span>) {
    let names = document
        .functions
        .iter()
        .map(|(id, f)| (*id, f.name.clone()))
        .collect();
    let mut printer = Printer::new(&names);
    for kernel in &document.requires.kernels {
        printer.out.push_str(&format!("kernel {kernel}\n"));
    }
    printer.out.push_str(match document.numbers {
        NumberPolicy::Float => "numbers float\n",
        NumberPolicy::BySpelling => "numbers by_spelling\n",
    });
    printer.imports(&document.imports);
    for function in document.library.iter() {
        printer.out.push_str(&format!("library @{function}\n"));
    }
    for library in &document.requires.libraries {
        printer.out.push_str(&format!("helpers \"{library}\"\n"));
    }
    for native in &document.requires.natives {
        printer.out.push_str(&format!("native @{native}\n"));
    }
    for (name, export) in &document.exports {
        printer.out.push_str("export ");
        printer.name(name);
        printer.out.push(' ');
        write_quoted(
            &serde_json::to_string(export).unwrap_or_else(|_| unreachable!("serializable Export")),
            '"',
            &mut printer.out,
        );
        printer.out.push('\n');
    }
    for function in document.functions.values() {
        printer.out.push_str("definition ");
        let (source, ranges) = definition_with_ranges(function);
        // Statement spans can overlap (nested closures), so order their
        // boundaries before quoting. Each disjoint source slice is escaped
        // once with the canonical writer; ranges only look up its offsets.
        let mut offsets: BTreeMap<usize, usize> = ranges
            .values()
            .flat_map(|span| [span.start, span.end])
            .chain([source.len()])
            .map(|offset| (offset, 0))
            .collect();
        printer.out.push('"');
        let mut previous = 0;
        let mut quoted = String::new();
        for (offset, encoded) in &mut offsets {
            quoted.clear();
            quote_definition(&source[previous..*offset], &mut quoted);
            printer.out.push_str(&quoted[1..quoted.len() - 1]);
            *encoded = printer.out.len();
            previous = *offset;
        }
        printer.out.push('"');
        for (statement, span) in ranges {
            printer.ranges.insert(
                statement,
                crate::Span {
                    start: offsets[&span.start],
                    end: offsets[&span.end],
                },
            );
        }
        printer.out.push('\n');
    }
    if !document.private_bindings.is_empty() {
        printer.out.push_str("private ");
        printer.names(document.private_bindings.iter());
        printer.out.push('\n');
    }
    if let Some(main) = &document.main {
        printer.out.push_str("main ");
        printer.block(main);
        printer.out.push('\n');
    }
    let ranges = document
        .ordinal_nodes()
        .into_iter()
        .filter_map(|(site, node)| {
            let crate::Node::Stmt(stmt) = node else {
                return None;
            };
            printer
                .ranges
                .get(&std::ptr::from_ref(stmt))
                .map(|span| (site, *span))
        })
        .collect();
    (printer.out, ranges)
}

/// Writes a library-function definition as kernel text.
/// [`super::parse_definition`] reads the text back to an equal definition.
pub fn print_definition(definition: &FunctionDefinition) -> String {
    definition_with_ranges(definition).0
}
fn definition_with_ranges(
    definition: &FunctionDefinition,
) -> (String, BTreeMap<*const Stmt, crate::Span>) {
    let no_functions = BTreeMap::new();
    let functions = definition
        .body()
        .map_or(&no_functions, |body| &body.functions);
    let mut printer = Printer::new(functions);
    printer
        .out
        .push_str(&format!("function {}", definition.name));
    printer.signature(&definition.signature);
    printer
        .out
        .push_str(&format!("\nkernel {}\n", definition.kernel));
    if !definition.errors.is_empty() {
        printer.out.push_str("errors ");
        for (index, kind) in definition.errors.iter().enumerate() {
            if index > 0 {
                printer.out.push_str(", ");
            }
            write_quoted(kind, '"', &mut printer.out);
        }
        printer.out.push('\n');
    }
    if let Some(charge) = definition.charge() {
        printer.out.push_str("charge ");
        printer.formula(charge);
        printer.out.push('\n');
    } else {
        printer.out.push_str("metering statements\n");
    }
    if let Some(guard) = &definition.guard {
        printer.out.push_str("guard ");
        write_quoted(&guard.unit, '"', &mut printer.out);
        printer.out.push(' ');
        printer.formula(&guard.limit);
        printer.out.push('\n');
    }
    if definition.has_native() {
        printer.out.push_str("native");
        if definition.native_version != crate::FIRST_NATIVE_VERSION {
            printer
                .out
                .push_str(&format!(" {}", definition.native_version));
        }
        printer.out.push('\n');
    }
    if let Some(body) = definition.body() {
        printer.imports(&body.imports);
        printer.uses(&body.functions);
        printer.out.push_str("body ");
        printer.block(&body.block);
        printer.out.push('\n');
    }
    (printer.out, printer.ranges)
}

struct Printer<'a> {
    out: String,
    indent: usize,
    ranges: BTreeMap<*const Stmt, crate::Span>,
    /// The functions a call may be written by name: those whose name one
    /// `use` line alone gives.
    named: BTreeMap<FunctionId, &'a FunctionName>,
}

impl<'a> Printer<'a> {
    fn new(functions: &'a BTreeMap<FunctionId, FunctionName>) -> Self {
        let mut uses: BTreeMap<&FunctionName, usize> = BTreeMap::new();
        for name in functions.values() {
            *uses.entry(name).or_default() += 1;
        }
        let named = functions
            .iter()
            // `fn(` and `read(` begin other expressions.
            .filter(|(_, name)| uses[name] == 1 && !matches!(name.as_str(), "fn" | "read"))
            .map(|(function, name)| (*function, name))
            .collect();
        Self {
            out: String::new(),
            indent: 0,
            ranges: BTreeMap::new(),
            named,
        }
    }

    fn uses(&mut self, functions: &BTreeMap<FunctionId, FunctionName>) {
        let mut lines: Vec<(&FunctionName, &FunctionId)> = functions
            .iter()
            .map(|(function, name)| (name, function))
            .collect();
        lines.sort();
        for (name, function) in lines {
            self.out.push_str(&format!("use {name} = @{function}\n"));
        }
    }

    fn name(&mut self, name: &Name) {
        let text = name.as_str();
        if is_identifier(text) && !is_keyword(text) {
            self.out.push_str(text);
        } else {
            write_quoted(text, '`', &mut self.out);
        }
    }

    fn names<'n>(&mut self, names: impl Iterator<Item = &'n Name>) {
        for (index, name) in names.enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.name(name);
        }
    }

    fn field_name(&mut self, field: &str) {
        if is_identifier(field) {
            self.out.push_str(field);
        } else {
            write_quoted(field, '"', &mut self.out);
        }
    }

    fn line(&mut self) {
        self.out.push('\n');
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
    }

    fn block(&mut self, block: &Block) {
        if block.is_empty() {
            self.out.push_str("{}");
            return;
        }
        self.out.push('{');
        self.indent += 1;
        for stmt in block {
            self.line();
            self.stmt(stmt);
        }
        self.indent -= 1;
        self.line();
        self.out.push('}');
    }

    fn stmt(&mut self, stmt: &Stmt) {
        let start = self.out.len();
        self.statement(stmt);
        self.ranges.insert(
            std::ptr::from_ref(stmt),
            crate::Span {
                start,
                end: self.out.len(),
            },
        );
    }

    fn statement(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let { name, value } => {
                self.out.push_str("let ");
                self.name(name);
                self.out.push_str(" = ");
                self.rhs(value);
            }
            Stmt::Assign {
                place,
                value,
                order,
            } => {
                self.out.push_str("set ");
                match place {
                    Place::Variable(name) => self.name(name),
                    Place::Member(member) => self.member(member),
                }
                self.out.push_str(" = ");
                self.rhs(value);
                if *order == AssignOrder::PlaceFirst {
                    self.out.push_str(" place first");
                }
            }
            Stmt::Remove { member } => {
                self.out.push_str("remove ");
                self.member(member);
            }
            Stmt::Do { value } => {
                self.out.push_str("do ");
                self.expr(value);
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.out.push_str("if ");
                self.expr(condition);
                self.out.push(' ');
                self.block(then_block);
                if !else_block.is_empty() {
                    self.out.push_str(" else ");
                    self.block(else_block);
                }
            }
            Stmt::For {
                metered,
                binding,
                iterable,
                via,
                body,
                else_block,
            } => {
                self.out.push_str("for ");
                self.name(binding);
                self.out.push_str(" in ");
                self.expr(iterable);
                if let Some(via) = via {
                    self.out.push_str(" via (open ");
                    self.target(&via.open);
                    for (word, hook) in [
                        ("next", &via.next),
                        ("item", &via.item),
                        ("check", &via.check),
                    ] {
                        if let Some(hook) = hook {
                            self.out.push_str(", ");
                            self.out.push_str(word);
                            self.out.push(' ');
                            self.target(hook);
                        }
                    }
                    self.out.push(')');
                }
                self.out.push(' ');
                if *metered != 0 {
                    self.out.push_str(&format!("metered {metered} "));
                }
                self.block(body);
                self.loop_else(else_block);
            }
            Stmt::While {
                metered,
                condition,
                body,
                else_block,
            } => {
                self.out.push_str("while ");
                self.expr(condition);
                self.out.push(' ');
                if *metered != 0 {
                    self.out.push_str(&format!("metered {metered} "));
                }
                self.block(body);
                self.loop_else(else_block);
            }
            Stmt::Break => self.out.push_str("break"),
            Stmt::Continue => self.out.push_str("continue"),
            Stmt::Return { value } => self.keyword_expr("return", value),
            Stmt::ReturnVia { value, policy } => {
                self.keyword_expr("return", value);
                self.out.push_str(" via ");
                self.target(policy);
            }
            Stmt::Throw { value } => self.keyword_expr("throw", value),
            Stmt::Print { value } => self.keyword_expr("print", value),
            Stmt::Finish { value } => self.keyword_expr("finish", value),
            Stmt::Fail { value } => self.keyword_expr("fail", value),
            Stmt::Try(scope) => {
                self.out.push_str("try ");
                self.block(&scope.body);
                if let Some(catch) = &scope.catch {
                    self.out.push_str(" catch ");
                    self.name(&catch.binding);
                    self.out.push(' ');
                    self.block(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    self.out.push_str(" finally ");
                    self.block(finally);
                }
            }
        }
    }

    fn keyword_expr(&mut self, keyword: &str, value: &Expr) {
        self.out.push_str(keyword);
        self.out.push(' ');
        self.expr(value);
    }

    fn rhs(&mut self, rhs: &Rhs) {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr),
            Rhs::Action(action) => self.action(action),
        }
    }

    fn action(&mut self, action: &Action) {
        match action {
            Action::Call { callee, args } => self.callee(callee, args),
            Action::Spawn { callee, args } => {
                self.out.push_str("spawn ");
                self.callee(callee, args);
            }
            Action::Perform {
                slot,
                args,
                result,
                then_yield,
            } => {
                self.out.push_str(&format!("perform {}", slot.0));
                self.atoms(args);
                self.out.push_str(" as ");
                self.ty(result);
                self.then_yield(*then_yield);
            }
            Action::Await { policy, value } => {
                self.out.push_str("await ");
                self.atom(value);
                self.out.push_str(" via ");
                self.target(policy);
            }
            Action::Join { task } => {
                self.out.push_str("join ");
                self.atom(task);
            }
            Action::JoinMany { mode, tasks } => {
                self.out.push_str(match mode {
                    JoinMode::All => "join all ",
                    JoinMode::AllSettled => "join settled ",
                    JoinMode::Race => "join race ",
                    JoinMode::Any => "join any ",
                });
                self.atom(tasks);
            }
            Action::Yield => self.out.push_str("yield"),
            Action::Cancel { task } => {
                self.out.push_str("cancel ");
                self.atom(task);
            }
        }
    }

    fn then_yield(&mut self, then_yield: bool) {
        if then_yield {
            self.out.push_str(" then yield");
        }
    }

    fn callee(&mut self, callee: &Callee, args: &[Atom]) {
        self.callee_name(callee);
        self.atoms(args);
    }

    fn callee_name(&mut self, callee: &Callee) {
        match callee {
            Callee::Import(slot) => self.out.push_str(&format!("invoke import {}", slot.0)),
            Callee::Value(name) => {
                self.out.push_str("apply ");
                self.name(name);
            }
            Callee::Local(function) => {
                self.out.push_str("invoke ");
                self.function(function);
            }
        }
    }

    fn loop_else(&mut self, else_block: &Block) {
        if !else_block.is_empty() {
            self.out.push_str(" else ");
            self.block(else_block);
        }
    }

    fn function(&mut self, function: &FunctionId) {
        match self.named.get(function) {
            Some(name) => self.out.push_str(name.as_str()),
            None => self.out.push_str(&format!("@{function}")),
        }
    }

    fn atoms(&mut self, atoms: &[Atom]) {
        self.out.push('(');
        for (index, atom) in atoms.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.atom(atom);
        }
        self.out.push(')');
    }

    fn atom(&mut self, atom: &Atom) {
        match atom {
            Atom::Variable(name) => self.name(name),
            Atom::Literal(literal) => self.literal(literal),
        }
    }

    fn literal(&mut self, literal: &Literal) {
        match literal {
            Literal::Null => self.out.push_str("null"),
            Literal::Absent => self.out.push_str("absent"),
            Literal::Bool(true) => self.out.push_str("true"),
            Literal::Bool(false) => self.out.push_str("false"),
            Literal::Int(value) => self.out.push_str(&value.to_string()),
            Literal::Float(value) => self.out.push_str(&value.to_string()),
            Literal::Text(text) => write_quoted(text, '"', &mut self.out),
            Literal::Bytes(bytes) => {
                self.out.push('b');
                write_quoted(&bytes.to_hex(), '"', &mut self.out);
            }
            Literal::Function(id) => {
                self.out.push('&');
                self.function(id);
            }
        }
    }

    fn exprs<'e>(&mut self, exprs: impl Iterator<Item = &'e Expr>) {
        for (index, expr) in exprs.enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.expr(expr);
        }
    }

    fn member(&mut self, member: &Member) {
        match member {
            Member::Field { target, field } => {
                self.expr(target);
                self.out.push('.');
                self.field_name(field);
            }
            Member::Index { target, index } => {
                self.expr(target);
                self.out.push('[');
                self.expr(index);
                self.out.push(']');
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Literal(literal) => self.literal(literal),
            Expr::Variable(name) => self.name(name),
            Expr::Tuple(items) => {
                self.out.push('(');
                self.exprs(items.iter());
                if items.len() == 1 {
                    self.out.push(',');
                }
                self.out.push(')');
            }
            Expr::List(items) => {
                self.out.push('[');
                self.exprs(items.iter());
                self.out.push(']');
            }
            Expr::Set(items) => {
                self.out.push_str("set{");
                self.exprs(items.iter());
                self.out.push('}');
            }
            Expr::Map(entries) => {
                self.out.push_str("map{");
                for (index, entry) in entries.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(", ");
                    }
                    self.expr(&entry.key);
                    self.out.push_str(": ");
                    self.expr(&entry.value);
                }
                self.out.push('}');
            }
            Expr::Record(entries) => {
                self.out.push('{');
                for (index, entry) in entries.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(", ");
                    }
                    self.field_name(&entry.field);
                    self.out.push_str(": ");
                    self.expr(&entry.value);
                }
                self.out.push('}');
            }
            Expr::Member(member) => self.member(member),
            Expr::Closure(closure) => {
                self.out.push_str("fn ");
                self.function(&closure.function);
                self.start(closure.presentation.as_ref());
            }
            Expr::Call { function, args } => {
                self.function(function);
                self.out.push('(');
                self.exprs(args.iter());
                self.out.push(')');
            }
            Expr::If { cond, then, r#else } => {
                self.out.push_str("if(");
                self.expr(cond);
                self.out.push_str(", ");
                self.expr(then);
                self.out.push_str(", ");
                self.expr(r#else);
                self.out.push(')');
            }
            Expr::Let { name, value, body } => {
                self.out.push_str("let(");
                self.name(name);
                self.out.push_str(" = ");
                self.expr(value);
                self.out.push_str(", ");
                self.expr(body);
                self.out.push(')');
            }
            Expr::Act(act) => match &act.kind {
                ActKind::Call(callee) => {
                    self.callee_name(callee);
                    self.out.push('(');
                    self.exprs(act.args.iter());
                    self.out.push(')');
                }
                ActKind::Spawn(callee) => {
                    self.out.push_str("spawn ");
                    self.callee_name(callee);
                    self.out.push('(');
                    self.exprs(act.args.iter());
                    self.out.push(')');
                }
                ActKind::Perform {
                    slot,
                    result,
                    then_yield,
                } => {
                    self.out.push_str(&format!("perform {}(", slot.0));
                    self.exprs(act.args.iter());
                    self.out.push_str(") as ");
                    self.ty(result);
                    self.then_yield(*then_yield);
                }
                ActKind::Join => {
                    self.out.push_str("join(");
                    self.exprs(act.args.iter());
                    self.out.push(')');
                }
                ActKind::JoinMany(mode) => {
                    self.out.push_str(match mode {
                        JoinMode::All => "join all(",
                        JoinMode::AllSettled => "join settled(",
                        JoinMode::Race => "join race(",
                        JoinMode::Any => "join any(",
                    });
                    self.exprs(act.args.iter());
                    self.out.push(')');
                }
                ActKind::Yield => self.out.push_str("yield"),
                ActKind::Cancel => {
                    self.out.push_str("cancel(");
                    self.exprs(act.args.iter());
                    self.out.push(')');
                }
                ActKind::Await(policy) => {
                    self.out.push_str("await(");
                    self.exprs(act.args.iter());
                    self.out.push_str(") via ");
                    self.target(policy);
                }
            },
        }
    }

    /// A start adapter after a function's parameters (`K-CLO-004`).
    fn start(&mut self, start: Option<&CallablePresentation>) {
        let Some(start) = start else {
            return;
        };
        self.out.push_str(" presentation");
        self.signature(&start.signature);
        match &start.representation {
            FunctionRepresentation::Bare => self.out.push_str(" bare"),
            FunctionRepresentation::Token { tag, callable } => {
                self.out.push_str(" token(");
                write_quoted(tag, '"', &mut self.out);
                self.out.push_str(&format!(", {callable})"));
            }
        }
        self.convention(&start.convention);
    }

    fn convention(&mut self, convention: &StartConvention) {
        match convention {
            StartConvention::Direct => self.out.push_str(" direct"),
            StartConvention::PyArgsList { positional } => {
                self.out.push_str(&format!(" py_args({positional})"))
            }
            StartConvention::TsDirect { await_fn } => {
                self.out.push_str(" ts_direct(");
                self.target(await_fn);
                self.out.push(')');
            }
            StartConvention::TsArgsList { await_fn, same_fn } => {
                self.out.push_str(" ts_args(");
                self.target(await_fn);
                self.out.push_str(", ");
                self.target(same_fn);
                self.out.push(')');
            }
            StartConvention::PyDirect { cleanup } => {
                self.out.push_str(" py_direct(");
                self.target(cleanup);
                self.out.push(')');
            }
        }
    }

    fn target(&mut self, target: &crate::FunctionTarget) {
        match target {
            crate::FunctionTarget::Local(id) => self.function(id),
            crate::FunctionTarget::Import(slot) => self.out.push_str(&format!("import {}", slot.0)),
        }
    }
    fn imports(
        &mut self,
        imports: &BTreeMap<lash_kernel_value::names::ImportSlot, lash_kernel_value::names::Import>,
    ) {
        use {lash_kernel_value::names::Convention, lash_kernel_value::names::ImportKind};
        for (slot, import) in imports {
            let name = format!("{}.{}", import.name.package, import.name.member);
            self.out.push_str(&format!("import {} ", slot.0));
            match &import.kind {
                ImportKind::Operation { signature } => {
                    self.out.push_str(&format!("operation {name}"));
                    self.signature(signature);
                }
                ImportKind::Code {
                    signature,
                    convention,
                } => {
                    self.out.push_str(&format!("code {name}"));
                    self.signature(signature);
                    self.out.push_str(match convention {
                        Convention::Direct => " direct",
                        Convention::TsDirect => " ts_direct",
                        Convention::TsArgsList => " ts_args",
                        Convention::PyDirect => " py_direct",
                        Convention::PyArgsList => " py_args",
                    });
                }
                ImportKind::Type { .. } => self.out.push_str(&format!("type {name}")),
                ImportKind::Error { .. } => self.out.push_str(&format!("error {name}")),
            }
            self.out.push('\n');
        }
    }
    fn signature(&mut self, signature: &Signature) {
        self.out.push('(');
        for (index, param) in signature.params.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.name(&param.name);
            if param.optional {
                self.out.push('?');
            }
            self.out.push_str(": ");
            self.ty(&param.ty);
        }
        self.out.push_str(") -> ");
        self.ty(&signature.result);
    }

    fn ty(&mut self, ty: &Type) {
        self.out.push_str(&lash_kernel_value::print_type(ty));
    }

    fn formula(&mut self, formula: &Formula) {
        let (name, terms): (&str, &[Formula]) = match formula {
            Formula::CeilLog2(term) => ("ceil_log2", std::slice::from_ref(term.as_ref())),
            Formula::Constant(amount) => {
                self.out.push_str(&amount.to_string());
                return;
            }
            Formula::Size(operand) => return self.measure("size", operand),
            Formula::DeepSize(operand) => return self.measure("deep", operand),
            Formula::NestedSize(operand) => return self.measure("nested", operand),
            Formula::Magnitude(operand) => return self.measure("magnitude", operand),
            Formula::Sum(terms) => ("sum", terms),
            Formula::Product(terms) => ("product", terms),
            Formula::Max(terms) => ("max", terms),
            Formula::Min(terms) => ("min", terms),
        };
        self.out.push_str(name);
        self.out.push('(');
        for (index, term) in terms.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.formula(term);
        }
        self.out.push(')');
    }

    fn measure(&mut self, name: &str, operand: &Operand) {
        self.out.push_str(name);
        self.out.push('(');
        match operand {
            Operand::Result => self.out.push_str("result"),
            Operand::Param(name) => self.name(name),
        }
        self.out.push(')');
    }
}

#[cfg(test)]
thread_local! {
    static DEFINITION_QUOTE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn quote_definition(source: &str, out: &mut String) {
    #[cfg(test)]
    DEFINITION_QUOTE_BYTES.with(|bytes| bytes.set(bytes.get() + source.len()));
    write_quoted(source, '"', out);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7135: mapping statement spans into a quoted definition must scan
    /// its source once, independent of how many statement boundaries it has.
    #[test]
    fn definition_range_mapping_quotes_each_source_byte_once() {
        let definition = crate::parse_definition(&format!(
            "function probe() -> Any\nkernel 1\nmetering statements\nbody {{ {} return null }}",
            r#"print "é\n\t\\\"" "#.repeat(128),
        ))
        .unwrap();
        let (source, original_ranges) = definition_with_ranges(&definition);
        let mut document = Document::new(NumberPolicy::Float, Vec::new());
        document.functions.insert(definition.identity(), definition);
        DEFINITION_QUOTE_BYTES.with(|bytes| bytes.set(0));
        let (text, ranges) = print_document_with_ranges(&document);
        let quoted = DEFINITION_QUOTE_BYTES.with(|bytes| bytes.get());
        assert_eq!(crate::parse_document(&text).unwrap(), document);
        assert_eq!(ranges.len(), 129);
        let base = text.find("definition ").unwrap() + "definition ".len();
        let expected: std::collections::BTreeSet<_> = original_ranges
            .values()
            .map(|span| {
                let offset = |boundary| {
                    let mut prefix = String::new();
                    write_quoted(&source[..boundary], '"', &mut prefix);
                    base + prefix.len() - 1
                };
                (offset(span.start), offset(span.end))
            })
            .collect();
        assert_eq!(
            ranges
                .values()
                .map(|span| (span.start, span.end))
                .collect::<std::collections::BTreeSet<_>>(),
            expected,
        );
        for span in ranges.values() {
            let statement = &text[span.start..span.end];
            assert!(
                statement.starts_with("print ") || statement == "return null",
                "{statement}"
            );
        }
        assert_eq!(
            quoted,
            source.len(),
            "statement boundaries must not re-quote source prefixes"
        );
    }
}
