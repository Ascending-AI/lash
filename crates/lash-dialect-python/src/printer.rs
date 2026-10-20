//! Exported Python: plain source whose environment stays in the base
//! document; paired inverses live in generated.rs.
use crate::{Python, generated_scope::BindingScope};
mod names;
mod readable;
mod signature;
use lash_kernel_dialect::{
    NamePlan, PrintError, PrintOptions, PrintedSource, Printer, SourceFormat, SourceWriter,
    check_document_depth,
};
use lash_kernel_doc as k;
pub(crate) use names::{attribute_identifier, identifier, legal_identifier};

/// Render plain Python without compiling or resolving dependencies. The
/// manifest, entries, private bindings, helper identities and name renames
/// are not written: [`crate::read`] takes them from the base document.
/// Each caption is spelled as `#:` lines directly above what it captions,
/// an `else`, `except` or `finally` clause for an arm, the `def` for a
/// function, a parameter written on its own line, and a field of a
/// `TypedDict`. A function whose parameters or whose types' fields carry
/// captions takes one parameter a line, and each parameter or result with a
/// captioned field is annotated with its type, written from the entry's
/// signature, each record a `TypedDict` above every function.
/// [`PrintedSource::dropped`] lists an entry's parameter and field captions
/// when its function takes other parameters than its signature names, and a
/// field's below a record with a field no class body can name.
pub fn print(
    document: &k::Document,
    captions: Option<&k::Captions>,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    check_document_depth(document, options.limits)?;
    if k::KernelVersion::of(document.manifest.kernel).is_none() {
        return Err(PrintError::Invalid("unsupported kernel version".into()));
    }
    let plan = NamePlan::new(document, legal_identifier);
    let inline = InlinePlan::new(document);
    let mut r = Renderer {
        out: SourceWriter::with_captions(options, document, captions)?,
        plan: &plan,
        inline: &inline,
        indent: 0,
        scope: Default::default(),
        main: false,
        operators: crate::generated_operators::Operators::new(&document.manifest.functions),
        captions: captions.is_some_and(|captions| !captions.is_empty()),
    };
    let signatures = r.typed_dicts(document);
    for (name, function) in &document.functions {
        if r.captions {
            r.caption_of(k::Site::new(k::Unit::Function(name.clone()), Vec::new()));
        }
        r.out.write(if awaits(&function.body) {
            "async def "
        } else {
            "def "
        });
        r.name(name);
        r.signature(&signatures, name, function);
        r.out.write(":\n");
        let unit = r
            .out
            .tracks_sites()
            .then(|| k::Unit::Function(name.clone()));
        r.scope = BindingScope::new(&function.params);
        r.root_block(&function.body, unit.as_ref());
    }
    r.scope = Default::default();
    r.main = true;
    r.out.write(if awaits(&document.main) {
        "async def main():\n"
    } else {
        "def main():\n"
    });
    let tracks = r.out.tracks_sites();
    r.root_block(&document.main, tracks.then_some(&k::Unit::Main));
    r.out.finish(document, SourceFormat::PythonV1)
}
impl Printer for Python {
    fn print(
        &self,
        document: &k::Document,
        captions: Option<&k::Captions>,
        options: PrintOptions,
    ) -> Result<PrintedSource, PrintError> {
        print(document, captions, options)
    }
}

struct Renderer<'a> {
    out: SourceWriter,
    plan: &'a NamePlan,
    inline: &'a InlinePlan,
    indent: usize,
    scope: BindingScope,
    main: bool,
    operators: crate::generated_operators::Operators,
    /// Whether any caption waits to be spelled.
    captions: bool,
}
impl Renderer<'_> {
    /// The caption of the node the writer opens next, taken to be spelled.
    fn next_caption(&mut self) -> Option<k::Caption> {
        let site = self.captions.then(|| self.out.next_site(None)).flatten()?;
        self.out.take_caption(site)
    }
    /// Spells the caption of the node the writer opens next, if it has one.
    fn caption_next(&mut self) {
        if let Some(caption) = self.next_caption() {
            self.spell(&caption);
        }
    }
    fn caption_of(&mut self, site: k::Site) {
        if let Some(caption) = self.out.take_caption(site) {
            self.spell(&caption);
        }
    }
    /// `caption`'s `#:` lines, at the current indentation.
    fn spell(&mut self, caption: &k::Caption) {
        for line in crate::captions::lines(caption) {
            self.padding();
            self.out.write("#:");
            if !line.is_empty() {
                self.out.write(" ");
                self.out.write(line);
            }
            self.out.write("\n");
        }
    }
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
        if matches!(node, k::Node::Stmt(_) | k::Node::Block(_)) {
            self.caption_next();
        }
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
        self.render_statement(stmt);
        self.scope.types.statement(stmt, self.operators.functions());
    }
    fn render_statement(&mut self, stmt: &k::Stmt) {
        if matches!(stmt, k::Stmt::For { metered, .. } | k::Stmt::While { metered, .. } if *metered != 0)
        {
            self.out.refuse(PrintError::Invalid(
                "a metered library loop requires kernel text".into(),
            ));
            return;
        }
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
                self.scope.bind(name, Some(true));
                self.suite();
                self.rhs(value);
            }
            // The front end writes Python's own order, the right-hand side
            // first, so its source has no spelling for another.
            k::Stmt::Assign {
                order: k::AssignOrder::PlaceFirst,
                ..
            } => {
                self.out.refuse(PrintError::Invalid(
                    "a place-first assignment has no generated Python spelling".into(),
                ));
                return;
            }
            k::Stmt::Assign { place, value, .. } => {
                match place {
                    k::Place::Variable(n) => {
                        self.out.write("with k.assign(");
                        self.name(n);
                        self.scope.bind(n, Some(false));
                        self.suite();
                    }
                    k::Place::Member(m) => self.member_header("assign_", m),
                }
                self.rhs(value);
            }
            k::Stmt::Remove { member } => self.member_header("remove_", member),
            k::Stmt::Do { value } => {
                self.out.write("with k.do(");
                self.suite();
                self.expr(value);
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
                via,
                body,
                else_block,
                ..
            } => {
                self.out.write("with k.for_(");
                self.name(binding);
                self.scope.introduce(binding);
                // A protocol's hooks follow the binding, `None` for one it
                // omits.
                if let Some(via) = via {
                    for hook in [Some(via.open), via.next, via.item, via.check] {
                        self.out.write(", ");
                        match hook {
                            Some(id) => self.helper(&id),
                            None => self.out.write("None"),
                        }
                    }
                }
                self.suite();
                self.expr(iterable);
                self.block(body);
                if !else_block.is_empty() {
                    self.block(else_block);
                }
            }
            k::Stmt::While {
                condition,
                body,
                else_block,
                ..
            } => {
                self.out.write("with k.while_(");
                self.suite();
                self.expr(condition);
                self.block(body);
                if !else_block.is_empty() {
                    self.block(else_block);
                }
            }
            k::Stmt::ReturnVia { value, policy } => {
                self.out.write("with k.return_via(");
                self.helper(policy);
                self.suite();
                self.expr(value);
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
                    self.scope.introduce(&catch.binding);
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
                let parent = std::mem::replace(&mut self.scope, BindingScope::new(&c.params));
                let outer_main = self.main;
                self.main = false;
                self.block(&c.body);
                self.main = outer_main;
                self.scope = parent;
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
            k::Expr::If { cond, then, r#else } => {
                self.out.write("with k.choose(");
                self.suite();
                self.expr(cond);
                self.expr(then);
                self.expr(r#else);
            }
            k::Expr::Let { name, value, body } => {
                self.out.write("with k.bind(");
                self.out.json(name.as_str());
                self.suite();
                self.expr(value);
                self.expr(body);
            }
            k::Expr::Read(read) => {
                self.out.write("with k.read(");
                self.suite();
                self.expr(&read.handle);
                self.expr(&read.request);
            }
            k::Expr::Act(act) => self.nested_action_suite(act),
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
            k::Callee::Declared(name) => {
                self.out.write("declared.");
                self.name(name);
            }
            k::Callee::Value(name) => self.name(name),
            k::Callee::Library(id) => self.helper(id),
        }
    }
    pub(super) fn authored_call(
        &mut self,
        id: &k::FunctionId,
        args: &[k::Expr],
        mapped: bool,
    ) -> bool {
        use crate::generated_operators::Authored;
        match self.operators.authored(id, args) {
            Some(Authored::Builtin(name)) => {
                self.out.write(name);
                self.out.write("(");
                self.authored_args(args, mapped);
                self.out.write(")");
            }
            Some(Authored::Slice) => {
                self.authored_arg(&args[0], mapped);
                self.out.write("[");
                for (i, bound) in args[1..].iter().enumerate() {
                    if i == 2 && matches!(bound, k::Expr::Literal(k::Literal::Absent)) {
                        break;
                    }
                    if i > 0 {
                        self.out.write(":");
                    }
                    if !matches!(bound, k::Expr::Literal(k::Literal::Absent)) {
                        self.authored_arg(bound, mapped);
                    }
                }
                self.out.write("]");
            }
            Some(Authored::TypeName) => {
                self.out.write("type(");
                self.authored_arg(&args[0], mapped);
                self.out.write(").__name__");
            }
            Some(Authored::Method(method)) => {
                let numeric = matches!(
                    args[0],
                    k::Expr::Literal(k::Literal::Int(_) | k::Literal::Float(_))
                );
                if numeric {
                    self.out.write("(");
                }
                self.authored_arg(&args[0], mapped);
                if numeric {
                    self.out.write(")");
                }
                self.out.write(".");
                self.out.write(&method);
                self.out.write("(");
                self.authored_args(&args[1..], mapped);
                self.out.write(")");
            }
            None => return false,
        }
        true
    }
    fn authored_args(&mut self, args: &[k::Expr], mapped: bool) {
        for (index, expr) in args.iter().enumerate() {
            if index > 0 {
                self.out.write(", ");
            }
            self.authored_arg(expr, mapped);
        }
    }
    fn authored_arg(&mut self, expr: &k::Expr, mapped: bool) {
        if mapped {
            self.inline_expr(expr);
        } else {
            match expr {
                k::Expr::Variable(name) => self.atom(&k::Atom::Variable(name.clone())),
                k::Expr::Literal(value) => self.atom(&k::Atom::Literal(value.clone())),
                _ => unreachable!("a statement action has atom operands"),
            }
        }
    }
    fn action(&mut self, action: &k::Action) {
        match action {
            k::Action::Call { callee, args } => {
                if let k::Callee::Library(id) = callee
                    && self.authored_call(
                        id,
                        &args
                            .iter()
                            .map(|arg| match arg {
                                k::Atom::Variable(name) => k::Expr::Variable(name.clone()),
                                k::Atom::Literal(value) => k::Expr::Literal(value.clone()),
                            })
                            .collect::<Vec<_>>(),
                        false,
                    )
                {
                    return;
                }
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
                then_yield,
            } => {
                self.out.write("await ");
                self.operation_path(effect.as_str());
                self.out.write("(");
                self.call_atoms(args);
                // Authored lowering performs as `any`; only another result is spelled.
                if *result != k::Type::Any {
                    if !args.is_empty() {
                        self.out.write(", ");
                    }
                    self.out.write("result=");
                    self.metadata(result);
                }
                // Python's `await` takes no turn of its own; a wait that
                // takes one more once its outcome is in says so.
                if *then_yield {
                    if !args.is_empty() || *result != k::Type::Any {
                        self.out.write(", ");
                    }
                    self.out.write("then_yield=True");
                }
            }
            // Python's own `await`: by the `py.await` the base holds.
            k::Action::Await {
                policy,
                value: value @ k::Atom::Variable(_),
            } if self.operators.unique("py.await") == Some(*policy) => {
                self.out.write("await ");
                self.atom(value);
                return;
            }
            k::Action::Await { policy, value } => {
                self.out.write("await_by(");
                self.helper(policy);
                self.out.write(", ");
                self.atom(value);
            }
            k::Action::Spawn { callee, args } => {
                self.out.write("spawn(");
                self.callee(callee);
                self.out.write(", ");
                self.atoms(args);
            }
            k::Action::Sleep {
                duration,
                then_yield,
            } => {
                self.out.write("await sleep(");
                self.atom(duration);
                if *then_yield {
                    self.out.write(", then_yield=True");
                }
            }
            k::Action::Join { task } => {
                self.out.write("await join(");
                self.atom(task);
            }
            k::Action::Cancel { task } => {
                self.out.write("k.cancel(");
                self.atom(task);
            }
            k::Action::JoinMany { mode, tasks } => {
                self.out.write(match mode {
                    k::JoinMode::All => "await join_all(",
                    k::JoinMode::AllSettled => "await join_all_settled(",
                    k::JoinMode::Race => "await join_race(",
                    k::JoinMode::Any => "await join_any(",
                });
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
                k::Expr::Act(act) => act.args.iter().all(|arg| self.contains(arg)),
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

/// Only native calls emit await. A bound closure has its own function context.
fn awaits(body: &k::Block) -> bool {
    fn node(node: k::Node<'_>) -> bool {
        match node {
            k::Node::Expr(k::Expr::Act(act)) if !matches!(act.kind, k::ActKind::Spawn(_)) => true,
            k::Node::Action(
                k::Action::Call { .. }
                | k::Action::Perform { .. }
                | k::Action::Await { .. }
                | k::Action::Sleep { .. }
                | k::Action::Join { .. }
                | k::Action::JoinMany { .. },
            ) => true,
            k::Node::Stmt(
                k::Stmt::Let {
                    value: k::Rhs::Expr(k::Expr::Closure(_)),
                    ..
                }
                | k::Stmt::Assign {
                    place: k::Place::Variable(_),
                    value: k::Rhs::Expr(k::Expr::Closure(_)),
                    ..
                },
            ) => false,
            _ => node.children().into_iter().any(node_awaits),
        }
    }
    fn node_awaits(value: k::Node<'_>) -> bool {
        node(value)
    }
    body.iter().any(|stmt| node(k::Node::Stmt(stmt)))
}
