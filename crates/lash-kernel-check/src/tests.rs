//! Laws of the checker. Each pins a named rule of
//! `docs/kernel/semantics.md`.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Annotations, Document, EffectName, FunctionDefinition, FunctionId, InvalidReason, Label, Name,
    Node, NodeAnnotation, Param, Signature, Site, Type, Unit, parse_definition, parse_document,
    validate_annotations,
};

use crate::{
    ActionNode, BindingKind, Edge, EdgeKind, Environment, ExprNode, Graph, NodeId, NodeKind,
    Reference, RefusalReason, Refused, SiteKind, StmtNode, Target, admit, derive,
    derive_definition, requirements,
};

/// The library the laws run against: a native function, a function with
/// only a kernel body, and one whose body calls that.
struct Library {
    catalog: BTreeMap<FunctionId, FunctionDefinition>,
    neg: FunctionId,
    twice: FunctionId,
    outer: FunctionId,
}

fn register(catalog: &mut BTreeMap<FunctionId, FunctionDefinition>, text: &str) -> FunctionId {
    let definition = parse_definition(text).unwrap_or_else(|error| panic!("{error}\n{text}"));
    let function = definition.identity().unwrap();
    catalog.insert(function, definition);
    function
}

fn library() -> Library {
    let mut catalog = BTreeMap::new();
    let neg = register(
        &mut catalog,
        "function num.neg(x: Number) -> Number\nkernel 1\ncharge 1\nnative\n",
    );
    let twice = register(
        &mut catalog,
        "function helper.twice(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }\n",
    );
    let outer = register(
        &mut catalog,
        &format!(
            "function helper.outer(x: Any) -> Any\nkernel 1\ncharge 1\n\
             use helper.twice = @{twice}\nbody {{\n  let y = invoke helper.twice(x)\n  return y\n}}\n"
        ),
    );
    Library {
        catalog,
        neg,
        twice,
        outer,
    }
}

fn document(text: &str) -> Document {
    parse_document(text).unwrap_or_else(|error| panic!("{error}\n{text}"))
}

fn signature(params: &[(&str, Type)], result: Type) -> Signature {
    Signature {
        params: params
            .iter()
            .map(|(name, ty)| Param {
                name: Name::new(*name),
                ty: ty.clone(),
                optional: false,
            })
            .collect(),
        result,
    }
}

fn effect(name: &str) -> EffectName {
    EffectName::new(name).unwrap()
}

/// An environment that provides `fetch(url: Text) -> Any` and the library.
fn environment(library: &Library) -> Environment<'_> {
    let mut environment = Environment::new(&library.catalog);
    environment.effects.insert(
        effect("fetch"),
        signature(&[("url", Type::Text)], Type::Any),
    );
    environment
}

/// The reasons `text` is refused for, in `environment`.
fn refusals(text: &str, environment: &Environment<'_>) -> Vec<Refused> {
    match admit(&document(text), environment) {
        Ok(_) => panic!("admitted:\n{text}"),
        Err(refusal) => refusal.errors,
    }
}

fn main_site(path: &[u32]) -> Site {
    Site::new(Unit::Main, path)
}

/// One document that uses every form, and admits. It reads one session
/// binding, `seen`.
fn every_form(library: &Library) -> String {
    format!(
        r#"kernel 1
numbers float
effect fetch(url: Text) -> Any
use num.neg = @{neg}
use helper.twice = @{twice}
use helper.outer = @{outer}
entry start(count: Int) -> Any

fn start(count) {{
  let page = perform fetch("u") as Record{{status: Int, ..Any}}
  do sleep 10
  do yield
  return page
}}

main {{
  let xs = [1, 2.5]
  let t = (xs, "a", b"00", null, absent, true)
  let m = map{{"k": 1}}
  let s = set{{1, 2}}
  let r = {{name: "n"}}
  set r.name = m["k"]
  set xs[0] = num.neg(1)
  set seen = clock
  remove m["k"]
  remove r.name
  if true {{
    let inner = 1
  }} else {{
    while false {{
      break
    }}
  }}
  for x in xs {{
    if s[x] {{
      continue
    }}
  }}
  try {{
    throw r
  }} catch e {{
    print e
  }} finally {{
    print "done"
  }}
  let f = fn(a) {{
    let g = fn() {{
      return a
    }}
    return g
  }}
  let roll = random
  let got = read(t, {{path: ["a", 0]}})
  let h = spawn call start(1)
  let h2 = spawn apply f(h)
  let hs = [h, h2]
  let first = join h
  let every = join all hs
  do cancel h
  let twice = invoke helper.outer(first)
  let applied = apply f(&start)
  let again = call start(2)
  if roll {{
    fail "no"
  }}
  finish got
}}
"#,
        neg = library.neg,
        twice = library.twice,
        outer = library.outer,
    )
}

/// `K-ADM-001` to `K-ADM-009`: a document whose requirements the
/// environment meets is admitted under its own identity, and what it
/// derives is what its code says.
#[test]
fn a_document_that_uses_every_form_admits() {
    let library = library();
    let document = document(&every_form(&library));
    let mut environment = environment(&library);

    // `seen` is a session binding the environment has not provided yet.
    let refused = admit(&document, &environment).unwrap_err();
    assert_eq!(
        refused.errors,
        [Refused {
            site: Some(main_site(&[7])),
            reason: RefusalReason::UnboundVariable {
                name: Name::new("seen")
            },
        }]
    );

    environment.bindings.insert(Name::new("seen"));
    let admitted = admit(&document, &environment).unwrap_or_else(|refusal| panic!("{refusal}"));
    assert_eq!(admitted.identity, document.identity().unwrap());

    // The manifest the document carries is the one its code derives.
    let derived = requirements(&document, &library.catalog);
    assert_eq!(
        derived.effects,
        document.manifest.effects.keys().cloned().collect()
    );
    assert_eq!(derived.functions, document.manifest.functions);

    // Effect sets (`K-FN-001`): `start` performs `fetch`; `main` reaches it
    // by `call`, `spawn` and a reference, and also calls through values.
    let graph = &admitted.graph;
    let start = graph.unit(&Unit::Function(Name::new("start"))).unwrap();
    assert_eq!(start.effects.effects, BTreeSet::from([effect("fetch")]));
    assert!(!start.effects.through_values);
    assert_eq!(start.signature, document.entries.values().next().cloned());
    let main = graph.unit(&Unit::Main).unwrap();
    assert_eq!(main.effects.effects, BTreeSet::from([effect("fetch")]));
    assert!(main.effects.through_values);
}

/// The label of a graph node's variant. The matches are exhaustive, so a
/// form the view gains must be named here.
fn form(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Block(_) => "block",
        NodeKind::Stmt(stmt) => match stmt {
            StmtNode::Let { .. } => "stmt:let",
            StmtNode::Assign { .. } => "stmt:assign",
            StmtNode::Remove { .. } => "stmt:remove",
            StmtNode::Do { .. } => "stmt:do",
            StmtNode::If { .. } => "stmt:if",
            StmtNode::For { .. } => "stmt:for",
            StmtNode::While { .. } => "stmt:while",
            StmtNode::Break { .. } => "stmt:break",
            StmtNode::Continue { .. } => "stmt:continue",
            StmtNode::Return { .. } => "stmt:return",
            StmtNode::Throw { .. } => "stmt:throw",
            StmtNode::Print { .. } => "stmt:print",
            StmtNode::Finish { .. } => "stmt:finish",
            StmtNode::Fail { .. } => "stmt:fail",
            StmtNode::Try { .. } => "stmt:try",
        },
        NodeKind::Action(action) => match action {
            ActionNode::Call { .. } => "action:call",
            ActionNode::Perform { .. } => "action:perform",
            ActionNode::Sleep { .. } => "action:sleep",
            ActionNode::Join { .. } => "action:join",
            ActionNode::JoinMany { .. } => "action:join_many",
            ActionNode::Yield => "action:yield",
            ActionNode::Spawn { .. } => "action:spawn",
            ActionNode::Cancel { .. } => "action:cancel",
        },
        NodeKind::Expr(expr) => match expr {
            ExprNode::Literal(_) => "expr:literal",
            ExprNode::Variable(_) => "expr:variable",
            ExprNode::Tuple(_) => "expr:tuple",
            ExprNode::List(_) => "expr:list",
            ExprNode::Map(_) => "expr:map",
            ExprNode::Set(_) => "expr:set",
            ExprNode::Record(_) => "expr:record",
            ExprNode::Member(_) => "expr:member",
            ExprNode::Closure { .. } => "expr:closure",
            ExprNode::Call { .. } => "expr:call",
            ExprNode::Clock => "expr:clock",
            ExprNode::Random => "expr:random",
            ExprNode::Read { .. } => "expr:read",
        },
    }
}

/// An independent derivation of ids and sites: a pre-order walk of the
/// document's own tree, `main` first, then the functions by name.
fn walk_document(document: &Document) -> Vec<(Site, &'static str)> {
    fn walk<'a>(node: Node<'a>, site: Site, out: &mut Vec<(Site, &'static str)>) {
        out.push((
            site.clone(),
            match node {
                Node::Block(_) => "block",
                Node::Stmt(_) => "stmt",
                Node::Action(_) => "action",
                Node::Expr(_) => "expr",
            },
        ));
        for (index, child) in (0u32..).zip(node.children()) {
            walk(child, site.child(index), out);
        }
    }
    let mut out = Vec::new();
    walk(
        Node::Block(&document.main),
        Site::new(Unit::Main, []),
        &mut out,
    );
    for (name, function) in &document.functions {
        walk(
            Node::Block(&function.body),
            Site::new(Unit::Function(name.clone()), []),
            &mut out,
        );
    }
    out
}

/// `K-DOC-001`: the view is total. Walking it from each unit's body through
/// its typed slots reaches every node of the document, each at the site and
/// of the class the document's own tree gives it, and a document that uses
/// every form shows every typed variant.
#[test]
fn walking_the_graph_view_reaches_every_node_and_slot() {
    let library = library();
    let document = document(&every_form(&library));
    let graph = derive(&document, &library.catalog).unwrap_or_else(|refusal| panic!("{refusal}"));

    let mut reached: Vec<NodeId> = Vec::new();
    let mut forms = BTreeSet::new();
    let mut pending: Vec<NodeId> = graph.units().iter().rev().map(|unit| unit.body).collect();
    while let Some(id) = pending.pop() {
        let node = graph.node(id);
        reached.push(id);
        forms.insert(form(&node.kind));
        let children = node.children();
        for (index, child) in (0u32..).zip(&children) {
            // A slot's child sits at the slot's index (`K-ID-004`).
            assert_eq!(graph.node(*child).site, node.site.child(index));
            assert_eq!(graph.node(*child).parent, Some(id));
        }
        pending.extend(children.into_iter().rev());
    }

    let expected = walk_document(&document);
    assert_eq!(reached.len(), expected.len());
    assert_eq!(reached.len(), graph.nodes().len());
    for (id, (site, class)) in reached.iter().zip(&expected) {
        let node = graph.node(*id);
        assert_eq!(&node.site, site);
        assert_eq!(form(&node.kind).split(':').next(), Some(*class), "{site}");
        // An expression and an action carry a facet; nothing else does.
        assert_eq!(
            node.facet.is_some(),
            matches!(*class, "expr" | "action"),
            "{site}"
        );
    }
    let every_form = [
        "action:call",
        "action:cancel",
        "action:join",
        "action:join_many",
        "action:perform",
        "action:sleep",
        "action:spawn",
        "action:yield",
        "block",
        "expr:call",
        "expr:clock",
        "expr:closure",
        "expr:list",
        "expr:literal",
        "expr:map",
        "expr:member",
        "expr:random",
        "expr:read",
        "expr:record",
        "expr:set",
        "expr:tuple",
        "expr:variable",
        "stmt:assign",
        "stmt:break",
        "stmt:continue",
        "stmt:do",
        "stmt:fail",
        "stmt:finish",
        "stmt:for",
        "stmt:if",
        "stmt:let",
        "stmt:print",
        "stmt:remove",
        "stmt:return",
        "stmt:throw",
        "stmt:try",
        "stmt:while",
    ];
    assert_eq!(forms.into_iter().collect::<Vec<_>>(), every_form);
}

/// `K-ADM-009`, `K-SITE-005`: ids and sites are a function of the document
/// alone. Two derivations agree, an independent walk of the document's tree
/// numbers the nodes the same way, and annotating every node changes
/// neither the identity nor anything derived.
#[test]
fn derived_ids_and_sites_are_a_function_of_the_document_alone() {
    let library = library();
    let document = document(&every_form(&library));
    let graph = derive(&document, &library.catalog).unwrap();

    // A second derivation, from the stored form.
    let stored = Document::from_json(&document.to_json().unwrap()).unwrap();
    assert_eq!(derive(&stored, &library.catalog).unwrap(), graph);

    // The independent walk gives each node the id the view gives it.
    for ((site, _), id) in walk_document(&document).iter().zip(0u32..) {
        let node = graph.node_at(site).unwrap();
        assert_eq!(node.id.index(), id as usize, "{site}");
        assert_eq!(graph.node(node.id).site, *site);
    }

    // Every derived site is an address annotations can attach to, and a
    // document annotated at every one derives the same view.
    let mut sites: Vec<Site> = graph.nodes().iter().map(|node| node.site.clone()).collect();
    sites.sort();
    let mut annotations = Annotations::new(document.identity().unwrap());
    annotations.dialect = Some("typescript".into());
    annotations.source = Some("// authored".into());
    annotations.nodes = sites
        .into_iter()
        .map(|site| NodeAnnotation {
            site,
            label: Some(Label {
                title: "a host's label".into(),
                description: None,
            }),
            data: BTreeMap::new(),
        })
        .collect();
    validate_annotations(&annotations, &document).unwrap();
    assert_eq!(annotations.document, stored.identity().unwrap());
    assert_eq!(derive(&document, &library.catalog).unwrap(), graph);
}

/// `K-SITE-001` to `K-SITE-004`: an action is named by its own site, its
/// statement by that site less the last index, and its loops by their
/// statements, a closure body bounding them.
#[test]
fn execution_sites_name_the_action_its_statement_and_its_loops() {
    let library = library();
    let text = r#"kernel 1
numbers float
effect fetch(url: Text) -> Any

main {
  for x in [[1]] {
    while true {
      let f = fn() {
        do yield
      }
      set x[0] = perform fetch("u") as Any
    }
  }
}
"#;
    let graph = derive(&document(text), &library.catalog).unwrap_or_else(|e| panic!("{e}"));
    let sites = graph.execution_sites();
    assert_eq!(sites.len(), 2);

    let wait = &sites[0];
    assert_eq!(wait.kind, SiteKind::Yield);
    assert!(wait.kind.waits());
    assert_eq!(wait.site, main_site(&[0, 1, 0, 1, 0, 0, 0, 0, 0]));
    assert_eq!(wait.statement, main_site(&[0, 1, 0, 1, 0, 0, 0, 0]));
    assert!(wait.loops.is_empty());

    let perform = &sites[1];
    assert_eq!(perform.kind, SiteKind::Perform);
    assert_eq!(perform.site, main_site(&[0, 1, 0, 1, 1, 2]));
    assert_eq!(perform.statement, main_site(&[0, 1, 0, 1, 1]));
    assert_eq!(perform.loops, [main_site(&[0]), main_site(&[0, 1, 0])]);
    assert_eq!(
        graph.loop_sites().cloned().collect::<Vec<_>>(),
        perform.loops
    );
    // Every action's statement is a place a task may stand.
    let statements: BTreeSet<&Site> = graph.statement_sites().collect();
    assert!(
        sites
            .iter()
            .all(|site| statements.contains(&site.statement))
    );
}

/// `K-CLO-001`, `K-FORM-003`: a closure shares the enclosing variables its
/// body names, an inner closure's through the outer one, and a name is
/// resolved to the latest declaration at that point.
#[test]
fn closures_capture_enclosing_variables_by_reference() {
    let library = library();
    let text = r#"kernel 1
numbers float

main {
  let a = 1
  let f = fn(b) {
    let g = fn() {
      set a = b
    }
    return g
  }
  let a = 2
  print a
}
"#;
    let graph = derive(&document(text), &library.catalog).unwrap_or_else(|e| panic!("{e}"));
    let named = |name: &str| {
        graph
            .bindings()
            .iter()
            .filter(|binding| binding.name.as_str() == name)
            .collect::<Vec<_>>()
    };
    let (first_a, second_a, b) = (named("a")[0], named("a")[1], named("b")[0]);
    assert!(first_a.captured && first_a.assigned);
    assert!(!second_a.captured && !second_a.assigned);
    assert_eq!((b.kind, b.captured), (BindingKind::Param, true));
    // An assigned variable may hold anything; a single `let` types it.
    assert_eq!((&first_a.facet, &second_a.facet), (&Type::Any, &Type::Int));

    let captures = |site: &[u32]| match &graph.node_at(&main_site(site)).unwrap().kind {
        NodeKind::Expr(ExprNode::Closure { captures, .. }) => captures.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(captures(&[1, 0]), [Reference::Binding(first_a.id)]);
    assert_eq!(
        captures(&[1, 0, 0, 0, 0]),
        [Reference::Binding(first_a.id), Reference::Binding(b.id)]
    );
    // `print a` reads the second `a`.
    assert_eq!(
        graph.node_at(&main_site(&[3, 0])).unwrap().kind,
        NodeKind::Expr(ExprNode::Variable(Reference::Binding(second_a.id)))
    );
}

/// `K-FORM-017`: a `break` out of a `try` with a `finally` leads into the
/// `finally`, and resumes from it to where the loop leads.
#[test]
fn a_departure_through_finally_resumes_after_it() {
    let library = library();
    let text = r#"kernel 1
numbers float

main {
  while true {
    try {
      break
    } finally {
      print 1
    }
  }
  return 2
}
"#;
    let graph = derive(&document(text), &library.catalog).unwrap_or_else(|e| panic!("{e}"));
    let id = |site: &[u32]| graph.node_at(&main_site(site)).unwrap().id;
    let (brk, finally, print, ret) = (
        id(&[0, 1, 0, 0, 0]),
        id(&[0, 1, 0, 1]),
        id(&[0, 1, 0, 1, 0]),
        id(&[1]),
    );
    assert_eq!(
        graph.edges_from(brk).copied().collect::<Vec<_>>(),
        [Edge {
            from: brk,
            to: Target::Node(print),
            kind: EdgeKind::Break
        }]
    );
    assert_eq!(
        graph.edges_from(finally).copied().collect::<Vec<_>>(),
        [Edge {
            from: finally,
            to: Target::Node(ret),
            kind: EdgeKind::Break
        }]
    );
    assert_eq!(
        graph.edges_from(ret).copied().collect::<Vec<_>>(),
        [Edge {
            from: ret,
            to: Target::Leave,
            kind: EdgeKind::Return
        }]
    );
}

/// `K-ADM-001`, `K-ADM-002`, `K-ADM-003`: one refusal names every effect
/// the environment lacks, every effect whose signature does not serve, and
/// every function identity it does not hold.
#[test]
fn a_refusal_names_everything_the_environment_is_missing() {
    let library = library();
    let text = format!(
        r#"kernel 1
numbers float
effect fetch(url: Text) -> Any
effect store(key: Text) -> Int
use num.neg = @{neg}
use helper.twice = @{twice}

main {{
  let page = perform fetch("u") as Any
  let n = perform store("k") as Int
  let m = num.neg(n)
  let t = invoke helper.twice(m)
}}
"#,
        neg = library.neg,
        twice = library.twice,
    );
    let empty = BTreeMap::new();
    let mut environment = Environment::new(&empty);
    // `store` is provided, but takes an integer and promises less.
    let provided = signature(&[("key", Type::Int)], Type::Any);
    environment
        .effects
        .insert(effect("store"), provided.clone());

    let mut functions = [(library.neg, "num.neg"), (library.twice, "helper.twice")];
    functions.sort();
    let mut expected = vec![
        RefusalReason::MissingEffect {
            effect: effect("fetch"),
        },
        RefusalReason::EffectSignature {
            effect: effect("store"),
            expected: signature(&[("key", Type::Text)], Type::Int),
            provided,
        },
    ];
    expected.extend(
        functions.map(|(function, name)| RefusalReason::MissingFunction {
            function,
            name: name.parse_name(),
        }),
    );
    let reasons: Vec<RefusalReason> = refusals(&text, &environment)
        .into_iter()
        .map(|refused| refused.reason)
        .collect();
    assert_eq!(reasons, expected);
}

/// `K-ADM-002`: a provided signature serves when it takes what the document
/// may send and returns only what the document expects.
#[test]
fn an_effect_signature_that_takes_more_and_returns_less_serves() {
    let library = library();
    let text = r#"kernel 1
numbers float
effect store(key: Text) -> Number

main {
  let n = perform store("k") as Int
}
"#;
    let mut environment = Environment::new(&library.catalog);
    let mut provided = signature(
        &[
            ("key", Type::Union(vec![Type::Text, Type::Bytes])),
            ("ttl", Type::Int),
        ],
        Type::Int,
    );
    provided.params[1].optional = true;
    environment.effects.insert(effect("store"), provided);
    admit(&document(text), &environment).unwrap_or_else(|refusal| panic!("{refusal}"));
}

/// `K-DOC-005`, `K-ADM-008`: a structural fault refuses, in the document
/// and in the body of a library function the document reaches, where the
/// statement rule is checked as it is in a document (`K-STMT-002`).
#[test]
fn a_structural_fault_refuses_in_the_document_and_in_a_reached_body() {
    let mut library = library();
    let environment = Environment::new(&library.catalog);
    let errors = refusals(
        "kernel 1\nnumbers float\n\nmain {\n  break\n}\n",
        &environment,
    );
    assert_eq!(
        errors,
        [Refused {
            site: Some(main_site(&[0])),
            reason: RefusalReason::Invalid(InvalidReason::LoopControlOutsideLoop {
                keyword: "break"
            }),
        }]
    );

    // A body that nests a call of a function with no native implementation.
    let nested = register(
        &mut library.catalog,
        &format!(
            "function helper.nested(x: Any) -> Any\nkernel 1\ncharge 1\n\
             use helper.twice = @{twice}\nbody {{\n  return helper.twice(x)\n}}\n",
            twice = library.twice,
        ),
    );
    let text = format!(
        "kernel 1\nnumbers float\nuse helper.nested = @{nested}\nuse helper.twice = @{twice}\n\n\
         main {{\n  let y = invoke helper.nested(1)\n}}\n",
        twice = library.twice,
    );
    let errors = refusals(&text, &Environment::new(&library.catalog));
    assert_eq!(
        errors,
        [Refused {
            site: Some(Site::new(Unit::Library(nested), [0, 0])),
            reason: RefusalReason::Invalid(InvalidReason::CallNotNative {
                function: library.twice,
                name: "helper.twice".parse_name(),
            }),
        }]
    );
}

/// `K-FORM-003`, `K-ADM-004`: a variable used before it is bound is
/// refused, naming the node: in a declared function whatever the
/// environment provides, in `main` unless the session provides it, and in a
/// library body.
#[test]
fn a_variable_used_before_it_is_bound_is_refused_naming_the_node() {
    let library = library();
    let unbound = |name: &str, site: Site| Refused {
        site: Some(site),
        reason: RefusalReason::UnboundVariable {
            name: Name::new(name),
        },
    };
    let text = r#"kernel 1
numbers float

fn f(a) {
  if a {
    let t1 = 1
  }
  return t1
}

main {
  print x
  let x = 1
  let g = call f(x)
}
"#;
    // `main`'s variables are not `f`'s, and `t1` has left its block.
    let mut environment = Environment::new(&library.catalog);
    environment
        .bindings
        .extend([Name::new("t1"), Name::new("a")]);
    assert_eq!(
        refusals(text, &environment),
        [
            unbound("t1", Site::new(Unit::Function(Name::new("f")), [1, 0])),
            unbound("x", main_site(&[0, 0])),
        ]
    );
    environment.bindings.insert(Name::new("x"));
    assert_eq!(
        refusals(text, &environment),
        [unbound(
            "t1",
            Site::new(Unit::Function(Name::new("f")), [1, 0])
        )]
    );

    let body = parse_definition(
        "function helper.leak(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return y }\n",
    )
    .unwrap();
    let site = Site::new(Unit::Library(body.identity().unwrap()), [0, 0]);
    assert_eq!(
        derive_definition(&body, &library.catalog)
            .unwrap_err()
            .errors,
        [unbound("y", site)]
    );
}

/// `K-ADM-005`: an argument that can never be of its parameter's type is
/// refused, in a library call and in a `perform`; one that might be is
/// admitted.
#[test]
fn an_argument_that_cannot_fit_its_parameter_is_refused() {
    let library = library();
    let environment = environment(&library);
    let header = format!(
        "kernel 1\nnumbers float\neffect fetch(url: Text) -> Any\nuse num.neg = @{}\n",
        library.neg
    );
    let mismatch = |callee: &str, param: &str, expected: Type, found: Type, site: &[u32]| Refused {
        site: Some(main_site(site)),
        reason: RefusalReason::ArgumentType {
            callee: callee.into(),
            param: Name::new(param),
            expected,
            found,
        },
    };
    let text = format!(
        "{header}\nmain {{\n  let n = \"t\"\n  let a = num.neg(n)\n  \
         let p = perform fetch(1) as Any\n}}\n"
    );
    assert_eq!(
        refusals(&text, &environment),
        [
            mismatch("num.neg", "x", Type::Number, Type::Text, &[1, 0, 0]),
            mismatch("fetch", "url", Type::Text, Type::Int, &[2, 0]),
        ]
    );
    // Once something assigns `n`, the document no longer says what it holds.
    let text = format!(
        "{header}\nmain {{\n  let n = \"t\"\n  set n = 1\n  let a = num.neg(n)\n  \
         let p = perform fetch(\"u\") as Any\n}}\n"
    );
    admit(&document(&text), &environment).unwrap_or_else(|refusal| panic!("{refusal}"));
}

/// `K-ADM-006`: a `perform` that states a result the effect's signature can
/// never give is refused.
#[test]
fn a_perform_result_the_signature_cannot_give_is_refused() {
    let library = library();
    let mut environment = Environment::new(&library.catalog);
    environment
        .effects
        .insert(effect("count"), signature(&[], Type::Int));
    let text = "kernel 1\nnumbers float\neffect count() -> Int\n\n\
                main {\n  let c = perform count() as Text\n}\n";
    assert_eq!(
        refusals(text, &environment),
        [Refused {
            site: Some(main_site(&[0, 0])),
            reason: RefusalReason::PerformResult {
                effect: effect("count"),
                declared: Type::Int,
                stated: Type::Text,
            },
        }]
    );
}

/// `K-ADM-007`: the manifest a document carries must be the one its code
/// derives: no effect nothing performs, no function nothing reaches, no
/// function reached through a body left out, no function under another
/// name.
#[test]
fn a_manifest_that_is_not_the_derived_one_is_refused() {
    let library = library();
    let environment = environment(&library);
    let reasons = |text: &str| -> Vec<RefusalReason> {
        refusals(text, &environment)
            .into_iter()
            .map(|refused| {
                assert_eq!(refused.site, None);
                refused.reason
            })
            .collect()
    };

    let text = format!(
        "kernel 1\nnumbers float\neffect fetch(url: Text) -> Any\nuse num.neg = @{}\n\n\
         main {{\n  return 1\n}}\n",
        library.neg
    );
    assert_eq!(
        reasons(&text),
        [
            RefusalReason::EffectNotPerformed {
                effect: effect("fetch")
            },
            RefusalReason::FunctionNotReached {
                function: library.neg,
                name: "num.neg".parse_name(),
            },
        ]
    );

    // `helper.outer`'s body calls `helper.twice`, which is not listed.
    let text = format!(
        "kernel 1\nnumbers float\nuse helper.outer = @{}\n\n\
         main {{\n  let y = invoke helper.outer(1)\n}}\n",
        library.outer
    );
    assert_eq!(
        reasons(&text),
        [RefusalReason::FunctionNotListed {
            function: library.twice,
            name: "helper.twice".parse_name(),
            through: library.outer,
        }]
    );

    let text = format!(
        "kernel 1\nnumbers float\nuse num.negate = @{}\n\n\
         main {{\n  let y = num.negate(1)\n}}\n",
        library.neg
    );
    assert_eq!(
        reasons(&text),
        [RefusalReason::FunctionName {
            function: library.neg,
            listed: "num.negate".parse_name(),
            defined: "num.neg".parse_name(),
        }]
    );
}

trait ParseName {
    fn parse_name(&self) -> lash_kernel_doc::FunctionName;
}

impl ParseName for str {
    fn parse_name(&self) -> lash_kernel_doc::FunctionName {
        lash_kernel_doc::FunctionName::new(self).unwrap()
    }
}

/// The view of a library body is a view like any other (`K-SITE-005`).
#[test]
fn a_library_body_derives_a_view_in_its_own_unit() {
    let library = library();
    let definition = &library.catalog[&library.outer];
    let graph: Graph = derive_definition(definition, &library.catalog).unwrap();
    let unit = Unit::Library(library.outer);
    assert_eq!(graph.units().len(), 1);
    assert_eq!(graph.units()[0].unit, unit);
    assert!(graph.units()[0].effects.through_values);
    let sites = graph.execution_sites();
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].kind, SiteKind::Call);
    assert_eq!(sites[0].site, Site::new(unit, [0, 0]));
}
