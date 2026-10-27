//! Ordinary Python source, verified by the readable export path.
use crate::generated_scope::BindingScope;
mod authored;
mod direct;
mod names;
mod packed;
mod readable;
mod signature;
mod task;
use lash_kernel_dialect::{
    NamePlan, PrintError, PrintOptions, PrintedSource, SourceFormat, SourceWriter,
    check_document_depth,
};
use lash_kernel_doc as k;
use lash_kernel_value::Name;
pub(crate) use names::{attribute_identifier, identifier, legal_identifier};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// Authored exports and the ordered parts of their Main body.
pub(crate) struct Regions {
    pub(crate) authored: BTreeSet<Name>,
    pub(crate) whole: Vec<lash_kernel_dialect::regions::ModulePart>,
}
pub(crate) fn print_regions(
    document: &k::Document,
    library: &dyn lash_kernel_dialect::Library,
    captions: Option<&k::Captions>,
    options: PrintOptions,
    regions: &Regions,
    synchronous: &BTreeSet<Name>,
) -> Result<(PrintedSource, BTreeMap<Name, Range<usize>>), PrintError> {
    check_document_depth(document, options.limits)?;
    let plan = NamePlan::new(document, legal_identifier);
    let operators = crate::generated_operators::Operators::new(document, library);
    let inline = InlinePlan::new(document);
    let mut r = Renderer {
        out: SourceWriter::with_captions(options, document, captions)?,
        plan: &plan,
        inline: &inline,
        document,
        indent: 0,
        scope: Default::default(),
        main: false,
        operators,
        captions: captions.is_some_and(|c| !c.is_empty()),
        authored: true,
        sync: synchronous.clone(),
        asynchronous: BTreeSet::new(),
        substitutions: BTreeMap::new(),
        direct: BTreeMap::new(),
        comprehensions: BTreeMap::new(),
        expression_depth: 0,
        max_depth: options.limits.max_depth.min(k::MAX_NESTING_DEPTH),
    };
    let asyncio = ["py.task.run", "py.gather", "py.sleep"]
        .iter()
        .any(|helper| r.operators.uses(helper));
    let time = r.operators.uses("py.time");
    if asyncio {
        r.out.write("import asyncio\n");
    }
    if time {
        r.out.write("import time\n");
    }
    if asyncio || time {
        r.out.write("\n");
    }
    let signatures = r.typed_dicts(document, &regions.authored);
    let mut written = BTreeMap::new();
    let main = document.main.as_deref().unwrap_or_default();
    r.out
        .begin_node(Some(&lash_kernel_value::sites::Unit::Main));
    let body_start = r.out.len();
    for part in &regions.whole {
        use lash_kernel_dialect::regions::ModulePart;
        match part {
            ModulePart::Export(name) => {
                if r.out.len() > body_start {
                    r.out.write("\n");
                }
                r.out.begin_node(None); // the Main binding
                let range = r.export_function(name, &signatures)?;
                r.implicit(); // its function literal
                r.out.end_node();
                written.insert(name.clone(), range);
                r.scope.bind(name, Some(true));
            }
            ModulePart::Statements(range) => {
                r.main = true;
                r.authored_statement_range(main, range.clone());
            }
            ModulePart::Alias { .. } => {
                return Err(PrintError::Unprintable {
                    site: lash_kernel_value::sites::Site::new(
                        lash_kernel_value::sites::Unit::Main,
                        Vec::new(),
                    ),
                });
            }
            ModulePart::Implicit => r.implicit(),
        }
    }
    r.out.end_node();
    r.out
        .finish(document, SourceFormat::PythonV1)
        .map(|printed| (printed, written))
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
    /// Whether the region being written is authored source.
    authored: bool,
    /// The document owning stored function bodies and import slots.
    document: &'a k::Document,
    /// The local functions an authored region writes as plain `def`s,
    /// whose calls take no `await`.
    sync: BTreeSet<Name>,
    asynchronous: BTreeSet<Name>,
    substitutions: BTreeMap<Name, k::Expr>,
    direct: BTreeMap<Name, (Name, lash_kernel_value::FunctionId)>,
    comprehensions: BTreeMap<Name, authored::Comprehension>,
    expression_depth: usize,
    max_depth: usize,
}
impl Renderer<'_> {
    fn export_function(
        &mut self,
        name: &Name,
        signatures: &signature::Signatures,
    ) -> Result<Range<usize>, PrintError> {
        let Some(k::Export::Function(export)) = self.document.exports.get(name) else {
            return Err(PrintError::Invalid(
                "a non-function export has no Python declaration".into(),
            ));
        };
        let definition = self
            .document
            .functions
            .get(&export.function)
            .ok_or_else(|| PrintError::Invalid("missing exported function".into()))?;
        let body = definition
            .body()
            .ok_or_else(|| PrintError::Invalid("native export has no Python body".into()))?;
        let params = definition
            .signature
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>();
        let start = self.out.len();
        let unit = lash_kernel_value::sites::Unit::Library(export.function);
        self.caption_of(lash_kernel_value::sites::Site::new(
            unit.clone(),
            Vec::new(),
        ));
        if !export.asynchronous {
            self.sync.insert(name.clone());
        } else {
            self.asynchronous.insert(name.clone());
        }
        self.out.write(if export.asynchronous {
            "async def "
        } else {
            "def "
        });
        self.name(name);
        self.signature(signatures, name, &params);
        self.out.write(":\n");
        let mut local = BindingScope::new(&params);
        // The exported annotations supply the same operand facts as lowering.
        for param in &export.signature.params {
            use lash_kernel_dialect::source_types::Kind;
            let kind = match &param.ty {
                lash_kernel_value::Type::Text => Kind::Text,
                lash_kernel_value::Type::Int
                | lash_kernel_value::Type::Float
                | lash_kernel_value::Type::Number => Kind::Number,
                lash_kernel_value::Type::Bool => Kind::Bool,
                _ => Kind::Unknown,
            };
            local.types.0.insert(param.name.clone(), kind);
        }
        let scope = std::mem::replace(&mut self.scope, local);
        let main = std::mem::replace(&mut self.main, false);
        self.root_block(&body.block, Some(&unit));
        self.main = main;
        self.scope = scope;
        Ok(start..self.out.len())
    }

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
    fn caption_of(&mut self, site: lash_kernel_value::sites::Site) {
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
    fn parameters(&mut self, names: &[Name]) {
        self.out.write("(");
        for (i, name) in names.iter().enumerate() {
            if i > 0 {
                self.out.write(", ");
            }
            self.name(name);
        }
        self.out.write(")");
    }
    fn root_block(&mut self, block: &k::Block, root: Option<&lash_kernel_value::sites::Unit>) {
        let sync = self.sync.clone();
        let asynchronous = self.asynchronous.clone();
        self.out.begin_node(root);
        self.indent += 1;
        if block.is_empty() {
            self.empty();
        }
        if let Some(lash_kernel_value::sites::Unit::Library(id)) = root {
            let params = self.document.functions[id]
                .signature
                .params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>();
            self.nonlocals(block, &params);
        }
        self.authored_statements(block);
        self.indent -= 1;
        self.out.end_node();
        self.sync = sync;
        self.asynchronous = asynchronous;
    }
    fn nonlocals(&mut self, block: &k::Block, params: &[Name]) {
        let mut locals: BTreeSet<_> = params.iter().cloned().collect();
        let mut writes = BTreeSet::new();
        let mut pending = vec![k::Node::Block(block)];
        while let Some(node) = pending.pop() {
            match node {
                k::Node::Stmt(k::Stmt::Let { name, .. } | k::Stmt::For { binding: name, .. })
                | k::Node::Expr(k::Expr::Let { name, .. }) => {
                    locals.insert(name.clone());
                }
                k::Node::Stmt(k::Stmt::Assign {
                    place: k::Place::Variable(name),
                    ..
                }) => {
                    writes.insert(name.clone());
                }
                _ => {}
            }
            pending.extend(node.children());
        }
        for name in writes.difference(&locals) {
            self.padding();
            self.out.write("nonlocal ");
            self.name(name);
            self.out.write("\n");
        }
    }
    fn name(&mut self, n: &Name) {
        // Authored source spells the document's names; only the kernel
        // surface reserves some for its own syntax.
        if self.authored && attribute_identifier(n.as_str()) {
            self.out.write(n.as_str());
        } else {
            self.out.write(&self.plan.names[n]);
        }
    }
    fn helper(&mut self, id: &impl crate::generated_operators::Reference) {
        let Some(name) = self.operators.name(id) else {
            self.out
                .refuse(PrintError::Invalid("missing callable source name".into()));
            return;
        };
        self.out.write(&name);
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
    fn node(&mut self, node: k::Node<'_>, root: Option<&lash_kernel_value::sites::Unit>) {
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
    // Members have no Site of their own. Their expression children stay in kernel order.
    fn statement(&mut self, stmt: &k::Stmt) {
        self.render_statement(stmt);
        self.scope.types.statement(stmt, self.operators.functions());
    }
    fn render_statement(&mut self, stmt: &k::Stmt) {
        if !self.readable_statement(stmt) {
            self.out.refuse(PrintError::Invalid(
                "statement has no authored Python spelling".into(),
            ));
        }
    }
    fn expression(&mut self, expr: &k::Expr) {
        self.inline_body(expr);
    }
    fn literal(&mut self, literal: &k::Literal) {
        match literal {
            k::Literal::Null => self.out.write("None"),
            k::Literal::Absent => self.out.write("value.absent()"),
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
                self.out.write("bytes.from_hex(");
                self.out.json(&b.to_hex());
                self.out.write(")");
            }
            k::Literal::Function(id) => self.helper(id),
        }
    }
    fn atom(&mut self, atom: &k::Atom) {
        match atom {
            k::Atom::Literal(l) => self.literal(l),
            k::Atom::Variable(n) => self.name(n),
        }
    }
    fn callee(&mut self, callee: &k::Callee) {
        match callee {
            k::Callee::Local(id) => self.helper(id),
            k::Callee::Import(slot) => self.helper(slot),
            k::Callee::Value(name) => self.name(name),
        }
    }
    pub(super) fn authored_call(
        &mut self,
        id: &impl crate::generated_operators::Reference,
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
            Some(Authored::Index) => {
                self.authored_arg(&args[0], mapped);
                self.out.write("[");
                self.authored_arg(&args[1], mapped);
                self.out.write("]");
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
        let k::Expr::Act(act) = k::Expr::from(action.clone()) else {
            unreachable!("action expression");
        };
        self.nested_action(&act, false);
    }
}

/// Memoize the applicability predicate in one postorder pass. This is naming/
/// rendering work even with maps off; it creates no Sites or source tree.
struct InlinePlan {
    expressions: std::collections::HashSet<*const k::Expr>,
}
impl InlinePlan {
    fn new(document: &k::Document) -> Self {
        let mut plan = Self {
            expressions: Default::default(),
        };
        if let Some(main) = &document.main {
            plan.collect(k::Node::Block(main));
        }
        for f in document.functions.values() {
            if let Some(body) = f.body() {
                plan.collect(k::Node::Block(&body.block));
            }
        }
        plan
    }
    fn contains(&self, expr: &k::Expr) -> bool {
        self.expressions.contains(&(expr as *const _)) || self.eligible(expr)
    }
    // Reconstructed authored bodies own cloned expressions. Their shape
    // obeys the same eligibility rule as nodes in the original document.
    fn eligible(&self, expr: &k::Expr) -> bool {
        match expr {
            k::Expr::Literal(_) | k::Expr::Variable(_) => true,
            k::Expr::Member(member) => match member.as_ref() {
                k::Member::Field { target, .. } => self.contains(target),
                k::Member::Index { target, index } => self.contains(target) && self.contains(index),
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
            k::Expr::If { cond, then, r#else } => {
                self.contains(cond) && self.contains(then) && self.contains(r#else)
            }
            k::Expr::Let { value, body, .. } => self.contains(value) && self.contains(body),
            k::Expr::Closure(_) => true,
        }
    }
    fn collect(&mut self, node: k::Node<'_>) {
        for child in node.children() {
            self.collect(child);
        }
        if let k::Node::Expr(expr) = node {
            let eligible = self.eligible(expr);
            if eligible {
                self.expressions.insert(expr as *const _);
            }
        }
    }
}
