//! Complete Python generic forms; paired inverses live in generated.rs.
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
    let mut r = Renderer {
        out: SourceWriter::new(options),
        plan: &plan,
        indent: 0,
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
        r.out.write("with k.declare(");
        r.out.json(name);
        r.out.write(", ");
        r.names(&function.params);
        r.suite();
        let unit = options.source_map.then(|| k::Unit::Function(name.clone()));
        r.node(k::Node::Block(&function.body), unit.as_ref());
        r.indent -= 1;
    }
    r.out.write("with k.main():\n");
    r.indent = 1;
    r.node(
        k::Node::Block(&document.main),
        options.source_map.then_some(&k::Unit::Main),
    );
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
pub(crate) fn legal_identifier(name: &str) -> bool {
    identifier(name)
        && !name.starts_with("helper_")
        && !name.starts_with("kernel_name_")
        && !matches!(
            name,
            "k" | "False"
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
    indent: usize,
}
impl Renderer<'_> {
    fn metadata(&mut self, value: &impl serde::Serialize) {
        self.out.metadata(value);
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
        match stmt {
            k::Stmt::Break => {
                self.out.write("k.break_()\n");
                return;
            }
            k::Stmt::Continue => {
                self.out.write("k.continue_()\n");
                return;
            }
            k::Stmt::Let { name, value } => {
                self.out.write("with k.let(");
                self.name(name);
                self.suite();
                self.rhs(value);
            }
            k::Stmt::Assign { place, value } => {
                match place {
                    k::Place::Variable(n) => {
                        self.out.write("with k.assign(");
                        self.name(n);
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
        match expr {
            k::Expr::Literal(l) => {
                self.literal(l);
                self.out.write("\n");
                return;
            }
            k::Expr::Variable(n) => {
                self.name(n);
                self.out.write("\n");
                return;
            }
            k::Expr::Clock => {
                self.out.write("k.clock()\n");
                return;
            }
            k::Expr::Random => {
                self.out.write("k.random()\n");
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
                self.block(&c.body);
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
                self.out.write("k.int(");
                self.out.json(&n.to_string());
                self.out.write(")");
            }
            k::Literal::Float(n) => {
                self.out.write("k.float(");
                self.out.json(&n.to_string());
                self.out.write(")");
            }
            k::Literal::Bytes(b) => {
                self.out.write("k.bytes(");
                self.out.json(&b.to_hex());
                self.out.write(")");
            }
            k::Literal::Function(n) => {
                self.out.write("k.function(");
                self.out.json(n);
                self.out.write(")");
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
            k::Action::Call { callee, args } | k::Action::Spawn { callee, args } => {
                self.out.write(if matches!(action, k::Action::Call { .. }) {
                    "k.call("
                } else {
                    "k.spawn("
                });
                self.callee(callee);
                self.out.write(", ");
                self.atoms(args);
            }
            k::Action::Perform {
                effect,
                args,
                result,
            } => {
                self.out.write("k.perform(");
                self.out.json(effect);
                self.out.write(", ");
                self.atoms(args);
                self.out.write(", k.type(");
                self.metadata(result);
                self.out.write(")");
            }
            k::Action::Sleep { duration } => {
                self.out.write("k.sleep(");
                self.atom(duration);
            }
            k::Action::Join { task } | k::Action::Cancel { task } => {
                self.out.write(if matches!(action, k::Action::Join { .. }) {
                    "k.join("
                } else {
                    "k.cancel("
                });
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
}
