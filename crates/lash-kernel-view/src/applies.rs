//! The may-call set of an action (`ActionView::applies`), in site order.
//!
//! Union writes and follow values through lexical bindings, parameters at
//! document call sites, collection members and kernel library results. A
//! library summary records both the inputs it applies and the value it
//! returns, including wrappers that apply an input. Summaries use parameter
//! paths, so a callable inside a collection is applied only when the body
//! selects it. Native-only bodies and values supplied by hosts or effects
//! contribute no known callable. Session writes in this document are visible;
//! saved-function binding identities from earlier cells are not supplied to
//! this view. A same-named declared function is not proof of a session binding.
//! Promise.all and asyncio.gather only await tasks: the nested call/spawn
//! actions that start their members name the closures, not the aggregate.
//!
//! Each traversal tracks its active nodes/bindings, cutting recursive edges
//! without discarding other writes. Library identities form a DAG, and their
//! summaries are memoized by identity for the whole view derivation.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, BindingId, CalleeNode, ExprNode, Graph, MemberNode, NodeId,
    NodeKind, PlaceNode, Reference, StmtNode,
};
use lash_kernel_doc::{FunctionDeclarations, Literal};

use crate::ViewContext;
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use lash_vocabulary::{CallableId, LinkId};

use crate::Derived;

/// A member selection; `Any` is a dynamic index or a loop's element.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Index(String),
    Field(String),
    Any,
}

/// A symbolic input in a library summary, or a callable in the document.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Origin {
    Input(usize, Vec<Key>),
    Callable(Site),
}

/// Callable provenance at each member path. A symbolic input represents the
/// whole value at that path; selecting it extends its input path. Keeping
/// paths intact prevents a list containing a closure from being callable.
#[derive(Clone, Debug, Default)]
struct Value(BTreeMap<Vec<Key>, BTreeSet<Origin>>);

impl Value {
    fn origin(origin: Origin) -> Self {
        Self(BTreeMap::from([(Vec::new(), BTreeSet::from([origin]))]))
    }

    fn union(&mut self, other: Self) {
        for (path, origins) in other.0 {
            self.0.entry(path).or_default().extend(origins);
        }
    }

    fn member(&self, key: &Key) -> Self {
        let mut result = Self::default();
        for (path, origins) in &self.0 {
            if let Some((first, tail)) = path.split_first() {
                if first == key || *first == Key::Any || *key == Key::Any {
                    result
                        .0
                        .entry(tail.to_vec())
                        .or_default()
                        .extend(origins.iter().cloned());
                }
            } else {
                for origin in origins {
                    if let Origin::Input(index, path) = origin {
                        let mut path = path.clone();
                        path.push(key.clone());
                        result.union(Self::origin(Origin::Input(*index, path)));
                    }
                }
            }
        }
        result
    }

    fn inside(self, key: Key) -> Self {
        Self(
            self.0
                .into_iter()
                .map(|(mut path, origins)| {
                    path.insert(0, key.clone());
                    (path, origins)
                })
                .collect(),
        )
    }

    fn direct(&self) -> BTreeSet<Origin> {
        self.0.get(&Vec::new()).cloned().unwrap_or_default()
    }

    /// Instantiate a library result or its applied inputs at a call site.
    fn substitute(&self, args: &[Self]) -> Self {
        let mut result = Self::default();
        for (path, origins) in &self.0 {
            for origin in origins {
                let mut value = match origin {
                    Origin::Callable(_) => Self::origin(origin.clone()),
                    Origin::Input(index, selections) => {
                        let mut value = args.get(*index).cloned().unwrap_or_default();
                        for key in selections {
                            value = value.member(key);
                        }
                        value
                    }
                };
                for key in path.iter().rev() {
                    value = value.inside(key.clone());
                }
                result.union(value);
            }
        }
        result
    }
}

#[derive(Clone, Default)]
struct Summary {
    applied: Value,
    returned: Value,
}

/// The facts shared by every action of one document view.
pub(crate) struct Links {
    library: BTreeMap<CallableId, Summary>,
}

impl Links {
    pub(crate) fn new() -> Self {
        Self {
            library: BTreeMap::new(),
        }
    }

    pub(crate) fn applies(
        &mut self,
        graph: &Graph,
        functions: &FunctionDeclarations,
        action: Derived<'_>,
        context: &ViewContext,
    ) -> Vec<Site> {
        let mut analysis = Analysis::new(
            graph,
            functions,
            &mut self.library,
            context,
            context.reference.link,
            false,
        );
        let applied = analysis.action(action, &mut BTreeSet::new());
        analysis
            .expand(applied, &mut BTreeSet::new())
            .direct()
            .into_iter()
            .filter_map(|origin| match origin {
                Origin::Callable(site) => Some(site),
                Origin::Input(..) => None,
            })
            .collect()
    }
}

/// Identity of an active dependency, scoped to this graph traversal.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Seen {
    Node(NodeId),
    Binding(BindingId),
    Session(Name),
    Body(Site),
}

struct Analysis<'a> {
    context: &'a ViewContext,
    link: LinkId,
    summarizing: bool,
    graph: &'a Graph,
    functions: &'a FunctionDeclarations,
    library: &'a mut BTreeMap<CallableId, Summary>,
    inputs: BTreeMap<BindingId, usize>,
}

impl<'a> Analysis<'a> {
    fn new(
        graph: &'a Graph,
        functions: &'a FunctionDeclarations,
        library: &'a mut BTreeMap<CallableId, Summary>,
        context: &'a ViewContext,
        link: LinkId,
        summarizing: bool,
    ) -> Self {
        Self {
            graph,
            functions,
            library,
            context,
            link,
            summarizing,
            inputs: BTreeMap::new(),
        }
    }

    fn summary(&mut self, function: &CallableId) -> Summary {
        if let Some(summary) = self.library.get(function) {
            return summary.clone();
        }
        // Insert before traversing so a recursive import edge terminates.
        self.library.insert(function.clone(), Summary::default());
        let summary = self
            .context
            .document(function.link)
            .and_then(|document| {
                let definition = document.functions.get(&function.function)?;
                definition.body()?;
                let graph =
                    lash_kernel_check::derive_definition(definition, &document.functions).ok()?;
                let unit = graph.units().first()?;
                let mut analysis = Analysis::new(
                    &graph,
                    &document.functions,
                    self.library,
                    self.context,
                    function.link,
                    true,
                );
                analysis.inputs.extend(
                    unit.params
                        .iter()
                        .enumerate()
                        .map(|(index, binding)| (*binding, index)),
                );
                let mut seen = BTreeSet::new();
                let applied = analysis.body_applies(unit.body, &mut seen);
                let returned = analysis.returns(unit.body, &mut seen);
                Some(Summary {
                    applied: analysis.expand(applied, &mut seen),
                    returned: analysis.expand(returned, &mut seen),
                })
            })
            .unwrap_or_default();
        self.library.insert(function.clone(), summary.clone());
        summary
    }
    fn target(&self, callee: &CalleeNode) -> Option<CallableId> {
        match callee {
            CalleeNode::Local(function) => Some(CallableId {
                link: self.link,
                function: *function,
            }),
            CalleeNode::Import(slot) => self.context.resolve(self.link, *slot),
            CalleeNode::Value(_) => None,
        }
    }
    fn atom(&mut self, atom: &AtomNode, seen: &mut BTreeSet<Seen>) -> Value {
        match atom {
            AtomNode::Variable(reference) => self.reference(reference, seen),
            AtomNode::Literal(literal) => literal_value(literal),
        }
    }

    fn argument(&mut self, args: Arguments<'_>, index: usize, seen: &mut BTreeSet<Seen>) -> Value {
        match args {
            Arguments::Atoms(args) => args.get(index).map(|arg| self.atom(arg, seen)),
            Arguments::Expressions(args) => args.get(index).map(|arg| self.value(*arg, seen)),
        }
        .unwrap_or_default()
    }

    /// Only inputs used by this summary need provenance. In particular a
    /// native scalar calculation cannot send us back through its operands.
    fn arguments(
        &mut self,
        args: Arguments<'_>,
        needed: &Value,
        seen: &mut BTreeSet<Seen>,
    ) -> Vec<Value> {
        let indices: BTreeSet<_> = needed
            .0
            .values()
            .flatten()
            .filter_map(|origin| match origin {
                Origin::Input(index, _) => Some(*index),
                Origin::Callable(_) => None,
            })
            .collect();
        let mut values = Vec::new();
        for index in indices {
            values.resize_with(index + 1, Value::default);
            values[index] = self.argument(args, index, seen);
        }
        values
    }

    fn callee(&mut self, callee: &CalleeNode, seen: &mut BTreeSet<Seen>) -> Value {
        match callee {
            CalleeNode::Value(reference) => self.reference(reference, seen),
            CalleeNode::Import(_) => Value::default(),
            CalleeNode::Local(_) => Value::default(),
        }
    }

    fn action(&mut self, action: Derived<'_>, seen: &mut BTreeSet<Seen>) -> Value {
        let Some((callee, args)) = call(action) else {
            return Value::default();
        };
        if let Some(function) = self.target(callee) {
            let applied = self.summary(&function).applied;
            let args = self.arguments(args, &applied, seen);
            applied.substitute(&args)
        } else {
            self.callee(callee, seen)
        }
    }
    fn result(
        &mut self,
        callee: &CalleeNode,
        args: Arguments<'_>,
        seen: &mut BTreeSet<Seen>,
    ) -> Value {
        if let Some(function) = self.target(callee) {
            let returned = self.summary(&function).returned;
            let args = self.arguments(args, &returned, seen);
            return returned.substitute(&args);
        }
        let mut result = Value::default();
        for origin in self.callee(callee, seen).direct() {
            if let Origin::Callable(site) = origin
                && seen.insert(Seen::Body(site.clone()))
            {
                if let Some(body) = self.body(&site) {
                    result.union(self.returns(body, seen));
                }
                seen.remove(&Seen::Body(site));
            }
        }
        result
    }

    fn reference(&mut self, reference: &Reference, seen: &mut BTreeSet<Seen>) -> Value {
        let key = match reference {
            Reference::Binding(binding) => Seen::Binding(*binding),
            Reference::Session(name) => Seen::Session(name.clone()),
        };
        if !seen.insert(key.clone()) {
            return Value::default();
        }
        let mut result = Value::default();
        if let Reference::Binding(binding) = reference {
            if let Some(index) = self.inputs.get(binding) {
                result.union(Value::origin(Origin::Input(*index, Vec::new())));
            } else if self.graph.binding(*binding).kind == lash_kernel_check::BindingKind::Param {
                result.union(self.parameter(*binding, seen));
            }
        }
        for node in self.graph.nodes() {
            match &node.kind {
                NodeKind::Stmt(StmtNode::Let { binding, value })
                | NodeKind::Expr(ExprNode::Let { binding, value, .. })
                    if *reference == Reference::Binding(*binding) =>
                {
                    result.union(self.value(*value, seen))
                }
                NodeKind::Stmt(StmtNode::Assign {
                    place: PlaceNode::Variable(written),
                    value,
                    ..
                }) if written == reference => result.union(self.value(*value, seen)),
                NodeKind::Stmt(StmtNode::For {
                    binding, iterable, ..
                }) if *reference == Reference::Binding(*binding) => {
                    result.union(self.value(*iterable, seen).member(&Key::Any));
                }
                NodeKind::Stmt(StmtNode::Assign {
                    place: PlaceNode::Member(member),
                    value,
                    ..
                }) => {
                    let (target, member_key) = self.member(member);
                    if matches!(&self.graph.node(target).kind, NodeKind::Expr(ExprNode::Variable(target)) if target == reference)
                    {
                        result.union(self.value(*value, seen).inside(member_key));
                    }
                }
                _ => {}
            }
        }
        seen.remove(&key);
        result
    }

    /// All call sites supply facts to the parameter's lexical identity,
    /// including recursive calls; the active binding guard cuts cycles.
    fn parameter(&mut self, binding: BindingId, seen: &mut BTreeSet<Seen>) -> Value {
        let declaration = self.graph.node(self.graph.binding(binding).declared_at);
        let (site, params) =
            if let NodeKind::Expr(ExprNode::Closure { params, .. }) = &declaration.kind {
                (declaration.site.clone(), params.as_slice())
            } else if let Some(unit) = self
                .graph
                .units()
                .iter()
                .find(|unit| unit.params.contains(&binding))
            {
                (
                    Site::new(unit.unit.clone(), Vec::new()),
                    unit.params.as_slice(),
                )
            } else {
                return Value::default();
            };
        let Some(index) = params.iter().position(|param| *param == binding) else {
            return Value::default();
        };
        let mut result = Value::default();
        for node in self.graph.nodes() {
            if let Some(action) = derived(&node.kind)
                && let Some((callee, args)) = call(action)
                && self
                    .callee(callee, seen)
                    .direct()
                    .contains(&Origin::Callable(site.clone()))
            {
                result.union(self.argument(args, index, seen));
            }
        }
        result
    }

    fn member(&self, member: &MemberNode) -> (NodeId, Key) {
        match member {
            MemberNode::Field { target, field } => (*target, Key::Field(field.clone())),
            MemberNode::Index { target, index } => (
                *target,
                match &self.graph.node(*index).kind {
                    NodeKind::Expr(ExprNode::Literal(literal)) => literal_key(literal),
                    _ => Key::Any,
                },
            ),
        }
    }

    fn value(&mut self, id: NodeId, seen: &mut BTreeSet<Seen>) -> Value {
        if !seen.insert(Seen::Node(id)) {
            return Value::default();
        }
        let result = match &self.graph.node(id).kind {
            NodeKind::Expr(ExprNode::Closure { .. }) => {
                Value::origin(Origin::Callable(self.graph.node(id).site.clone()))
            }
            NodeKind::Expr(ExprNode::Variable(reference)) => self.reference(reference, seen),
            NodeKind::Expr(ExprNode::Literal(literal)) => literal_value(literal),
            NodeKind::Expr(ExprNode::Let { body, .. }) => self.value(*body, seen),
            NodeKind::Expr(ExprNode::If { then, r#else, .. }) => {
                let mut result = self.value(*then, seen);
                result.union(self.value(*r#else, seen));
                result
            }
            NodeKind::Expr(
                ExprNode::Tuple(items) | ExprNode::List(items) | ExprNode::Set(items),
            ) => {
                let mut result = Value::default();
                for (index, item) in items.iter().enumerate() {
                    result.union(
                        self.value(*item, seen)
                            .inside(Key::Index(index.to_string())),
                    );
                }
                result
            }
            NodeKind::Expr(ExprNode::Record(fields)) => {
                let mut result = Value::default();
                for field in fields {
                    result.union(
                        self.value(field.value, seen)
                            .inside(Key::Field(field.field.clone())),
                    );
                }
                result
            }
            NodeKind::Expr(ExprNode::Map(entries)) => {
                let mut result = Value::default();
                for entry in entries {
                    let key = match &self.graph.node(entry.key).kind {
                        NodeKind::Expr(ExprNode::Literal(literal)) => literal_key(literal),
                        _ => Key::Any,
                    };
                    result.union(self.value(entry.value, seen).inside(key));
                }
                result
            }
            NodeKind::Expr(ExprNode::Member(member)) => {
                let (target, key) = self.member(member);
                self.value(target, seen).member(&key)
            }
            NodeKind::Expr(ExprNode::Call { function, args }) => self.result(
                &CalleeNode::Local(*function),
                Arguments::Expressions(args),
                seen,
            ),
            NodeKind::Action(ActionNode::Call { callee, args }) => {
                self.result(callee, Arguments::Atoms(args), seen)
            }
            NodeKind::Expr(ExprNode::Act {
                kind: ActNode::Call(callee),
                args,
            }) => self.result(callee, Arguments::Expressions(args), seen),
            _ => Value::default(),
        };
        seen.remove(&Seen::Node(id));
        result
    }

    fn body(&self, site: &Site) -> Option<NodeId> {
        match &self.graph.node_at(site)?.kind {
            NodeKind::Expr(ExprNode::Closure { body, .. }) => Some(*body),
            NodeKind::Block(_) => Some(self.graph.node_at(site)?.id),
            _ => None,
        }
    }

    fn body_nodes(&self, root: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            result.push(id);
            if !matches!(
                self.graph.node(id).kind,
                NodeKind::Expr(ExprNode::Closure { .. })
            ) {
                pending.extend(self.graph.node(id).children());
            }
        }
        result
    }

    fn body_applies(&mut self, body: NodeId, seen: &mut BTreeSet<Seen>) -> Value {
        let mut result = Value::default();
        for id in self.body_nodes(body) {
            let kind = &self.graph.node(id).kind;
            if let Some(action) = derived(kind) {
                result.union(self.action(action, seen));
            }
            if let NodeKind::Expr(ExprNode::Call { function, args }) = kind {
                let applied = self
                    .summary(&CallableId {
                        link: self.link,
                        function: *function,
                    })
                    .applied;
                let args = self.arguments(Arguments::Expressions(args), &applied, seen);
                result.union(applied.substitute(&args));
            }
        }
        self.expand(result, seen)
    }

    fn returns(&mut self, body: NodeId, seen: &mut BTreeSet<Seen>) -> Value {
        let mut result = Value::default();
        for id in self.body_nodes(body) {
            if let NodeKind::Stmt(StmtNode::Return { value } | StmtNode::ReturnVia { value, .. }) =
                self.graph.node(id).kind
            {
                result.union(self.value(value, seen));
            }
        }
        result
    }

    /// A library closure is an applier of the inputs its body calls. It
    /// never leaks a site outside the document into the host's view.
    fn expand(&mut self, value: Value, seen: &mut BTreeSet<Seen>) -> Value {
        let mut result = Value::default();
        for (path, origins) in value.0 {
            for origin in origins {
                let mut value = if let Origin::Callable(site) = &origin
                    && self.summarizing
                {
                    if seen.insert(Seen::Body(site.clone())) {
                        let value = self
                            .body(site)
                            .map(|body| self.body_applies(body, seen))
                            .unwrap_or_default();
                        seen.remove(&Seen::Body(site.clone()));
                        value
                    } else {
                        Value::default()
                    }
                } else {
                    Value::origin(origin)
                };
                for key in path.iter().rev() {
                    value = value.inside(key.clone());
                }
                result.union(value);
            }
        }
        result
    }
}

fn derived(kind: &NodeKind) -> Option<Derived<'_>> {
    match kind {
        NodeKind::Action(action) => Some(Derived::Statement(action)),
        NodeKind::Expr(ExprNode::Act { kind, args }) => Some(Derived::Nested(kind, args)),
        _ => None,
    }
}

fn literal_value(literal: &Literal) -> Value {
    match literal {
        Literal::Function(name) => Value::origin(Origin::Callable(Site::new(
            Unit::Library(*name),
            Vec::new(),
        ))),
        _ => Value::default(),
    }
}

fn literal_key(literal: &Literal) -> Key {
    match literal {
        Literal::Int(number) => Key::Index(number.to_string()),
        Literal::Float(number) => Key::Index(number.get().to_string()),
        Literal::Text(text) => Key::Field(text.clone()),
        _ => Key::Any,
    }
}

/// The argument slots retain their checked lexical references until needed.
#[derive(Clone, Copy)]
enum Arguments<'a> {
    Atoms(&'a [AtomNode]),
    Expressions(&'a [NodeId]),
}

fn call(action: Derived<'_>) -> Option<(&CalleeNode, Arguments<'_>)> {
    match action {
        Derived::Statement(
            ActionNode::Call { callee, args } | ActionNode::Spawn { callee, args },
        ) => Some((callee, Arguments::Atoms(args))),
        Derived::Nested(ActNode::Call(callee) | ActNode::Spawn(callee), args) => {
            Some((callee, Arguments::Expressions(args)))
        }
        _ => None,
    }
}
