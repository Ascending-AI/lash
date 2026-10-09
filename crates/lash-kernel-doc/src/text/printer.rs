//! Writes kernel text.

use std::collections::BTreeMap;

use super::is_keyword;
use super::lexer::write_quoted;
use crate::ast::{Action, Atom, Block, Callee, Expr, JoinMode, Literal, Member, Place, Rhs, Stmt};
use crate::document::Document;
use crate::function::{Formula, FunctionDefinition, Operand};
use crate::name::{FunctionId, FunctionName, Name, is_identifier};
use crate::number::NumberPolicy;
use crate::types::{Signature, Type};

/// Writes a document as kernel text. [`super::parse_document`] reads the
/// text back to an equal document.
pub fn print_document(document: &Document) -> String {
    let mut printer = Printer::new(&document.manifest.functions);
    let out = &mut printer.out;
    out.push_str(&format!("kernel {}\n", document.manifest.kernel));
    out.push_str(match document.manifest.numbers {
        NumberPolicy::Float => "numbers float\n",
        NumberPolicy::BySpelling => "numbers by_spelling\n",
    });
    for (name, signature) in &document.manifest.effects {
        printer.out.push_str(&format!("effect {name}"));
        printer.signature(signature);
        printer.out.push('\n');
    }
    printer.uses(&document.manifest.functions);
    for (name, signature) in &document.entries {
        printer.out.push_str("entry ");
        printer.name(name);
        printer.signature(signature);
        printer.out.push('\n');
    }
    if !document.private_bindings.is_empty() {
        printer.out.push_str("private ");
        printer.names(document.private_bindings.iter());
        printer.out.push('\n');
    }
    for (name, function) in &document.functions {
        printer.out.push_str("\nfn ");
        printer.name(name);
        printer.out.push('(');
        printer.names(function.params.iter());
        printer.out.push_str(") ");
        printer.block(&function.body);
        printer.out.push('\n');
    }
    printer.out.push_str("\nmain ");
    printer.block(&document.main);
    printer.out.push('\n');
    printer.out
}

/// Writes a library-function definition as kernel text.
/// [`super::parse_definition`] reads the text back to an equal definition.
pub fn print_definition(definition: &FunctionDefinition) -> String {
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
    printer.out.push_str("charge ");
    printer.formula(&definition.charge);
    printer.out.push('\n');
    if let Some(guard) = &definition.guard {
        printer.out.push_str("guard ");
        write_quoted(&guard.unit, '"', &mut printer.out);
        printer.out.push(' ');
        printer.formula(&guard.limit);
        printer.out.push('\n');
    }
    if definition.has_native() {
        printer.out.push_str("native\n");
    }
    if let Some(body) = definition.body() {
        printer.uses(&body.functions);
        printer.out.push_str("body ");
        printer.block(&body.block);
        printer.out.push('\n');
    }
    printer.out
}

struct Printer<'a> {
    out: String,
    indent: usize,
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
        match stmt {
            Stmt::Let { name, value } => {
                self.out.push_str("let ");
                self.name(name);
                self.out.push_str(" = ");
                self.rhs(value);
            }
            Stmt::Assign { place, value } => {
                self.out.push_str("set ");
                match place {
                    Place::Variable(name) => self.name(name),
                    Place::Member(member) => self.member(member),
                }
                self.out.push_str(" = ");
                self.rhs(value);
            }
            Stmt::Remove { member } => {
                self.out.push_str("remove ");
                self.member(member);
            }
            Stmt::Do { action } => {
                self.out.push_str("do ");
                self.action(action);
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
                binding,
                iterable,
                body,
            } => {
                self.out.push_str("for ");
                self.name(binding);
                self.out.push_str(" in ");
                self.expr(iterable);
                self.out.push(' ');
                self.block(body);
            }
            Stmt::While { condition, body } => {
                self.out.push_str("while ");
                self.expr(condition);
                self.out.push(' ');
                self.block(body);
            }
            Stmt::Break => self.out.push_str("break"),
            Stmt::Continue => self.out.push_str("continue"),
            Stmt::Return { value } => self.keyword_expr("return", value),
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
                effect,
                args,
                result,
            } => {
                self.out.push_str(&format!("perform {effect}"));
                self.atoms(args);
                self.out.push_str(" as ");
                self.ty(result);
            }
            Action::Sleep { duration } => {
                self.out.push_str("sleep ");
                self.atom(duration);
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

    fn callee(&mut self, callee: &Callee, args: &[Atom]) {
        match callee {
            Callee::Declared(name) => {
                self.out.push_str("call ");
                self.name(name);
            }
            Callee::Value(name) => {
                self.out.push_str("apply ");
                self.name(name);
            }
            Callee::Library(function) => {
                self.out.push_str("invoke ");
                self.function(function);
            }
        }
        self.atoms(args);
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
            Literal::Function(name) => {
                self.out.push('&');
                self.name(name);
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
                self.out.push_str("fn(");
                self.names(closure.params.iter());
                self.out.push_str(") ");
                self.block(&closure.body);
            }
            Expr::Call { function, args } => {
                self.function(function);
                self.out.push('(');
                self.exprs(args.iter());
                self.out.push(')');
            }
            Expr::Clock => self.out.push_str("clock"),
            Expr::Random => self.out.push_str("random"),
            Expr::Read(read) => {
                self.out.push_str("read(");
                self.expr(&read.handle);
                self.out.push_str(", ");
                self.expr(&read.request);
                self.out.push(')');
            }
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

    fn types(&mut self, name: &str, types: &[Type]) {
        self.out.push_str(name);
        self.out.push('(');
        for (index, ty) in types.iter().enumerate() {
            if index > 0 {
                self.out.push_str(", ");
            }
            self.ty(ty);
        }
        self.out.push(')');
    }

    fn ty(&mut self, ty: &Type) {
        match ty {
            Type::Any => self.out.push_str("Any"),
            Type::Null => self.out.push_str("Null"),
            Type::Absent => self.out.push_str("Absent"),
            Type::Bool => self.out.push_str("Bool"),
            Type::Int => self.out.push_str("Int"),
            Type::Float => self.out.push_str("Float"),
            Type::Number => self.out.push_str("Number"),
            Type::Text => self.out.push_str("Text"),
            Type::Bytes => self.out.push_str("Bytes"),
            Type::Timestamp => self.out.push_str("Timestamp"),
            Type::Error => self.out.push_str("Error"),
            Type::Tuple(members) => self.types("Tuple", members),
            Type::Union(members) => self.types("Union", members),
            Type::List(item) => self.types("List", std::slice::from_ref(item)),
            Type::Set(item) => self.types("Set", std::slice::from_ref(item)),
            Type::Task(item) => self.types("Task", std::slice::from_ref(item)),
            Type::Map(map) => {
                self.out.push_str("Map(");
                self.ty(&map.key);
                self.out.push_str(", ");
                self.ty(&map.value);
                self.out.push(')');
            }
            Type::Handle(kind) => {
                self.out.push_str("Handle(");
                write_quoted(kind, '"', &mut self.out);
                self.out.push(')');
            }
            Type::Enum(members) => {
                self.out.push_str("Enum(");
                for (index, member) in members.iter().enumerate() {
                    if index > 0 {
                        self.out.push_str(", ");
                    }
                    write_quoted(member, '"', &mut self.out);
                }
                self.out.push(')');
            }
            Type::Function(signature) => {
                self.out.push_str("Fn");
                self.signature(signature);
            }
            Type::Record(record) => {
                self.out.push_str("Record{");
                let mut first = true;
                for field in &record.fields {
                    if !std::mem::take(&mut first) {
                        self.out.push_str(", ");
                    }
                    self.field_name(&field.name);
                    if field.optional {
                        self.out.push('?');
                    }
                    self.out.push_str(": ");
                    self.ty(&field.ty);
                }
                if let Some(rest) = &record.rest {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str("..");
                    self.ty(rest);
                }
                self.out.push('}');
            }
        }
    }

    fn formula(&mut self, formula: &Formula) {
        let (name, terms) = match formula {
            Formula::Constant(amount) => {
                self.out.push_str(&amount.to_string());
                return;
            }
            Formula::Size(operand) => return self.measure("size", operand),
            Formula::DeepSize(operand) => return self.measure("deep", operand),
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
