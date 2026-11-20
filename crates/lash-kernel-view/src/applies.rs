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
//! Each graph is solved once per view context. Its results (an expression's
//! value, a variable's value, what a callable body returns or applies) form
//! a dependency graph; its strongly connected components are solved in
//! topological order, each to its least fixpoint with a worklist. Outside a
//! cycle every result is computed once from final inputs. Inside one the
//! answer does not depend on the order results are visited, and it holds
//! every callable that flows around the cycle. Library summaries are solved
//! the same way over the library call graph, so mutually recursive helpers
//! reach their joint least fixpoint. A summary holds only inputs (a library
//! closure expands to what it applies), so a call whose arguments hold
//! nothing needs none: each summary is solved when a call first passes it
//! something, together with everything it calls.
//!
//! Member paths are at most [`DEPTH`] selections deep. A deeper path widens
//! to "some member at least one selection below its first [`DEPTH`] keys":
//! the cap can only add callables, never drop one. A result inside a cycle,
//! and the summary of a recursive helper, keeps at most [`CYCLE_DEPTH`]: a
//! loop that selects from what it last produced shifts its paths one key
//! deeper per lap, and with the full depth its paths over a handful of keys
//! number in the tens of thousands.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, BindingId, BindingKind, CalleeNode, ExprNode, Graph, MemberNode,
    NodeId, NodeKind, PlaceNode, Reference, StmtNode,
};
use lash_kernel_doc::Literal;

use crate::ViewContext;
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;
use {lash_kernel_value::CallableId, lash_kernel_value::LinkId};

use crate::Derived;

/// The most concrete selections a member path keeps.
const DEPTH: usize = 4;

/// The most concrete selections a path keeps in a result inside a cycle.
const CYCLE_DEPTH: usize = 2;

#[cfg(test)]
std::thread_local! {
    /// Results this thread computed: one per evaluation of a result's equation.
    static COMPUTED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Results computed on this thread since the last read.
#[cfg(test)]
pub(crate) fn take_computed() -> u64 {
    COMPUTED.replace(0)
}

/// A member selection; `Any` is a dynamic index or a loop's element. `Deep`
/// ends a widened path: one or more selections of any keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Index(String),
    Field(String),
    Any,
    Deep,
}

/// Keep at most `depth` concrete keys; nothing follows `Deep`.
fn widen(mut path: Vec<Key>, depth: usize) -> Vec<Key> {
    if let Some(deep) = path.iter().position(|key| *key == Key::Deep) {
        path.truncate(deep + 1);
    }
    let concrete = path.len() - usize::from(path.last() == Some(&Key::Deep));
    if concrete > depth {
        path.truncate(depth);
        path.push(Key::Deep);
    }
    path
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Value(BTreeMap<Vec<Key>, BTreeSet<Origin>>);

static EMPTY: Value = Value(BTreeMap::new());

impl Value {
    fn origin(origin: Origin) -> Self {
        Self(BTreeMap::from([(Vec::new(), BTreeSet::from([origin]))]))
    }

    fn union(&mut self, other: Self) {
        for (path, origins) in other.0 {
            self.0.entry(path).or_default().extend(origins);
        }
    }

    fn union_ref(&mut self, other: &Self) {
        for (path, origins) in &other.0 {
            self.0
                .entry(path.clone())
                .or_default()
                .extend(origins.iter().cloned());
        }
    }

    /// Union `other`'s facts in; whether any was new.
    fn absorb(&mut self, other: Self) -> bool {
        let mut grew = false;
        for (path, origins) in other.0 {
            let known = self.0.entry(path).or_default();
            for origin in origins {
                grew |= known.insert(origin);
            }
        }
        grew
    }

    fn member(&self, key: &Key) -> Self {
        if *key == Key::Deep {
            // One or more selections: close over single arbitrary ones.
            let mut result = self.member(&Key::Any);
            let mut frontier = result.clone();
            while !frontier.0.is_empty() {
                let mut next = Self::default();
                for (path, origins) in frontier.member(&Key::Any).0 {
                    for origin in origins {
                        if result
                            .0
                            .entry(path.clone())
                            .or_default()
                            .insert(origin.clone())
                        {
                            next.0.entry(path.clone()).or_default().insert(origin);
                        }
                    }
                }
                frontier = next;
            }
            return result;
        }
        let mut result = Self::default();
        for (path, origins) in &self.0 {
            match path.split_first() {
                Some((Key::Deep, _)) => {
                    // One selection inside one or more: none or more remain.
                    for rest in [Vec::new(), vec![Key::Deep]] {
                        result
                            .0
                            .entry(rest)
                            .or_default()
                            .extend(origins.iter().cloned());
                    }
                }
                Some((first, tail)) => {
                    if first == key || *first == Key::Any || *key == Key::Any {
                        result
                            .0
                            .entry(tail.to_vec())
                            .or_default()
                            .extend(origins.iter().cloned());
                    }
                }
                None => {
                    for origin in origins {
                        if let Origin::Input(index, path) = origin {
                            let mut path = path.clone();
                            path.push(key.clone());
                            result.union(Self::origin(Origin::Input(*index, widen(path, DEPTH))));
                        }
                    }
                }
            }
        }
        result
    }

    fn inside(self, key: Key) -> Self {
        let mut result = Self::default();
        for (mut path, origins) in self.0 {
            path.insert(0, key.clone());
            result
                .0
                .entry(widen(path, DEPTH))
                .or_default()
                .extend(origins);
        }
        result
    }

    /// Widen every path, and every input's selections, to `depth` keys.
    fn limit(self, depth: usize) -> Self {
        let mut result = Self::default();
        for (path, origins) in self.0 {
            result
                .0
                .entry(widen(path, depth))
                .or_default()
                .extend(origins.into_iter().map(|origin| match origin {
                    Origin::Input(index, path) => Origin::Input(index, widen(path, depth)),
                    callable @ Origin::Callable(_) => callable,
                }));
        }
        result
    }

    fn direct(&self) -> &BTreeSet<Origin> {
        static NONE: BTreeSet<Origin> = BTreeSet::new();
        self.0.get([].as_slice()).unwrap_or(&NONE)
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

#[derive(Clone, Default, PartialEq, Eq)]
struct Summary {
    applied: Value,
    returned: Value,
}

/// The facts shared by every derivation from one view context: each library
/// callable's summary and the solved document graph.
#[derive(Default)]
pub(crate) struct Links {
    summaries: Summaries,
    document: Option<Facts>,
}

/// Library summaries solved so far, each when a call first needs it.
type Summaries = RefCell<BTreeMap<CallableId, Arc<Summary>>>;

impl std::fmt::Debug for Links {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Links")
            .field("summaries", &self.summaries.borrow().len())
            .field("document", &self.document.is_some())
            .finish()
    }
}

/// The callables `action` may apply, in site order. The document graph is
/// solved on the first call, each library summary when a call first needs it.
pub(crate) fn applies(graph: &Graph, action: Derived<'_>, context: &ViewContext) -> Vec<Site> {
    let mut links = context
        .links
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Links {
        summaries,
        document,
    } = &mut *links;
    let link = context.reference.link();
    let facts = document.get_or_insert_with(|| Facts::solve(graph, context, link, None, summaries));
    let analysis = Analysis {
        graph,
        context,
        summaries,
        facts,
        read: None,
    };
    analysis
        .expand(analysis.action(action))
        .direct()
        .iter()
        .filter_map(|origin| match origin {
            Origin::Callable(site) => Some(site.clone()),
            Origin::Input(..) => None,
        })
        .collect()
}

/// The library callables a graph's calls target.
fn targets(graph: &Graph, link: LinkId, context: &ViewContext) -> BTreeSet<CallableId> {
    graph
        .nodes()
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::Expr(ExprNode::Call { function, .. }) => Some(CallableId {
                link,
                function: *function,
            }),
            kind => derived(kind)
                .and_then(call)
                .and_then(|(callee, _)| target(callee, link, context)),
        })
        .collect()
}

fn target(callee: &CalleeNode, link: LinkId, context: &ViewContext) -> Option<CallableId> {
    match callee {
        CalleeNode::Local(function) => Some(CallableId {
            link,
            function: *function,
        }),
        CalleeNode::Import(slot) => context.resolve(link, *slot),
        CalleeNode::Value(_) => None,
    }
}

/// Summarize `roots` and every library callable they reach, one strongly
/// connected component of the library call graph at a time.
fn summarize(summaries: &Summaries, context: &ViewContext, roots: BTreeSet<CallableId>) {
    let mut search = Search {
        context,
        summaries,
        order: BTreeMap::new(),
        low: BTreeMap::new(),
        stack: Vec::new(),
        graphs: BTreeMap::new(),
    };
    for root in roots {
        if !search.summaries.borrow().contains_key(&root) && !search.order.contains_key(&root) {
            search.visit(root);
        }
    }
}

/// Tarjan's search over library callables. Recursion follows the call
/// graph's depth, which is that of nested helper calls.
struct Search<'a> {
    context: &'a ViewContext,
    summaries: &'a Summaries,
    order: BTreeMap<CallableId, usize>,
    low: BTreeMap<CallableId, usize>,
    stack: Vec<CallableId>,
    graphs: BTreeMap<CallableId, Option<(Graph, BTreeSet<CallableId>)>>,
}

impl Search<'_> {
    fn visit(&mut self, function: CallableId) {
        let order = self.order.len();
        self.order.insert(function, order);
        self.low.insert(function, order);
        // The component rooted here is everything pushed from here on.
        let at = self.stack.len();
        self.stack.push(function);
        let graph = self.context.function_graph(&function).map(|graph| {
            let callees = targets(&graph, function.link, self.context);
            (graph, callees)
        });
        let callees = graph
            .as_ref()
            .map(|(_, callees)| callees.clone())
            .unwrap_or_default();
        self.graphs.insert(function, graph);
        for callee in callees {
            if self.summaries.borrow().contains_key(&callee) {
                continue;
            }
            let reached = if let Some(order) = self.order.get(&callee) {
                self.stack.contains(&callee).then_some(*order)
            } else {
                self.visit(callee);
                Some(self.low[&callee])
            };
            if let Some(reached) = reached {
                self.low
                    .entry(function)
                    .and_modify(|low| *low = (*low).min(reached));
            }
        }
        if self.low[&function] != order {
            return;
        }
        let members: Vec<_> = self.stack.drain(at..).collect();
        let graphs: Vec<_> = members
            .iter()
            .map(|member| (*member, self.graphs.remove(member).flatten()))
            .collect();
        let recursive = graphs.iter().any(|(_, graph)| {
            graph
                .as_ref()
                .is_some_and(|(_, callees)| members.iter().any(|member| callees.contains(member)))
        });
        for member in &members {
            self.summaries
                .borrow_mut()
                .insert(*member, Arc::new(Summary::default()));
        }
        // From empty summaries up: each pass reads the last pass's, and a
        // component without a recursive edge is final after one.
        loop {
            let mut grew = false;
            for (member, graph) in &graphs {
                let mut summary = graph
                    .as_ref()
                    .map(|(graph, _)| summary(graph, *member, self.context, self.summaries))
                    .unwrap_or_default();
                if recursive {
                    summary = Summary {
                        applied: summary.applied.limit(CYCLE_DEPTH),
                        returned: summary.returned.limit(CYCLE_DEPTH),
                    };
                }
                let mut summaries = self.summaries.borrow_mut();
                if *summaries[member] != summary {
                    summaries.insert(*member, Arc::new(summary));
                    grew = true;
                }
            }
            if !recursive || !grew {
                return;
            }
        }
    }
}

/// What a library callable applies and returns, given the summaries of
/// what it calls.
fn summary(
    graph: &Graph,
    function: CallableId,
    context: &ViewContext,
    summaries: &Summaries,
) -> Summary {
    let Some(unit) = graph.units().first() else {
        return Summary::default();
    };
    let inputs = unit
        .params
        .iter()
        .enumerate()
        .map(|(index, binding)| (*binding, index))
        .collect();
    let facts = Facts::solve(graph, context, function.link, Some(inputs), summaries);
    let analysis = Analysis {
        graph,
        context,
        summaries,
        facts: &facts,
        read: None,
    };
    let returned = analysis.returns(unit.body);
    Summary {
        applied: analysis.body_applies(unit.body),
        returned: analysis.expand(returned),
    }
}

/// A variable a reference reads and an assignment writes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Variable {
    Binding(BindingId),
    Session(Name),
}

impl From<&Reference> for Variable {
    fn from(reference: &Reference) -> Self {
        match reference {
            Reference::Binding(binding) => Self::Binding(*binding),
            Reference::Session(name) => Self::Session(name.clone()),
        }
    }
}

/// A result of the graph's equations.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Task {
    /// The value of an expression or an action's result.
    Expr(NodeId),
    Variable(Variable),
    /// What a callable body returns.
    Returns(Site),
    /// What a callable body applies, when summarizing a library callable.
    Applies(Site),
    /// The callee and the argument at this position of every call through
    /// a value: what any closure's parameter there may read. Like the two
    /// below it computes nothing; it stands for a set of results, so the
    /// dependency graph has one edge per member instead of one per reader
    /// and member, with the same reachability between results.
    Passed(usize),
    /// Every body's [`Task::Returns`]: what a call through a value may read.
    Returned,
    /// Every body's [`Task::Applies`]: what applying an input may read.
    Applied,
}

/// One write to a variable.
#[derive(Clone)]
enum Writer {
    /// `let`, or an assignment of the whole variable.
    Value(NodeId),
    /// A loop binding takes each element of its iterable.
    Each(NodeId),
    /// An assignment to a member of the variable.
    Inside(NodeId, Key),
}

/// One graph's indexes and the solved value of every result.
struct Facts {
    link: LinkId,
    /// A library callable's parameters by input position while summarizing
    /// it; none for the document, whose parameters take their call sites'
    /// arguments.
    inputs: Option<BTreeMap<BindingId, usize>>,
    writers: BTreeMap<Variable, Vec<Writer>>,
    /// Each parameter's callable and position.
    params: BTreeMap<BindingId, (Site, usize)>,
    /// Calls through a value: the callee and the argument slots.
    applications: Vec<NodeId>,
    /// The calls through a value whose callee holds each closure, as far as
    /// solved: a parameter reads its own callers' arguments, not every
    /// callee in the graph.
    callers: BTreeMap<Site, Vec<NodeId>>,
    /// Every callable body in the graph.
    bodies: Vec<Site>,
    ids: BTreeMap<Task, usize>,
    values: Vec<Value>,
}

impl Facts {
    fn solve(
        graph: &Graph,
        context: &ViewContext,
        link: LinkId,
        inputs: Option<BTreeMap<BindingId, usize>>,
        summaries: &Summaries,
    ) -> Self {
        let mut facts = Self::index(graph, link, inputs);
        let (tasks, deps) = facts.dependencies(graph, context);
        facts.values = vec![Value::default(); tasks.len()];
        let mut component = vec![usize::MAX; tasks.len()];
        let mut queued = vec![false; tasks.len()];
        // Inside a cycle, the results that read each result: the static
        // dependencies over-approximate calls through values, so a result
        // is revisited only when one it actually read grows.
        let mut readers: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        // The calls each callee variable makes, and each closure's
        // parameters: when a callee gains a closure, its calls become that
        // closure's callers and its parameters are revisited.
        let mut calls: BTreeMap<usize, Vec<NodeId>> = BTreeMap::new();
        for node in &facts.applications {
            if let Some((CalleeNode::Value(reference), _)) =
                derived(&graph.node(*node).kind).and_then(call)
                && let Some(task) = facts.ids.get(&Task::Variable(reference.into()))
            {
                calls.entry(*task).or_default().push(*node);
            }
        }
        let mut params: BTreeMap<&Site, Vec<usize>> = BTreeMap::new();
        for (binding, (site, _)) in &facts.params {
            if let Some(task) = facts.ids.get(&Task::Variable(Variable::Binding(*binding))) {
                params.entry(site).or_default().push(*task);
            }
        }
        let params: BTreeMap<Site, Vec<usize>> = params
            .into_iter()
            .map(|(site, tasks)| (site.clone(), tasks))
            .collect();
        for (index, members) in components(&deps).iter().enumerate() {
            for member in members {
                component[*member] = index;
            }
            let cyclic = members.len() > 1 || deps[members[0]].contains(&members[0]);
            let mut pending: VecDeque<usize> = members.iter().copied().collect();
            for member in members {
                queued[*member] = true;
            }
            while let Some(task) = pending.pop_front() {
                queued[task] = false;
                let analysis = Analysis {
                    graph,
                    context,
                    summaries,
                    facts: &facts,
                    read: cyclic.then(Default::default),
                };
                let mut value = analysis.compute(&tasks[task]);
                if cyclic {
                    value = value.limit(CYCLE_DEPTH);
                }
                for read in analysis.read.unwrap_or_default().into_inner() {
                    if component[read] == index {
                        readers.entry(read).or_default().insert(task);
                    }
                }
                let before = calls
                    .contains_key(&task)
                    .then(|| facts.values[task].direct().clone());
                if !facts.values[task].absorb(value) {
                    continue;
                }
                let mut revisit: Vec<usize> = Vec::new();
                if let Some(before) = before {
                    let after = facts.values[task].direct().clone();
                    for origin in after.difference(&before) {
                        if let Origin::Callable(site) = origin {
                            facts
                                .callers
                                .entry(site.clone())
                                .or_default()
                                .extend(&calls[&task]);
                            revisit.extend(params.get(site).into_iter().flatten());
                        }
                    }
                }
                if cyclic {
                    revisit.extend(readers.get(&task).into_iter().flatten());
                }
                for reader in revisit {
                    if component[reader] == index && !queued[reader] {
                        queued[reader] = true;
                        pending.push_back(reader);
                    }
                }
            }
        }
        facts
    }

    /// Index writers, parameters, calls through values and bodies once.
    fn index(graph: &Graph, link: LinkId, inputs: Option<BTreeMap<BindingId, usize>>) -> Self {
        let mut writers: BTreeMap<Variable, Vec<Writer>> = BTreeMap::new();
        let mut applications = Vec::new();
        let mut bodies: Vec<Site> = graph
            .units()
            .iter()
            .map(|unit| Site::new(unit.unit.clone(), Vec::new()))
            .collect();
        for node in graph.nodes() {
            if let Some((CalleeNode::Value(_), _)) = derived(&node.kind).and_then(call) {
                applications.push(node.id);
            }
            let (variable, writer) = match &node.kind {
                NodeKind::Expr(ExprNode::Closure { .. }) => {
                    bodies.push(node.site.clone());
                    continue;
                }
                NodeKind::Stmt(StmtNode::Let { binding, value })
                | NodeKind::Expr(ExprNode::Let { binding, value, .. }) => {
                    (Variable::Binding(*binding), Writer::Value(*value))
                }
                NodeKind::Stmt(StmtNode::Assign {
                    place: PlaceNode::Variable(written),
                    value,
                    ..
                }) => (written.into(), Writer::Value(*value)),
                NodeKind::Stmt(StmtNode::For {
                    binding, iterable, ..
                }) => (Variable::Binding(*binding), Writer::Each(*iterable)),
                NodeKind::Stmt(StmtNode::Assign {
                    place: PlaceNode::Member(member),
                    value,
                    ..
                }) => {
                    let (target, key) = member_key(graph, member);
                    let NodeKind::Expr(ExprNode::Variable(target)) = &graph.node(target).kind
                    else {
                        continue;
                    };
                    (target.into(), Writer::Inside(*value, key))
                }
                _ => continue,
            };
            writers.entry(variable).or_default().push(writer);
        }
        let mut params = BTreeMap::new();
        for binding in graph.bindings() {
            if binding.kind != BindingKind::Param
                || inputs
                    .as_ref()
                    .is_some_and(|inputs| inputs.contains_key(&binding.id))
            {
                continue;
            }
            let declaration = graph.node(binding.declared_at);
            let (site, list) =
                if let NodeKind::Expr(ExprNode::Closure { params, .. }) = &declaration.kind {
                    (declaration.site.clone(), params.as_slice())
                } else if let Some(unit) = graph
                    .units()
                    .iter()
                    .find(|unit| unit.params.contains(&binding.id))
                {
                    (
                        Site::new(unit.unit.clone(), Vec::new()),
                        unit.params.as_slice(),
                    )
                } else {
                    continue;
                };
            if let Some(index) = list.iter().position(|param| *param == binding.id) {
                params.insert(binding.id, (site, index));
            }
        }
        Self {
            link,
            inputs,
            writers,
            params,
            applications,
            callers: BTreeMap::new(),
            bodies,
            ids: BTreeMap::new(),
            values: Vec::new(),
        }
    }

    /// Every result reachable from the graph's variables, bodies and call
    /// arguments, and the results each one's equation may read.
    fn dependencies(
        &mut self,
        graph: &Graph,
        context: &ViewContext,
    ) -> (Vec<Task>, Vec<Vec<usize>>) {
        let mut tasks = Vec::new();
        let mut deps = Vec::new();
        let mut roots: Vec<Task> = self.writers.keys().cloned().map(Task::Variable).collect();
        roots.extend(
            graph
                .bindings()
                .iter()
                .filter(|binding| binding.kind == BindingKind::Param)
                .map(|binding| Task::Variable(Variable::Binding(binding.id))),
        );
        roots.extend(self.bodies.iter().cloned().map(Task::Returns));
        // A library body's applications are read only while summarizing;
        // the document's are read by the action being asked about.
        if self.inputs.is_some() {
            roots.extend(self.bodies.iter().cloned().map(Task::Applies));
        }
        for node in graph.nodes() {
            if let Some((_, args)) = derived(&node.kind).and_then(call) {
                roots.extend(argument_tasks(args));
            }
            if let NodeKind::Expr(ExprNode::Call { args, .. }) = &node.kind {
                roots.extend(args.iter().map(|arg| Task::Expr(*arg)));
            }
        }
        let mut pending = Vec::new();
        for root in roots {
            self.intern(root, &mut tasks, &mut pending);
        }
        while let Some(index) = pending.pop() {
            let reads = self.reads(graph, context, &tasks[index]);
            let reads = reads
                .into_iter()
                .map(|task| self.intern(task, &mut tasks, &mut pending))
                .collect::<BTreeSet<_>>();
            if deps.len() <= index {
                deps.resize(index + 1, Vec::new());
            }
            deps[index] = reads.into_iter().collect();
        }
        deps.resize(tasks.len(), Vec::new());
        (tasks, deps)
    }

    fn intern(&mut self, task: Task, tasks: &mut Vec<Task>, pending: &mut Vec<usize>) -> usize {
        let next = tasks.len();
        let index = *self.ids.entry(task.clone()).or_insert(next);
        if index == next {
            tasks.push(task);
            pending.push(index);
        }
        index
    }

    /// The results `task`'s equation may read: a superset of what
    /// [`Analysis::compute`] reads for any values of them.
    fn reads(&self, graph: &Graph, context: &ViewContext, task: &Task) -> Vec<Task> {
        let call_reads = |callee: &CalleeNode, args: Arguments<'_>, reads: &mut Vec<Task>| {
            if target(callee, self.link, context).is_some() {
                reads.extend(argument_tasks(args));
            } else if let CalleeNode::Value(reference) = callee {
                reads.extend([Task::Variable(reference.into()), Task::Returned]);
            }
        };
        let mut reads = Vec::new();
        match task {
            Task::Expr(id) => match &graph.node(*id).kind {
                NodeKind::Expr(ExprNode::Variable(reference)) => {
                    reads.push(Task::Variable(reference.into()))
                }
                NodeKind::Expr(ExprNode::Let { body, .. }) => reads.push(Task::Expr(*body)),
                NodeKind::Expr(ExprNode::If { then, r#else, .. }) => {
                    reads.extend([Task::Expr(*then), Task::Expr(*r#else)])
                }
                NodeKind::Expr(
                    ExprNode::Tuple(items) | ExprNode::List(items) | ExprNode::Set(items),
                ) => reads.extend(items.iter().map(|item| Task::Expr(*item))),
                NodeKind::Expr(ExprNode::Record(fields)) => {
                    reads.extend(fields.iter().map(|field| Task::Expr(field.value)))
                }
                NodeKind::Expr(ExprNode::Map(entries)) => {
                    reads.extend(entries.iter().map(|entry| Task::Expr(entry.value)))
                }
                NodeKind::Expr(ExprNode::Member(member)) => {
                    reads.push(Task::Expr(member_key(graph, member).0))
                }
                NodeKind::Expr(ExprNode::Call { args, .. }) => {
                    reads.extend(args.iter().map(|arg| Task::Expr(*arg)))
                }
                NodeKind::Action(ActionNode::Call { callee, args }) => {
                    call_reads(callee, Arguments::Atoms(args), &mut reads)
                }
                NodeKind::Expr(ExprNode::Act {
                    kind: ActNode::Call(callee),
                    args,
                }) => call_reads(callee, Arguments::Expressions(args), &mut reads),
                _ => {}
            },
            Task::Variable(variable) => {
                if let Variable::Binding(binding) = variable
                    && let Some((_, index)) = self.params.get(binding)
                {
                    reads.push(Task::Passed(*index));
                }
                for writer in self.writers.get(variable).into_iter().flatten() {
                    let (Writer::Value(value) | Writer::Each(value) | Writer::Inside(value, _)) =
                        writer;
                    reads.push(Task::Expr(*value));
                }
            }
            Task::Returns(site) => {
                if let Some(body) = body(graph, site) {
                    reads.extend(returned(graph, body).into_iter().map(Task::Expr));
                }
            }
            Task::Applies(site) => {
                if let Some(body) = body(graph, site) {
                    for id in body_nodes(graph, body) {
                        let kind = &graph.node(id).kind;
                        if let Some((callee, args)) = derived(kind).and_then(call) {
                            if target(callee, self.link, context).is_some() {
                                reads.extend(argument_tasks(args));
                            } else if let CalleeNode::Value(reference) = callee {
                                reads.push(Task::Variable(reference.into()));
                            }
                        }
                        if let NodeKind::Expr(ExprNode::Call { args, .. }) = kind {
                            reads.extend(args.iter().map(|arg| Task::Expr(*arg)));
                        }
                    }
                    reads.push(Task::Applied);
                }
            }
            Task::Passed(index) => {
                for node in &self.applications {
                    if let Some((callee, args)) = derived(&graph.node(*node).kind).and_then(call) {
                        if let CalleeNode::Value(reference) = callee {
                            reads.push(Task::Variable(reference.into()));
                        }
                        reads.extend(argument_task(args, *index));
                    }
                }
            }
            Task::Returned => reads.extend(self.bodies.iter().cloned().map(Task::Returns)),
            Task::Applied => reads.extend(self.bodies.iter().cloned().map(Task::Applies)),
        }
        reads
    }
}

/// Tarjan's strongly connected components of `deps`, each listed before
/// every component that depends on it. Iterative: a cycle may be as long as
/// the graph.
fn components(deps: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNSEEN: usize = usize::MAX;
    let mut order = vec![UNSEEN; deps.len()];
    let mut low = vec![0; deps.len()];
    let mut on_stack = vec![false; deps.len()];
    let mut stack = Vec::new();
    let mut result = Vec::new();
    let mut next = 0;
    for root in 0..deps.len() {
        if order[root] != UNSEEN {
            continue;
        }
        let mut frames = vec![(root, 0)];
        order[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some((node, edge)) = frames.last_mut() {
            let node = *node;
            if let Some(&dep) = deps[node].get(*edge) {
                *edge += 1;
                if order[dep] == UNSEEN {
                    order[dep] = next;
                    low[dep] = next;
                    next += 1;
                    stack.push(dep);
                    on_stack[dep] = true;
                    frames.push((dep, 0));
                } else if on_stack[dep] {
                    low[node] = low[node].min(order[dep]);
                }
                continue;
            }
            frames.pop();
            if let Some((parent, _)) = frames.last() {
                low[*parent] = low[*parent].min(low[node]);
            }
            if low[node] == order[node] {
                let mut members = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                result.push(members);
            }
        }
    }
    result
}

/// The equations, read against one graph's current results.
struct Analysis<'a> {
    graph: &'a Graph,
    context: &'a ViewContext,
    summaries: &'a Summaries,
    facts: &'a Facts,
    /// The results read, while solving a cycle.
    read: Option<std::cell::RefCell<Vec<usize>>>,
}

impl Analysis<'_> {
    fn get(&self, task: &Task) -> &Value {
        let Some(index) = self.facts.ids.get(task) else {
            return &EMPTY;
        };
        if let Some(read) = &self.read {
            read.borrow_mut().push(*index);
        }
        &self.facts.values[*index]
    }

    fn compute(&self, task: &Task) -> Value {
        #[cfg(test)]
        COMPUTED.with(|count| count.set(count.get() + 1));
        match task {
            Task::Expr(id) => self.value(*id),
            Task::Variable(variable) => self.variable(variable),
            Task::Returns(site) => body(self.graph, site)
                .map(|body| self.returns(body))
                .unwrap_or_default(),
            Task::Applies(site) => body(self.graph, site)
                .map(|body| self.body_applies(body))
                .unwrap_or_default(),
            Task::Passed(_) | Task::Returned | Task::Applied => Value::default(),
        }
    }

    /// A library callable's summary, solved now if no call needed it yet.
    /// While summarizing, every callee's is solved before its caller's.
    fn summary(&self, function: &CallableId) -> Arc<Summary> {
        if let Some(summary) = self.summaries.borrow().get(function) {
            return summary.clone();
        }
        summarize(self.summaries, self.context, BTreeSet::from([*function]));
        self.summaries
            .borrow()
            .get(function)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether no argument holds a callable or an input. A summary holds
    /// only inputs, so it adds nothing at such a call and is not needed.
    fn empty(&self, args: Arguments<'_>) -> bool {
        match args {
            Arguments::Atoms(args) => args.iter().all(|arg| match arg {
                AtomNode::Variable(reference) => {
                    self.get(&Task::Variable(reference.into())).0.is_empty()
                }
                AtomNode::Literal(literal) => literal_value(literal).0.is_empty(),
            }),
            Arguments::Expressions(args) => args
                .iter()
                .all(|arg| self.get(&Task::Expr(*arg)).0.is_empty()),
        }
    }

    fn target(&self, callee: &CalleeNode) -> Option<CallableId> {
        target(callee, self.facts.link, self.context)
    }

    fn argument(&self, args: Arguments<'_>, index: usize) -> Value {
        match args {
            Arguments::Atoms(args) => match args.get(index) {
                Some(AtomNode::Variable(reference)) => {
                    self.get(&Task::Variable(reference.into())).clone()
                }
                Some(AtomNode::Literal(literal)) => literal_value(literal),
                None => Value::default(),
            },
            Arguments::Expressions(args) => args
                .get(index)
                .map(|arg| self.get(&Task::Expr(*arg)).clone())
                .unwrap_or_default(),
        }
    }

    /// Only inputs used by this summary need provenance. In particular a
    /// native scalar calculation cannot send us back through its operands.
    fn arguments(&self, args: Arguments<'_>, needed: &Value) -> Vec<Value> {
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
            values[index] = self.argument(args, index);
        }
        values
    }

    fn callee(&self, callee: &CalleeNode) -> &Value {
        match callee {
            CalleeNode::Value(reference) => self.get(&Task::Variable(reference.into())),
            CalleeNode::Import(_) | CalleeNode::Local(_) => &EMPTY,
        }
    }

    fn action(&self, action: Derived<'_>) -> Value {
        let Some((callee, args)) = call(action) else {
            return Value::default();
        };
        if let Some(function) = self.target(callee) {
            if self.empty(args) {
                return Value::default();
            }
            let summary = self.summary(&function);
            summary
                .applied
                .substitute(&self.arguments(args, &summary.applied))
        } else {
            self.callee(callee).clone()
        }
    }

    fn result(&self, callee: &CalleeNode, args: Arguments<'_>) -> Value {
        if let Some(function) = self.target(callee) {
            if self.empty(args) {
                return Value::default();
            }
            let summary = self.summary(&function);
            return summary
                .returned
                .substitute(&self.arguments(args, &summary.returned));
        }
        let mut result = Value::default();
        for origin in self.callee(callee).direct() {
            if let Origin::Callable(site) = origin {
                result.union_ref(self.get(&Task::Returns(site.clone())));
            }
        }
        result
    }

    fn variable(&self, variable: &Variable) -> Value {
        let mut result = Value::default();
        if let Variable::Binding(binding) = variable {
            if let Some(index) = self
                .facts
                .inputs
                .as_ref()
                .and_then(|inputs| inputs.get(binding))
            {
                result.union(Value::origin(Origin::Input(*index, Vec::new())));
            } else if let Some((site, index)) = self.facts.params.get(binding) {
                // Every call site supplies the parameter, recursive ones too.
                for node in self.facts.callers.get(site).into_iter().flatten() {
                    if let Some((_, args)) = derived(&self.graph.node(*node).kind).and_then(call) {
                        result.union(self.argument(args, *index));
                    }
                }
            }
        }
        for writer in self.facts.writers.get(variable).into_iter().flatten() {
            match writer {
                Writer::Value(value) => result.union_ref(self.get(&Task::Expr(*value))),
                Writer::Each(iterable) => {
                    result.union(self.get(&Task::Expr(*iterable)).member(&Key::Any))
                }
                Writer::Inside(value, key) => {
                    result.union(self.get(&Task::Expr(*value)).clone().inside(key.clone()))
                }
            }
        }
        result
    }

    fn value(&self, id: NodeId) -> Value {
        let item = |id: &NodeId| self.get(&Task::Expr(*id)).clone();
        match &self.graph.node(id).kind {
            NodeKind::Expr(ExprNode::Closure { .. }) => {
                Value::origin(Origin::Callable(self.graph.node(id).site.clone()))
            }
            NodeKind::Expr(ExprNode::Variable(reference)) => {
                self.get(&Task::Variable(reference.into())).clone()
            }
            NodeKind::Expr(ExprNode::Literal(literal)) => literal_value(literal),
            NodeKind::Expr(ExprNode::Let { body, .. }) => item(body),
            NodeKind::Expr(ExprNode::If { then, r#else, .. }) => {
                let mut result = item(then);
                result.union(item(r#else));
                result
            }
            NodeKind::Expr(
                ExprNode::Tuple(items) | ExprNode::List(items) | ExprNode::Set(items),
            ) => {
                let mut result = Value::default();
                for (index, element) in items.iter().enumerate() {
                    result.union(item(element).inside(Key::Index(index.to_string())));
                }
                result
            }
            NodeKind::Expr(ExprNode::Record(fields)) => {
                let mut result = Value::default();
                for field in fields {
                    result.union(item(&field.value).inside(Key::Field(field.field.clone())));
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
                    result.union(item(&entry.value).inside(key));
                }
                result
            }
            NodeKind::Expr(ExprNode::Member(member)) => {
                let (target, key) = member_key(self.graph, member);
                self.get(&Task::Expr(target)).member(&key)
            }
            NodeKind::Expr(ExprNode::Call { function, args }) => {
                self.result(&CalleeNode::Local(*function), Arguments::Expressions(args))
            }
            NodeKind::Action(ActionNode::Call { callee, args }) => {
                self.result(callee, Arguments::Atoms(args))
            }
            NodeKind::Expr(ExprNode::Act {
                kind: ActNode::Call(callee),
                args,
            }) => self.result(callee, Arguments::Expressions(args)),
            _ => Value::default(),
        }
    }

    fn body_applies(&self, body: NodeId) -> Value {
        let mut result = Value::default();
        for id in body_nodes(self.graph, body) {
            let kind = &self.graph.node(id).kind;
            if let Some(action) = derived(kind) {
                result.union(self.action(action));
            }
            if let NodeKind::Expr(ExprNode::Call { function, args }) = kind
                && !self.empty(Arguments::Expressions(args))
            {
                let summary = self.summary(&CallableId {
                    link: self.facts.link,
                    function: *function,
                });
                let args = self.arguments(Arguments::Expressions(args), &summary.applied);
                result.union(summary.applied.substitute(&args));
            }
        }
        self.expand(result)
    }

    fn returns(&self, body: NodeId) -> Value {
        let mut result = Value::default();
        for value in returned(self.graph, body) {
            result.union_ref(self.get(&Task::Expr(value)));
        }
        result
    }

    /// A library closure is an applier of the inputs its body calls. It
    /// never leaks a site outside the document into the host's view.
    fn expand(&self, value: Value) -> Value {
        if self.facts.inputs.is_none() {
            return value;
        }
        let mut result = Value::default();
        for (path, origins) in value.0 {
            for origin in origins {
                let mut value = match &origin {
                    Origin::Callable(site) => self.get(&Task::Applies(site.clone())).clone(),
                    Origin::Input(..) => Value::origin(origin),
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

fn body(graph: &Graph, site: &Site) -> Option<NodeId> {
    match &graph.node_at(site)?.kind {
        NodeKind::Expr(ExprNode::Closure { body, .. }) => Some(*body),
        NodeKind::Block(_) => Some(graph.node_at(site)?.id),
        _ => None,
    }
}

/// A body's own nodes, without the bodies of closures it writes.
fn body_nodes(graph: &Graph, root: NodeId) -> Vec<NodeId> {
    let mut result = Vec::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        result.push(id);
        if !matches!(
            graph.node(id).kind,
            NodeKind::Expr(ExprNode::Closure { .. })
        ) {
            pending.extend(graph.node(id).children());
        }
    }
    result
}

/// The values a body returns.
fn returned(graph: &Graph, body: NodeId) -> Vec<NodeId> {
    body_nodes(graph, body)
        .into_iter()
        .filter_map(|id| match graph.node(id).kind {
            NodeKind::Stmt(StmtNode::Return { value } | StmtNode::ReturnVia { value, .. }) => {
                Some(value)
            }
            _ => None,
        })
        .collect()
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

fn member_key(graph: &Graph, member: &MemberNode) -> (NodeId, Key) {
    match member {
        MemberNode::Field { target, field } => (*target, Key::Field(field.clone())),
        MemberNode::Index { target, index } => (
            *target,
            match &graph.node(*index).kind {
                NodeKind::Expr(ExprNode::Literal(literal)) => literal_key(literal),
                _ => Key::Any,
            },
        ),
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

fn argument_task(args: Arguments<'_>, index: usize) -> Option<Task> {
    match args {
        Arguments::Atoms(args) => match args.get(index)? {
            AtomNode::Variable(reference) => Some(Task::Variable(reference.into())),
            AtomNode::Literal(_) => None,
        },
        Arguments::Expressions(args) => args.get(index).map(|arg| Task::Expr(*arg)),
    }
}

fn argument_tasks(args: Arguments<'_>) -> Vec<Task> {
    let count = match args {
        Arguments::Atoms(args) => args.len(),
        Arguments::Expressions(args) => args.len(),
    };
    (0..count)
        .filter_map(|index| argument_task(args, index))
        .collect()
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
