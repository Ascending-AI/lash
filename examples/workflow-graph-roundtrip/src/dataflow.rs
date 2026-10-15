//! A derived, dialect-independent authoring view. IDs belong to one document
//! identity; edits always address canonical sites, never these transient IDs.

use lash::workflow::document::{
    Action, Atom, Document, Expr, Name, Node, Place, Site, Stmt, Type, Unit,
};
use lash::workflow::graph::{
    ActionNode, AtomNode, BindingId, BindingKind, CalleeNode, ExprNode, Graph, GraphNode, NodeId,
    NodeKind, Reference, ScopeId, StmtNode,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dataflow {
    pub bindings: Vec<BindingView>,
    pub expressions: Vec<ExpressionView>,
    pub actions: Vec<ActionView>,
    pub parameters: Vec<usize>,
    pub returns: Vec<ReturnView>,
    pub assignments: Vec<AssignmentView>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingView {
    pub id: usize,
    pub name: Name,
    pub kind: String,
    pub site: Site,
    pub block: Site,
    #[serde(rename = "type")]
    pub ty: Type,
    pub assigned: bool,
    pub captured: bool,
    pub value_site: Option<Site>,
    pub uses: Vec<Site>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpressionView {
    pub site: Site,
    pub statement: Site,
    pub block: Site,
    pub expression: Expr,
    #[serde(rename = "type")]
    pub ty: Type,
    pub references: Vec<usize>,
    pub available: Vec<usize>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionView {
    pub destination: Option<Place>,
    pub site: Site,
    pub statement: Site,
    pub block: Site,
    pub args: Vec<ArgumentView>,
    pub result_type: Type,
    pub available: Vec<usize>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArgumentView {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    pub atom: Atom,
    pub binding: Option<usize>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReturnView {
    pub callable: Site,
    pub expected_type: Type,
    pub statement: Site,
    pub expression_site: Site,
    #[serde(rename = "type")]
    pub ty: Type,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignmentView {
    pub statement: Site,
    pub value_site: Site,
    pub destination: Place,
}

fn return_contract(
    document: &Document,
    graph: &Graph,
    node: &GraphNode,
    entry: &Name,
) -> (Site, Type) {
    if !matches!(node.kind, NodeKind::Stmt(StmtNode::Finish { .. })) {
        let mut parent = node.parent;
        while let Some(id) = parent {
            let ancestor = graph.node(id);
            if matches!(ancestor.kind, NodeKind::Expr(ExprNode::Closure { .. })) {
                let expected = match &ancestor.facet {
                    Some(Type::Function(signature)) => signature.result.clone(),
                    _ => Type::Any,
                };
                return (ancestor.site.clone(), expected);
            }
            parent = ancestor.parent;
        }
    }
    (
        Site::new(Unit::Function(entry.clone()), vec![]),
        document
            .entries
            .get(entry)
            .map(|signature| signature.result.clone())
            .unwrap_or(Type::Any),
    )
}

fn statement<'a>(graph: &'a Graph, mut node: &'a GraphNode) -> Site {
    while !matches!(node.kind, NodeKind::Stmt(_)) {
        let Some(parent) = node.parent else { break };
        node = graph.node(parent);
    }
    node.site.clone()
}
fn block(graph: &Graph, scope: ScopeId) -> Site {
    graph.node(graph.scope(scope).block).site.clone()
}
fn direct_references(node: &GraphNode) -> Vec<BindingId> {
    let mut out = Vec::new();
    let mut add = |reference: &Reference| {
        if let Reference::Binding(id) = reference {
            out.push(*id);
        }
    };
    match &node.kind {
        NodeKind::Expr(ExprNode::Variable(reference)) => add(reference),
        NodeKind::Expr(ExprNode::Closure { captures, .. }) => {
            for reference in captures {
                add(reference);
            }
        }
        NodeKind::Action(action) => {
            match action {
                ActionNode::Call {
                    callee: CalleeNode::Value(reference),
                    ..
                }
                | ActionNode::Spawn {
                    callee: CalleeNode::Value(reference),
                    ..
                } => add(reference),
                _ => {}
            }
            for atom in atoms(action) {
                if let AtomNode::Variable(reference) = atom {
                    add(reference);
                }
            }
        }
        _ => {}
    }
    out
}
fn atoms(action: &ActionNode) -> Vec<&AtomNode> {
    match action {
        ActionNode::Call { args, .. }
        | ActionNode::Spawn { args, .. }
        | ActionNode::Perform { args, .. } => args.iter().collect(),
        ActionNode::Sleep { duration } => vec![duration],
        ActionNode::Join { task } | ActionNode::Cancel { task } => vec![task],
        ActionNode::JoinMany { tasks, .. } => vec![tasks],
        ActionNode::Yield => vec![],
    }
}
fn references(graph: &Graph, id: NodeId, out: &mut BTreeSet<usize>) {
    let node = graph.node(id);
    out.extend(direct_references(node).into_iter().map(BindingId::index));
    if !matches!(node.kind, NodeKind::Expr(ExprNode::Closure { .. })) {
        for child in node.children() {
            references(graph, child, out);
        }
    }
}
/// Resolve selectable names exactly as lexical lookup would at this site.
/// An initializer cannot select its own declaration; later and shadowed
/// bindings cannot be selected, even if they have a matching display name.
fn available(graph: &Graph, node: &GraphNode) -> Vec<usize> {
    let mut scope = Some(node.scope);
    let mut names = BTreeMap::new();
    while let Some(id) = scope {
        let current = graph.scope(id);
        for binding_id in current.bindings.iter().rev() {
            let binding = graph.binding(*binding_id);
            let declaration = graph.node(binding.declared_at);
            if binding.kind == BindingKind::Let
                && (declaration.id.index() >= node.id.index()
                    || (declaration.site.unit == node.site.unit
                        && node.site.path.starts_with(&declaration.site.path)))
            {
                continue;
            }
            names
                .entry(binding.name.clone())
                .or_insert(binding.id.index());
        }
        scope = current.parent;
    }
    names.into_values().collect()
}
fn action_result(document: &Document, node: &GraphNode) -> Type {
    match &node.kind {
        NodeKind::Action(ActionNode::Perform {
            effect,
            result: Type::Any,
            ..
        }) => document
            .manifest
            .effects
            .get(effect)
            .map(|s| s.result.clone())
            .unwrap_or(Type::Any),
        _ => node.facet.clone().unwrap_or(Type::Any),
    }
}

pub fn project(document: &Document, graph: &Graph, entry: &Name) -> Dataflow {
    let mut view = Dataflow::default();
    let unit = Unit::Function(entry.clone());
    if let Some(entry) = graph.unit(&unit) {
        view.parameters = entry.params.iter().copied().map(BindingId::index).collect();
    }
    for binding in graph.bindings() {
        let declaration = graph.node(binding.declared_at);
        if declaration.site.unit != unit {
            continue;
        }
        let value = match declaration.kind {
            NodeKind::Stmt(StmtNode::Let { value, .. }) => Some(graph.node(value)),
            _ => None,
        };
        view.bindings.push(BindingView {
            id: binding.id.index(),
            name: binding.name.clone(),
            kind: format!("{:?}", binding.kind).to_lowercase(),
            site: declaration.site.clone(),
            block: block(graph, binding.scope),
            ty: binding.facet.clone(),
            assigned: binding.assigned,
            captured: binding.captured,
            value_site: value.map(|n| n.site.clone()),
            uses: graph
                .nodes()
                .iter()
                .flat_map(|node| {
                    direct_references(node)
                        .into_iter()
                        .filter(move |id| *id == binding.id)
                        .map(move |_| node.site.clone())
                })
                .collect(),
        });
    }
    for node in graph.nodes().iter().filter(|node| node.site.unit == unit) {
        match document.node(&node.site) {
            Some(Node::Expr(expression)) => {
                let mut deps = BTreeSet::new();
                references(graph, node.id, &mut deps);
                view.expressions.push(ExpressionView {
                    site: node.site.clone(),
                    statement: statement(graph, node),
                    block: block(graph, node.scope),
                    expression: expression.clone(),
                    ty: node.facet.clone().unwrap_or(Type::Any),
                    references: deps.into_iter().collect(),
                    available: available(graph, node),
                });
            }
            Some(Node::Action(action)) => {
                let NodeKind::Action(derived) = &node.kind else {
                    continue;
                };
                let signature = match action {
                    Action::Perform { effect, .. } => document.manifest.effects.get(effect),
                    _ => None,
                };
                let raw: Vec<&Atom> = match action {
                    Action::Call { args, .. }
                    | Action::Spawn { args, .. }
                    | Action::Perform { args, .. } => args.iter().collect(),
                    Action::Sleep { duration } => vec![duration],
                    Action::Join { task } | Action::Cancel { task } => vec![task],
                    Action::JoinMany { tasks, .. } => vec![tasks],
                    Action::Yield => vec![],
                };
                let args = raw
                    .into_iter()
                    .zip(atoms(derived))
                    .enumerate()
                    .map(|(index, (atom, derived))| {
                        let param = signature.and_then(|s| s.params.get(index));
                        ArgumentView {
                            name: param
                                .map(|p| p.name.to_string())
                                .unwrap_or_else(|| format!("argument{}", index + 1)),
                            ty: param.map(|p| p.ty.clone()).unwrap_or(Type::Any),
                            atom: atom.clone(),
                            binding: match derived {
                                AtomNode::Variable(Reference::Binding(id)) => Some(id.index()),
                                _ => None,
                            },
                        }
                    })
                    .collect();
                let destination = match document.node(&statement(graph, node)) {
                    Some(Node::Stmt(Stmt::Assign { place, .. })) => Some(place.clone()),
                    _ => None,
                };
                view.actions.push(ActionView {
                    destination,
                    site: node.site.clone(),
                    statement: statement(graph, node),
                    block: block(graph, node.scope),
                    args,
                    result_type: action_result(document, node),
                    available: available(graph, node),
                });
            }
            _ => {}
        }
        if let NodeKind::Stmt(StmtNode::Return { value } | StmtNode::Finish { value }) = node.kind {
            let expression = graph.node(value);
            let (callable, expected_type) = return_contract(document, graph, node, entry);
            view.returns.push(ReturnView {
                callable,
                expected_type,
                statement: node.site.clone(),
                expression_site: expression.site.clone(),
                ty: expression.facet.clone().unwrap_or(Type::Any),
            });
        }
        if let NodeKind::Stmt(StmtNode::Assign { value, .. }) = node.kind
            && let Some(Node::Stmt(Stmt::Assign { place, .. })) = document.node(&node.site)
        {
            view.assignments.push(AssignmentView {
                statement: node.site.clone(),
                value_site: graph.node(value).site.clone(),
                destination: place.clone(),
            });
        }
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash::workflow::document::{FunctionRegistry, parse_document};

    /// K-ADM-005: aliases passed to another function can change record fields.
    #[test]
    fn helper_mutations_do_not_leave_stale_field_types_in_the_editor() {
        for mutation in [
            "let change = call mutator(record) do apply change()",
            "do call mutate(record)",
        ] {
            let document = parse_document(&format!(
                r#"kernel 1
numbers float
entry demo() -> Any
fn mutate(record) {{ set record.value = 42 }}
fn mutator(record) {{ return fn() {{ set record.value = 42 }} }}
fn demo() {{
  let record = {{value: "initial"}}
  {mutation}
  return record.value
}}
main {{ finish null }}
"#
            ))
            .expect("canonical helper mutation fixture");
            let graph = lash::workflow::graph::derive(&document, &FunctionRegistry::default())
                .expect("graph");
            let view = project(&document, &graph, &Name::new("demo"));
            let output = view.returns.first().expect("entry return");
            assert_eq!(
                output.ty,
                Type::Any,
                "{mutation}: a helper can replace Text with a number through an alias"
            );
            let expression = view
                .expressions
                .iter()
                .find(|expression| expression.site == output.expression_site)
                .expect("returned field expression");
            assert_eq!(expression.ty, Type::Any);
        }
    }

    #[test]
    fn source_choices_preserve_shadowing_and_exclude_own_initializers() {
        let document = parse_document(
            r#"kernel 1
numbers float
entry demo(source: Text) -> Any
fn demo(source) {
  let shared = source
  let capture = fn(source) {
    let selected = source
    return selected
  }
  let source = shared
  return source
}
main { finish null }
"#,
        )
        .expect("valid canonical fixture");
        let graph =
            lash::workflow::graph::derive(&document, &FunctionRegistry::default()).expect("graph");
        let view = project(&document, &graph, &Name::new("demo"));
        let params: Vec<_> = view
            .bindings
            .iter()
            .filter(|b| b.name.as_str() == "source" && b.kind == "param")
            .collect();
        assert_eq!(params.len(), 2);
        let selected = view
            .bindings
            .iter()
            .find(|b| b.name.as_str() == "selected")
            .expect("selected");
        let initializer = view
            .expressions
            .iter()
            .find(|e| Some(&e.site) == selected.value_site.as_ref())
            .expect("initializer");
        assert_eq!(initializer.references, vec![params[1].id]);
        assert!(initializer.available.contains(&params[1].id));
        assert!(!initializer.available.contains(&params[0].id));
        assert!(!initializer.available.contains(&selected.id));
        let replacement = view
            .bindings
            .iter()
            .find(|b| b.name.as_str() == "source" && b.kind == "let")
            .expect("replacement");
        let tail = view
            .expressions
            .iter()
            .find(|e| e.site.path == [3, 0])
            .expect("return source");
        assert_eq!(tail.references, vec![replacement.id]);
        assert!(tail.available.contains(&replacement.id));
        assert!(!tail.available.contains(&params[0].id));
    }
    #[test]
    fn returns_and_assignments_preserve_callable_and_write_identity() {
        let document = parse_document(
            r#"kernel 1
numbers float
effect demo.read() -> Text
entry demo() -> Text
fn demo() {
  let state = "initial"
  set state = perform demo.read() as Any
  set state = "changed"
  let record = {value: "initial"}
  set record.value = state
  set record["value"] = state
  let nested = fn() { return state }
  if true { return state }
  return state
}
main { finish null }
"#,
        )
        .expect("canonical state and return fixture");
        let graph =
            lash::workflow::graph::derive(&document, &FunctionRegistry::default()).expect("graph");
        let view = project(&document, &graph, &Name::new("demo"));
        assert_eq!(
            view.assignments
                .iter()
                .map(|write| write.value_site.path.clone())
                .collect::<Vec<_>>(),
            vec![vec![1, 0], vec![2, 0], vec![4, 1], vec![5, 2]]
        );
        assert_eq!(
            view.actions[0].destination,
            Some(Place::Variable(Name::new("state")))
        );
        let nested = view
            .returns
            .iter()
            .find(|output| output.callable.path == [6, 0])
            .expect("closure return belongs to closure");
        assert_eq!(nested.expected_type, Type::Any);
        let entry_returns: Vec<_> = view
            .returns
            .iter()
            .filter(|output| output.callable.path.is_empty())
            .collect();
        assert_eq!(entry_returns.len(), 2);
        assert!(
            entry_returns
                .iter()
                .all(|output| output.expected_type == Type::Text)
        );
    }
}
