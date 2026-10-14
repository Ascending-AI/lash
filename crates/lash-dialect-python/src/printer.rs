//! Exact Python spelling; paired inverses live in generated.rs.
use crate::Python;
use lash_kernel_dialect::{
    NamePlan, PrintError, PrintOptions, PrintedSource, Printer, SourceFormat, SourceWriter,
    check_document_depth,
};
use lash_kernel_doc as k;

/// Render exact generated Python without compiling or resolving dependencies.
pub fn print(document: &k::Document, options: PrintOptions) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    if k::KernelVersion::of(document.manifest.kernel).is_none() {
        return Err(PrintError::Invalid("unsupported kernel version".into()));
    }
    let plan = NamePlan::new(document, legal_identifier);
    let inline = InlinePlan::new(document);
    let mut r = Renderer {
        out: SourceWriter::new(options),
        plan: &plan,
        inline: &inline,
        indent: 0,
        bound: Default::default(),
        main: false,
        effects: &document.manifest.effects,
        operators: crate::generated_operators::Operators::new(&document.manifest.functions),
    };
    r.out.write("k.document(\"lash.generated.python\", \"1\", ");
    r.metadata(&document.manifest);
    r.out.write(", ");
    r.metadata(&document.entries);
    r.out.write(", ");
    r.out.json(&document.private_bindings);
    r.out.write(", ");
    let names: Vec<_> = plan.names.iter().map(|(n, a)| (a, n)).collect();
    r.metadata(&names);
    r.out.write(", ");
    let helpers: Vec<_> = plan.helpers.iter().map(|(id, a)| (a, id)).collect();
    r.metadata(&helpers);
    r.out.write(")\n");
    for (name, function) in &document.functions {
        r.out.write("def ");
        r.name(name);
        r.parameters(&function.params);
        r.out.write(":\n");
        let unit = options.source_map.then(|| k::Unit::Function(name.clone()));
        r.bound = function.params.iter().cloned().collect();
        r.root_block(&function.body, unit.as_ref());
    }
    r.bound.clear();
    r.main = true;
    r.out.write("def main():\n");
    r.root_block(&document.main, options.source_map.then_some(&k::Unit::Main));
    r.out.finish(document, SourceFormat::PythonV1)
}
impl Printer for Python {
    fn print(
        &self,
        document: &k::Document,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        print(document, options)
    }
}

pub(crate) fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || unicode_ident::is_xid_start(c))
        && chars.all(unicode_ident::is_xid_continue)
}
pub(crate) fn attribute_identifier(name: &str) -> bool {
    identifier(name)
        && !matches!(
            name,
            "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        )
}
pub(crate) fn legal_identifier(name: &str) -> bool {
    identifier(name)
        && !name.starts_with("helper_")
        && !name.starts_with("kernel_name_")
        && !matches!(
            name,
            "k" | "main"
                | "Final"
                | "reassigned"
                | "tools"
                | "record"
                | "finish"
                | "failure"
                | "returned"
                | "declared"
                | "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
                | "match"
                | "case"
                | "type"
                | "__debug__"
                | "Ellipsis"
                | "NotImplemented"
        )
        && !BUILTINS.contains(&name)
}
// Python 3.13 builtins are reserved, including unsupported authored builtins.
const BUILTINS: &[&str] = &[
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "Ellipsis",
    "EncodingWarning",
    "EnvironmentError",
    "Exception",
    "ExceptionGroup",
    "False",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "None",
    "NotADirectoryError",
    "NotImplemented",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "PythonFinalizationError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "True",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "ZeroDivisionError",
    "__build_class__",
    "__debug__",
    "__doc__",
    "__import__",
    "__loader__",
    "__name__",
    "__package__",
    "__spec__",
    "abs",
    "aiter",
    "all",
    "anext",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "copyright",
    "credits",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "exit",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "license",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "quit",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];
struct Renderer<'a> {
    out: SourceWriter,
    plan: &'a NamePlan,
    inline: &'a InlinePlan,
    indent: usize,
    bound: std::collections::BTreeSet<k::Name>,
    main: bool,
    effects: &'a std::collections::BTreeMap<k::EffectName, k::Signature>,
    operators: crate::generated_operators::Operators,
}
impl Renderer<'_> {
    fn parameters(&mut self, names: &[k::Name]) {
        self.out.write("(");
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.name(name);
        }
        self.out.write(")");
    }
    fn root_block(&mut self, block: &k::Block, root: Option<&k::Unit>) {
        self.out.begin_node(root);
        self.indent += 1;
        if block.is_empty() {
            self.empty();
        }
        for stmt in block {
            self.node(k::Node::Stmt(stmt), None);
        }
        self.indent -= 1;
        self.out.end_node();
    }
    fn inline_rhs(&self, rhs: &k::Rhs) -> bool {
        match rhs {
            k::Rhs::Expr(e) => self.inline.contains(e),
            k::Rhs::Action(_) => true,
        }
    }
    fn write_inline_rhs(&mut self, rhs: &k::Rhs) {
        match rhs {
            k::Rhs::Expr(e) => self.inline_expr(e),
            k::Rhs::Action(a) => {
                self.out.begin_node(None);
                self.action(a);
                self.out.end_node();
            }
        }
    }
    fn inline_member(&self, member: &k::Member) -> bool {
        match member {
            k::Member::Field { target, .. } => self.inline.contains(target),
            k::Member::Index { target, index } => {
                self.inline.contains(target) && self.inline.contains(index)
            }
        }
    }
    fn write_member(&mut self, member: &k::Member) {
        match member {
            k::Member::Field { target, field } => {
                let grouped = !matches!(
                    target,
                    k::Expr::Variable(_) | k::Expr::Member(_) | k::Expr::Call { .. }
                );
                if grouped {
                    self.out.write("(");
                }
                self.inline_expr(target);
                if grouped {
                    self.out.write(")");
                }
                self.out.write(".");
                self.out.write(field);
            }
            k::Member::Index { target, index } => {
                self.inline_expr(target);
                self.out.write("[");
                self.inline_expr(index);
                self.out.write("]");
            }
        }
    }
    fn inline_expr(&mut self, expr: &k::Expr) {
        self.out.begin_node(None);
        self.inline_body(expr);
        self.out.end_node();
    }
    fn inline_body(&mut self, expr: &k::Expr) {
        match expr {
            k::Expr::Literal(l) => self.literal(l),
            k::Expr::Variable(n) => self.name(n),
            k::Expr::Clock => self.out.write("k.clock()"),
            k::Expr::Random => self.out.write("k.random()"),
            k::Expr::Call { function, args } => {
                if let Some(token) = self.operators.token(function, args.len()) {
                    match token {
                        "not" | "neg" => {
                            self.out.write(if token == "not" { "not (" } else { "-(" });
                            self.inline_expr(&args[0]);
                            self.out.write(")");
                        }
                        _ => {
                            self.out.write("(");
                            self.inline_expr(&args[0]);
                            self.out.write(" ");
                            self.out.write(token);
                            self.out.write(" ");
                            self.inline_expr(&args[1]);
                            self.out.write(")");
                        }
                    }
                } else {
                    self.helper(function);
                    self.out.write("(");
                    for (i, arg) in args.iter().enumerate() {
                        if i > 0 {
                            self.out.write(", ");
                        }
                        self.inline_expr(arg);
                    }
                    self.out.write(")");
                }
            }
            k::Expr::List(items) | k::Expr::Tuple(items) | k::Expr::Set(items) => {
                let (open, close) = match expr {
                    k::Expr::List(_) => ("[", "]"),
                    k::Expr::Tuple(_) => ("(", ")"),
                    _ if items.is_empty() => ("set(", ")"),
                    _ => ("{", "}"),
                };
                self.out.write(open);
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.inline_expr(item);
                }
                if matches!(expr, k::Expr::Tuple(_)) && items.len() == 1 {
                    self.out.write(",");
                }
                self.out.write(close);
            }
            k::Expr::Map(items) => {
                self.out.write("{");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.inline_expr(&item.key);
                    self.out.write(": ");
                    self.inline_expr(&item.value);
                }
                self.out.write("}");
            }
            k::Expr::Record(items) => {
                self.out.write("record({");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.write(", ");
                    }
                    self.out.json(&item.field);
                    self.out.write(": ");
                    self.inline_expr(&item.value);
                }
                self.out.write("})");
            }
            k::Expr::Member(member) if matches!(member.as_ref(), k::Member::Field { field, .. } if !attribute_identifier(field)) =>
            {
                let k::Member::Field { target, field } = member.as_ref() else {
                    unreachable!()
                };
                self.out.write("getattr(");
                self.inline_expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(")");
            }
            k::Expr::Member(member) => {
                let target = match member.as_ref() {
                    k::Member::Field { target, .. } | k::Member::Index { target, .. } => target,
                };
                let grouped = !matches!(
                    target,
                    k::Expr::Variable(_) | k::Expr::Member(_) | k::Expr::Call { .. }
                );
                if grouped {
                    self.out.write("(");
                }
                self.inline_expr(target);
                if grouped {
                    self.out.write(")");
                }
                match member.as_ref() {
                    k::Member::Field { field, .. } => {
                        self.out.write(".");
                        self.out.write(field);
                    }
                    k::Member::Index { index, .. } => {
                        self.out.write("[");
                        self.inline_expr(index);
                        self.out.write("]");
                    }
                }
            }
            _ => self
                .out
                .refuse(PrintError::Invalid("ineligible inline expression".into())),
        }
    }
    // The caller owns the block's indentation and any clause label. Empty
    // suites spell pass, so every real block has a nonempty final-text range.
    fn readable_block(&mut self, block: &k::Block) {
        self.out.begin_node(None);
        self.indent += 1;
        if block.is_empty() {
            self.empty();
        }
        for stmt in block {
            self.node(k::Node::Stmt(stmt), None);
        }
        self.indent -= 1;
        self.out.end_node();
    }
    fn readable_if(
        &mut self,
        condition: &k::Expr,
        then_block: &k::Block,
        else_block: &k::Block,
        keyword: &str,
    ) {
        self.out.write(keyword);
        self.out.write(" ");
        self.inline_expr(condition);
        self.out.write(":\n");
        self.readable_block(then_block);
        // An elif is exactly one nested If in the else Block. Keep both map
        // frames; this changes layout, never the stored statement boundaries.
        if let [
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = else_block.as_slice()
            && self.inline.contains(condition)
        {
            self.out.begin_node(None); // else Block
            self.out.begin_node(None); // nested If
            self.padding();
            self.readable_if(condition, then_block, else_block, "elif");
            self.out.end_node();
            self.out.end_node();
        } else {
            self.padding();
            self.out.write("else:\n");
            self.readable_block(else_block);
        }
    }
    fn readable_statement(&mut self, stmt: &k::Stmt) -> bool {
        match stmt {
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Closure(c)),
            }
            | k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(k::Expr::Closure(c)),
            } => {
                self.out.write("def ");
                self.name(name);
                self.out.begin_node(None); // Closure expression
                self.parameters(&c.params);
                let was_bound = self.bound.contains(name);
                let is_let = matches!(stmt, k::Stmt::Let { .. });
                self.bound.insert(name.clone());
                if is_let == was_bound {
                    self.out.write(if is_let {
                        " -> Final"
                    } else {
                        " -> reassigned"
                    });
                }
                let parent = std::mem::replace(&mut self.bound, c.params.iter().cloned().collect());
                let outer_main = self.main;
                self.main = false;
                self.out.write(":\n");
                self.readable_block(&c.body);
                self.main = outer_main;
                self.bound = parent;
                self.out.end_node();
            }
            k::Stmt::Let { name, value } if self.inline_rhs(value) => {
                self.name(name);
                if !self.bound.insert(name.clone()) {
                    self.out.write(": Final");
                }
                self.out.write(" = ");
                self.write_inline_rhs(value);
                self.out.write("\n");
            }
            k::Stmt::Assign {
                place: k::Place::Member(k::Member::Field { target, field }),
                value,
            } if !attribute_identifier(field)
                && self.inline.contains(target)
                && self.inline_rhs(value) =>
            {
                self.out.write("setattr(");
                self.inline_expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(", ");
                self.write_inline_rhs(value);
                self.out.write(")\n");
            }
            k::Stmt::Assign { place, value }
                if self.inline_rhs(value)
                    && match place {
                        k::Place::Variable(_) => true,
                        k::Place::Member(m) => self.inline_member(m),
                    } =>
            {
                match place {
                    k::Place::Variable(n) => {
                        self.name(n);
                        if self.bound.insert(n.clone()) {
                            self.out.write(": reassigned");
                        }
                    }
                    k::Place::Member(m) => self.write_member(m),
                }
                self.out.write(" = ");
                self.write_inline_rhs(value);
                self.out.write("\n");
            }
            k::Stmt::Remove {
                member: k::Member::Field { target, field },
            } if !attribute_identifier(field) && self.inline.contains(target) => {
                self.out.write("delattr(");
                self.inline_expr(target);
                self.out.write(", ");
                self.out.json(field);
                self.out.write(")\n");
            }
            k::Stmt::Remove { member } if self.inline_member(member) => {
                self.out.write("del ");
                self.write_member(member);
                self.out.write("\n");
            }
            k::Stmt::Do { action } => {
                self.out.begin_node(None);
                self.action(action);
                self.out.end_node();
                self.out.write("\n");
            }
            k::Stmt::Break => self.out.write("break\n"),
            k::Stmt::Continue => self.out.write("continue\n"),
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            } if self.inline.contains(condition) => {
                self.readable_if(condition, then_block, else_block, "if");
            }
            k::Stmt::For {
                binding,
                iterable,
                body,
            } if self.inline.contains(iterable) => {
                self.out.write("for ");
                self.name(binding);
                self.bound.insert(binding.clone());
                self.out.write(" in ");
                self.inline_expr(iterable);
                self.out.write(":\n");
                self.readable_block(body);
            }
            k::Stmt::While { condition, body } if self.inline.contains(condition) => {
                self.out.write("while ");
                self.inline_expr(condition);
                self.out.write(":\n");
                self.readable_block(body);
            }
            k::Stmt::Try(scope) if scope.catch.is_some() || scope.finally.is_some() => {
                self.out.write("try:\n");
                self.readable_block(&scope.body);
                if let Some(catch) = &scope.catch {
                    self.padding();
                    self.out.write("except BaseException as ");
                    self.name(&catch.binding);
                    self.bound.insert(catch.binding.clone());
                    self.out.write(":\n");
                    self.readable_block(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    self.padding();
                    self.out.write("finally:\n");
                    self.readable_block(finally);
                }
            }
            k::Stmt::Return { value }
            | k::Stmt::Throw { value }
            | k::Stmt::Print { value }
            | k::Stmt::Finish { value }
            | k::Stmt::Fail { value }
                if self.inline.contains(value) =>
            {
                let call = match stmt {
                    k::Stmt::Return { .. } => self.main,
                    k::Stmt::Finish { .. } => !self.main,
                    k::Stmt::Throw { .. } => false,
                    _ => true,
                };
                self.out.write(match stmt {
                    k::Stmt::Return { .. } if self.main => "return returned(",
                    k::Stmt::Return { .. } => "return ",
                    k::Stmt::Throw { .. } => "raise ",
                    k::Stmt::Print { .. } => "print(",
                    k::Stmt::Finish { .. } if self.main => "return ",
                    k::Stmt::Finish { .. } => "return finish(",
                    _ => "raise failure(",
                });
                self.inline_expr(value);
                if call {
                    self.out.write(")");
                }
                self.out.write("\n");
            }
            _ => return false,
        }
        true
    }
    fn metadata(&mut self, value: &impl serde::Serialize) {
        self.out.metadata(value, SourceFormat::PythonV1);
    }
    fn name(&mut self, n: &k::Name) {
        self.out.write(&self.plan.names[n]);
    }
    fn helper(&mut self, id: &k::FunctionId) {
        self.out.write(&self.plan.helpers[id]);
    }
    fn names(&mut self, names: &[k::Name]) {
        self.out.write("[");
        for (i, n) in names.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.name(n);
        }
        self.out.write("]");
    }
    fn padding(&mut self) {
        for _ in 0..self.indent {
            self.out.write("    ");
        }
    }
    fn suite(&mut self) {
        self.out.write("):\n");
        self.indent += 1;
    }
    fn empty(&mut self) {
        self.padding();
        self.out.write("pass\n");
    }
    fn node(&mut self, node: k::Node<'_>, root: Option<&k::Unit>) {
        self.out.begin_node(root);
        self.padding();
        match node {
            k::Node::Block(body) => {
                self.out.write("with k.block(");
                self.suite();
                if body.is_empty() {
                    self.empty();
                }
                for stmt in body {
                    self.node(k::Node::Stmt(stmt), None);
                }
                self.indent -= 1;
            }
            k::Node::Stmt(stmt) => self.statement(stmt),
            k::Node::Expr(expr) => self.expression(expr),
            k::Node::Action(action) => {
                self.action(action);
                self.out.write("\n");
            }
        }
        self.out.end_node();
    }
    fn expr(&mut self, expr: &k::Expr) {
        self.node(k::Node::Expr(expr), None);
    }
    fn block(&mut self, block: &k::Block) {
        self.node(k::Node::Block(block), None);
    }
    fn rhs(&mut self, rhs: &k::Rhs) {
        match rhs {
            k::Rhs::Expr(e) => self.expr(e),
            k::Rhs::Action(a) => self.node(k::Node::Action(a), None),
        }
    }
    // Members have no Site of their own. Their expression children stay in kernel order.
    fn member_header(&mut self, prefix: &str, member: &k::Member) {
        self.out.write("with k.");
        self.out.write(prefix);
        match member {
            k::Member::Field { field, .. } => {
                self.out.write("field(");
                self.out.json(field);
            }
            k::Member::Index { .. } => self.out.write("index("),
        }
        self.suite();
        match member {
            k::Member::Field { target, .. } => self.expr(target),
            k::Member::Index { target, index } => {
                self.expr(target);
                self.expr(index);
            }
        }
    }
    fn statement(&mut self, stmt: &k::Stmt) {
        if self.readable_statement(stmt) {
            return;
        }
        match stmt {
            k::Stmt::Break | k::Stmt::Continue => {
                self.out.refuse(PrintError::Invalid(
                    "unhandled readable leaf statement".into(),
                ));
                return;
            }
            k::Stmt::Let { name, value } => {
                self.out.write("with k.let(");
                self.name(name);
                self.bound.insert(name.clone());
                self.suite();
                self.rhs(value);
            }
            k::Stmt::Assign { place, value } => {
                match place {
                    k::Place::Variable(n) => {
                        self.out.write("with k.assign(");
                        self.name(n);
                        self.bound.insert(n.clone());
                        self.suite();
                    }
                    k::Place::Member(m) => self.member_header("assign_", m),
                }
                self.rhs(value);
            }
            k::Stmt::Remove { member } => self.member_header("remove_", member),
            k::Stmt::Do { action } => {
                self.out.write("with k.do(");
                self.suite();
                self.node(k::Node::Action(action), None);
            }
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                self.out.write("with k.if_(");
                self.suite();
                self.expr(condition);
                self.block(then_block);
                self.block(else_block);
            }
            k::Stmt::For {
                binding,
                iterable,
                body,
            } => {
                self.out.write("with k.for_(");
                self.name(binding);
                self.bound.insert(binding.clone());
                self.suite();
                self.expr(iterable);
                self.block(body);
            }
            k::Stmt::While { condition, body } => {
                self.out.write("with k.while_(");
                self.suite();
                self.expr(condition);
                self.block(body);
            }
            k::Stmt::Return { value }
            | k::Stmt::Throw { value }
            | k::Stmt::Print { value }
            | k::Stmt::Finish { value }
            | k::Stmt::Fail { value } => {
                self.out.write(match stmt {
                    k::Stmt::Return { .. } => "with k.return_(",
                    k::Stmt::Throw { .. } => "with k.throw(",
                    k::Stmt::Print { .. } => "with k.print(",
                    k::Stmt::Finish { .. } => "with k.finish(",
                    _ => "with k.fail(",
                });
                self.suite();
                self.expr(value);
            }
            k::Stmt::Try(scope) => {
                self.out.write("with k.try_(");
                if let Some(catch) = &scope.catch {
                    self.name(&catch.binding);
                    self.bound.insert(catch.binding.clone());
                } else {
                    self.out.write("None");
                }
                self.out.write(if scope.finally.is_some() {
                    ", True"
                } else {
                    ", False"
                });
                self.suite();
                self.block(&scope.body);
                if let Some(catch) = &scope.catch {
                    self.block(&catch.body);
                }
                if let Some(finally) = &scope.finally {
                    self.block(finally);
                }
            }
        }
        self.indent -= 1;
    }
    fn expression(&mut self, expr: &k::Expr) {
        if self.inline.contains(expr) {
            self.inline_body(expr);
            self.out.write("\n");
            return;
        }
        match expr {
            k::Expr::Literal(_) | k::Expr::Variable(_) | k::Expr::Clock | k::Expr::Random => {
                self.out.refuse(PrintError::Invalid(
                    "unhandled inline leaf expression".into(),
                ));
                return;
            }
            k::Expr::Member(m) => self.member_header("", m),
            k::Expr::Tuple(items) | k::Expr::List(items) | k::Expr::Set(items) => {
                self.out.write(match expr {
                    k::Expr::Tuple(_) => "with k.tuple(",
                    k::Expr::List(_) => "with k.list(",
                    _ => "with k.set(",
                });
                self.suite();
                if items.is_empty() {
                    self.empty();
                }
                for item in items {
                    self.expr(item);
                }
            }
            k::Expr::Map(items) => {
                self.out.write("with k.map(");
                self.suite();
                if items.is_empty() {
                    self.empty();
                }
                for item in items {
                    self.expr(&item.key);
                    self.expr(&item.value);
                }
            }
            k::Expr::Record(items) => {
                self.out.write("with k.record(");
                self.out
                    .json(&items.iter().map(|i| &i.field).collect::<Vec<_>>());
                self.suite();
                if items.is_empty() {
                    self.empty();
                }
                for item in items {
                    self.expr(&item.value);
                }
            }
            k::Expr::Closure(c) => {
                self.out.write("with k.closure(");
                self.names(&c.params);
                self.suite();
                let parent = std::mem::replace(&mut self.bound, c.params.iter().cloned().collect());
                let outer_main = self.main;
                self.main = false;
                self.block(&c.body);
                self.main = outer_main;
                self.bound = parent;
            }
            k::Expr::Call { function, args } => {
                self.out.write("with k.invoke(");
                self.helper(function);
                self.suite();
                if args.is_empty() {
                    self.empty();
                }
                for arg in args {
                    self.expr(arg);
                }
            }
            k::Expr::Read(read) => {
                self.out.write("with k.read(");
                self.suite();
                self.expr(&read.handle);
                self.expr(&read.request);
            }
        }
        self.indent -= 1;
    }
    fn literal(&mut self, literal: &k::Literal) {
        match literal {
            k::Literal::Null => self.out.write("None"),
            k::Literal::Absent => self.out.write("k.absent"),
            k::Literal::Bool(b) => self.out.write(if *b { "True" } else { "False" }),
            k::Literal::Text(s) => self.out.json(s),
            k::Literal::Int(n) => {
                self.out.write(&n.to_string());
            }
            k::Literal::Float(n) if n.get().is_finite() => self.out.write(&n.to_string()),
            k::Literal::Float(n) => {
                self.out.write("float(");
                self.out.json(&n.to_string());
                self.out.write(")");
            }
            k::Literal::Bytes(b) => {
                self.out.write("bytes.fromhex(");
                self.out.json(&b.to_hex());
                self.out.write(")");
            }
            k::Literal::Function(n) => {
                self.out.write("declared.");
                self.name(n);
            }
        }
    }
    fn atom(&mut self, atom: &k::Atom) {
        match atom {
            k::Atom::Literal(l) => self.literal(l),
            k::Atom::Variable(n) => self.name(n),
        }
    }
    fn atoms(&mut self, atoms: &[k::Atom]) {
        self.out.write("[");
        for (i, atom) in atoms.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.atom(atom);
        }
        self.out.write("]");
    }
    fn callee(&mut self, callee: &k::Callee) {
        match callee {
            k::Callee::Declared(n) | k::Callee::Value(n) => {
                self.out.write(if matches!(callee, k::Callee::Declared(_)) {
                    "k.declared("
                } else {
                    "k.value("
                });
                self.out.json(n);
            }
            k::Callee::Library(id) => {
                self.out.write("k.library(");
                self.helper(id);
            }
        }
        self.out.write(")");
    }
    fn action(&mut self, action: &k::Action) {
        match action {
            k::Action::Call { callee, args } => {
                self.out.write("await ");
                match callee {
                    k::Callee::Library(id) => self.helper(id),
                    k::Callee::Declared(n) => {
                        self.out.write("declared.");
                        self.name(n);
                    }
                    k::Callee::Value(n) => self.name(n),
                }
                self.out.write("(");
                self.call_atoms(args);
            }
            k::Action::Perform {
                effect,
                args,
                result,
            } => {
                self.out.write("await tools");
                if effect.as_str().split('.').all(attribute_identifier) {
                    self.out.write(".");
                    self.out.write(effect.as_str());
                } else {
                    self.out.write("[");
                    self.out.json(effect);
                    self.out.write("]");
                }
                self.out.write("(");
                self.call_atoms(args);
                if !self
                    .effects
                    .get(effect)
                    .is_some_and(|signature| &signature.result == result)
                {
                    if !args.is_empty() {
                        self.out.write(", ");
                    }
                    self.out.write("result=");
                    self.metadata(result);
                }
            }
            k::Action::Spawn {
                callee: k::Callee::Library(id),
                args,
            } => {
                self.out.write("k.spawn(");
                self.helper(id);
                self.out.write("(");
                self.call_atoms(args);
                self.out.write(")");
            }
            k::Action::Spawn { callee, args } => {
                self.out.write("k.spawn(");
                self.callee(callee);
                self.out.write(", ");
                self.atoms(args);
            }
            k::Action::Sleep { duration } => {
                self.out.write("k.sleep(");
                self.atom(duration);
            }
            k::Action::Join { task } => {
                self.out.write("k.join(");
                self.atom(task);
            }
            k::Action::Cancel { task } => {
                self.out.write("k.cancel(");
                self.atom(task);
            }
            k::Action::JoinMany { mode, tasks } => {
                self.out.write("k.join_many(");
                self.out.json(match mode {
                    k::JoinMode::All => "all",
                    k::JoinMode::AllSettled => "all_settled",
                    k::JoinMode::Race => "race",
                    k::JoinMode::Any => "any",
                });
                self.out.write(", ");
                self.atom(tasks);
            }
            k::Action::Yield => self.out.write("k.yield_("),
        }
        self.out.write(")");
    }
    fn call_atoms(&mut self, atoms: &[k::Atom]) {
        for (i, atom) in atoms.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.atom(atom);
        }
    }
}

/// Memoize the applicability predicate in one postorder pass. This is naming/
/// rendering work even with maps off; it creates no Sites or source tree.
struct InlinePlan(std::collections::HashSet<*const k::Expr>);
impl InlinePlan {
    fn new(document: &k::Document) -> Self {
        let mut plan = Self(Default::default());
        plan.collect(k::Node::Block(&document.main));
        for f in document.functions.values() {
            plan.collect(k::Node::Block(&f.body));
        }
        plan
    }
    fn contains(&self, expr: &k::Expr) -> bool {
        self.0.contains(&(expr as *const _))
    }
    fn collect(&mut self, node: k::Node<'_>) {
        for child in node.children() {
            self.collect(child);
        }
        if let k::Node::Expr(expr) = node {
            let eligible = match expr {
                k::Expr::Literal(_) | k::Expr::Variable(_) | k::Expr::Clock | k::Expr::Random => {
                    true
                }
                k::Expr::Member(member) => match member.as_ref() {
                    k::Member::Field { target, .. } => self.contains(target),
                    k::Member::Index { target, index } => {
                        self.contains(target) && self.contains(index)
                    }
                },
                k::Expr::Call { args, .. } => args.iter().all(|arg| self.contains(arg)),
                k::Expr::List(items) | k::Expr::Tuple(items) | k::Expr::Set(items) => {
                    items.iter().all(|e| self.contains(e))
                }
                k::Expr::Map(items) => items
                    .iter()
                    .all(|e| self.contains(&e.key) && self.contains(&e.value)),
                k::Expr::Record(items) => items.iter().all(|e| self.contains(&e.value)),
                _ => false,
            };
            if eligible {
                self.0.insert(expr as *const _);
            }
        }
    }
}
