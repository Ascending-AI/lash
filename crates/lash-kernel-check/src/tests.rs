//! Laws of the checker. Each pins a named rule of
//! `docs/kernel/semantics.md`.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Document, EffectName, FunctionDefinition, FunctionId, InvalidReason, Name, Node, Param,
    Signature, Site, SourceLanguage, SourceMap, SourceSite, Span, Type, Unit, parse_definition,
    parse_document,
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
    let function = definition.identity();
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
      return a via num.neg
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
  let cancelled = cancel h
  let yielded = yield
  let slept = sleep 0
  let twice = invoke helper.outer(first)
  let applied = apply f(&start)
  let again = call start(2)
  let awaited = await first via helper.twice
  if roll {{
    fail "no"
  }}
  print if(true, let(scoped = got, scoped), null)
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
    assert_eq!(admitted.identity, document.identity());

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
    assert_eq!(
        start.signature,
        document
            .entries
            .values()
            .next()
            .map(|entry| entry.signature.clone())
    );
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
            StmtNode::ReturnVia { .. } => "stmt:return-via",
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
            ActionNode::Await { .. } => "action:await",
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
            ExprNode::If { .. } => "expr:if",
            ExprNode::Let { .. } => "expr:let",
            ExprNode::Read { .. } => "expr:read",
            ExprNode::Act { .. } => "expr:act",
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
        "action:await",
        "action:call",
        "action:cancel",
        "action:join",
        "action:join_many",
        "action:perform",
        "action:sleep",
        "action:spawn",
        "action:yield",
        "block",
        "expr:act",
        "expr:call",
        "expr:clock",
        "expr:closure",
        "expr:if",
        "expr:let",
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
        "stmt:return-via",
        "stmt:throw",
        "stmt:try",
        "stmt:while",
    ];
    assert_eq!(forms.into_iter().collect::<Vec<_>>(), every_form);
}

/// `K-ADM-009`, `K-SITE-005`: ids and sites are a function of the document
/// alone. Two derivations agree, an independent walk of the document's tree
/// numbers the nodes the same way, and a source map of every node changes
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

    // Every derived site is an address a source map can attach to, and a
    // document mapped at every one derives the same view.
    let source = "// authored";
    let sites = graph
        .nodes()
        .iter()
        .map(|node| SourceSite {
            site: node.site.clone(),
            ranges: vec![Span::new(0, source.len()).unwrap()],
            implicit: None,
        })
        .collect();
    let map = SourceMap::new(&document, source, SourceLanguage::TypeScript, sites).unwrap();
    assert_eq!(map.document, stored.identity());
    assert_eq!(derive(&document, &library.catalog).unwrap(), graph);
}

/// `K-SITE-001` to `K-SITE-004`: an action is named by its own site, its
/// statement by that site less the last index, and its loops by their
/// statements, a closure body bounding them; a `while` encloses an action
/// in its own condition.
#[test]
fn execution_sites_name_the_action_its_statement_and_its_loops() {
    let library = library();
    let text = r#"kernel 1
numbers float
effect fetch(url: Text) -> Any

main {
  for x in [[1]] {
    while [perform fetch("c") as Any, true][1] {
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
    assert_eq!(sites.len(), 3);

    let condition = &sites[0];
    assert_eq!(condition.kind, SiteKind::Perform);
    assert_eq!(condition.site, main_site(&[0, 1, 0, 0, 0, 0]));
    assert_eq!(condition.statement, main_site(&[0, 1, 0]));
    assert_eq!(condition.loops, [main_site(&[0]), main_site(&[0, 1, 0])]);

    let wait = &sites[1];
    assert_eq!(wait.kind, SiteKind::Yield);
    assert!(wait.kind.waits());
    assert_eq!(wait.site, main_site(&[0, 1, 0, 1, 0, 0, 0, 0, 0]));
    assert_eq!(wait.statement, main_site(&[0, 1, 0, 1, 0, 0, 0, 0]));
    assert!(wait.loops.is_empty());

    let perform = &sites[2];
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
            mismatch: Box::new(crate::SignatureMismatch {
                side: crate::SignatureSide::Input,
                path: "/input/properties/key".into(),
                expected: Type::Text,
                provided: Type::Int,
            }),
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
    assert_eq!(
        reasons.iter().map(RefusalReason::code).collect::<Vec<_>>(),
        [
            "missing_effect",
            "effect_signature",
            "missing_function",
            "missing_function"
        ],
    );
}

/// FIG-6340: an open output offered for a closed output names the side
/// and the differing projected types, rather than just the effect.
#[test]
fn an_effect_signature_refusal_names_the_output_openness_difference() {
    let input = serde_json::json!({});
    let closed = serde_json::json!({
        "type": "object",
        "properties": {"message": {"type": "string"}},
        "additionalProperties": false
    });
    let mut open = closed.clone();
    open.as_object_mut().unwrap().remove("additionalProperties");
    let expected = crate::tool_signature(&input, true, &closed);
    let provided = crate::tool_signature(&input, true, &open);
    let mut document = document(
        "kernel 1\nnumbers float\neffect chat.message_to_self(input?: Any) -> Any\n\nmain {\n  let sent = perform chat.message_to_self() as Any\n}\n",
    );
    document.manifest.effects.insert(
        effect("chat.message_to_self"),
        lash_kernel_doc::OperationRequirement::of_signature(expected),
    );
    let catalog = BTreeMap::new();
    let mut environment = Environment::new(&catalog);
    environment
        .effects
        .insert(effect("chat.message_to_self"), provided);
    let refusal =
        admit(&document, &environment).expect_err("an open output cannot serve the closed output");
    let reason = refusal
        .errors
        .iter()
        .find(|error| error.reason.code() == "effect_signature")
        .expect("signature refusal");
    let message = reason.to_string();
    assert!(message.contains("output"), "{message}");
    assert!(message.contains("/output"), "{message}");
    assert!(
        message.contains("declared fields") && message.contains("JSON data"),
        "{message}"
    );
    let RefusalReason::EffectSignature {
        mismatch,
        expected,
        provided,
        ..
    } = &reason.reason
    else {
        unreachable!()
    };
    assert_eq!(mismatch.side, crate::SignatureSide::Output);
    assert_eq!(mismatch.path, "/output");
    assert_eq!(mismatch.expected, expected.result);
    assert_eq!(mismatch.provided, provided.result);
    // A native closed record retains its closedness in the signature.
    let closed = Signature {
        result: crate::type_of_schema(&closed),
        ..expected.clone()
    };
    let mismatch = crate::signature_mismatch(provided, &closed).expect("closed vs unconstrained");
    assert!(mismatch.to_string().contains("closed object"), "{mismatch}");
    // When both projections retain record structure, the rest itself is
    // the first incompatible part.
    let mismatch = crate::signature_mismatch(expected, &closed).expect("open rest vs closed");
    assert_eq!(mismatch.path, "/output/additionalProperties");
    assert_eq!(mismatch.expected, closed.result);
    assert_eq!(mismatch.provided, expected.result);
    let message = mismatch.to_string();
    assert!(
        message.contains("closed object") && message.contains("open rest"),
        "{message}"
    );
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
/// and in the body of a library function the document reaches, where a
/// call by name is checked as it is in a document (`K-LIB-002`).
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
                keyword: "break".into()
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
    let site = Site::new(Unit::Library(body.identity()), [0, 0]);
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

/// A record variable's fields may change after its `let`,
/// so only its kind is known (`K-ADM-005`).
#[test]
fn a_record_field_written_after_its_let_is_not_typed_by_it() {
    let library = library();
    let environment = environment(&library);
    let text = format!(
        "kernel 1\nnumbers float\nuse num.neg = @{}\n\nmain {{\n  let r = {{a: \"t\"}}\n  \
         set r.a = 1\n  let x = r.a\n  let n = num.neg(x)\n}}\n",
        library.neg
    );
    admit(&document(&text), &environment).unwrap_or_else(|refusal| panic!("{refusal}"));
}

/// An effect whose result may lack a field does not serve
/// one whose result requires it (`K-ADM-002`).
#[test]
fn an_optional_result_field_does_not_serve_a_required_one() {
    let library = library();
    let mut environment = Environment::new(&library.catalog);
    let wanted = "kernel 1\nnumbers float\neffect get() -> Record{x: Int}\nmain { let r = perform get() as Record{x: Int} }\n";
    let offered = "kernel 1\nnumbers float\neffect get() -> Record{x?: Int}\nmain { let r = perform get() as Record{x?: Int} }\n";
    // The empty record is allowed by the provided result, but cannot
    // satisfy the required x in the expected result.
    let provided = document(offered).manifest.effects[&effect("get")]
        .signature
        .clone();
    let expected = document(wanted).manifest.effects[&effect("get")]
        .signature
        .clone();
    environment.effects.insert(effect("get"), provided.clone());
    assert_eq!(
        refusals(wanted, &environment),
        [Refused {
            site: None,
            reason: RefusalReason::EffectSignature {
                effect: effect("get"),
                expected,
                provided,
                mismatch: Box::new(crate::SignatureMismatch {
                    side: crate::SignatureSide::Output,
                    path: "/output/properties/x".into(),
                    expected: Type::Int,
                    provided: Type::Union(vec![Type::Int, Type::Absent]),
                }),
            },
        }]
    );
}

/// Records that agree once an optional field is absent
/// share a value (`K-ADM-006`).
#[test]
fn records_that_differ_only_in_optional_fields_share_a_value() {
    // The empty record inhabits both types: neither requires x, even
    // though an x that is present cannot be both Int and Text.
    let library = library();
    let mut environment = Environment::new(&library.catalog);
    let text = "kernel 1\nnumbers float\neffect count() -> Record{x?: Int}\nmain { let c = perform count() as Record{x?: Text} }\n";
    let doc = document(text);
    environment.effects.insert(
        effect("count"),
        doc.manifest.effects[&effect("count")].signature.clone(),
    );
    admit(&doc, &environment).unwrap_or_else(|refusal| panic!("{refusal}"));
}

/// FIG-6255: admission checks document units, never revalidates or identifies registered code.
#[test]
fn admission_reuses_registered_library_validation_identity_and_graphs() {
    let library = library();
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    for function in [library.neg, library.twice, library.outer] {
        registry
            .register(library.catalog[&function].clone(), None)
            .unwrap();
    }
    let document = document(&format!(
        "kernel 1\nuse helper.outer = @{}\nuse helper.twice = @{}\nnumbers float\nmain {{ let x = invoke helper.outer(1) }}\n",
        library.outer, library.twice,
    ));
    crate::admit::LIBRARY_WORK.with(|work| work.set((0, 0, 0)));
    let admitted = admit(&document, &Environment::new(&registry)).unwrap();
    assert_eq!(
        crate::admit::LIBRARY_WORK.with(std::cell::Cell::get),
        (0, 0, 2)
    );
    crate::admit::LIBRARY_WORK.with(|work| work.set((0, 0, 0)));
    assert_eq!(
        crate::derive_view(&document, &registry).unwrap(),
        admitted.graph
    );
    derive_definition(
        registry.get(&library.outer).unwrap().definition().as_ref(),
        &registry,
    )
    .unwrap();
    assert_eq!(
        crate::admit::LIBRARY_WORK.with(std::cell::Cell::get),
        (0, 0, 0)
    );
    assert_eq!(
        admit(&document, &Environment::new(&library.catalog)).unwrap(),
        admitted
    );
}

/// K-DOC-002 (FIG-6865): a registered body's callees are every library
/// reference its code holds, so a registry derives the requirements its
/// definitions alone do: a statement `await` and a closure's start adapter
/// included.
#[test]
fn registered_callees_follow_every_library_reference_slot() {
    let library = library();
    let slots = parse_definition(&format!(
        "function helper.slots(x: Any) -> Any\nkernel 1\ncharge 1\n\
         use helper.twice = @{twice}\nuse helper.outer = @{outer}\nbody {{\n\
         let y = await x via helper.twice\n\
         let f = fn(a) start (a: Any) -> Any bare ts_direct(helper.outer) {{ return a }}\n\
         return f\n}}\n",
        twice = library.twice,
        outer = library.outer,
    ))
    .unwrap();
    let mut catalog = library.catalog.clone();
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    for function in [library.neg, library.twice, library.outer] {
        registry.register(catalog[&function].clone(), None).unwrap();
    }
    let function = registry.register(slots.clone(), None).unwrap();
    catalog.insert(function, slots);
    let document = document(&format!(
        "kernel 1\nnumbers float\nuse helper.slots = @{function}\nmain {{ let x = invoke helper.slots(1) }}\n",
    ));
    let declared = requirements(&document, &catalog).functions;
    assert_eq!(
        declared.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([function, library.twice, library.outer])
    );
    assert_eq!(requirements(&document, &registry).functions, declared);
}

/// FIG-6255, K-FORM-003: cached library faults keep their sites and order,
/// including in a registry clone and in the structural view.
#[test]
fn cached_library_refusals_keep_the_uncached_errors_and_view() {
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    let mut catalog = BTreeMap::new();
    let bad = parse_definition(
        "function helper.leak(x: Any) -> Any\nkernel 1\ncharge 1\nbody { print z\nreturn y }\n",
    )
    .unwrap();
    let function = registry.register(bad.clone(), None).unwrap();
    catalog.insert(function, bad);
    let document = document(&format!(
        "kernel 1\nnumbers float\nuse helper.leak = @{function}\nmain {{ let x = invoke helper.leak(1) }}\n",
    ));
    let expected = admit(&document, &Environment::new(&catalog)).unwrap_err();
    assert_eq!(expected.errors.len(), 2);
    assert_eq!(
        admit(&document, &Environment::new(&registry)).unwrap_err(),
        expected
    );
    let cloned = registry.clone();
    crate::admit::LIBRARY_WORK.with(|work| work.set((0, 0, 0)));
    assert_eq!(
        admit(&document, &Environment::new(&cloned)).unwrap_err(),
        expected
    );
    assert_eq!(
        crate::derive_view(&document, &cloned).unwrap(),
        crate::derive_view(&document, &catalog).unwrap()
    );
    assert_eq!(
        derive_definition(
            cloned.get(&function).unwrap().definition().as_ref(),
            &cloned
        )
        .unwrap_err(),
        expected
    );
    // The uncached structural view above is the only validation/identity/graph work.
    assert_eq!(
        crate::admit::LIBRARY_WORK.with(std::cell::Cell::get),
        (1, 1, 1)
    );
}

/// FIG-6340: localisation follows the failing variance comparison, skipping
/// harmless name and type differences, and escapes field names as pointers.
#[test]
fn an_effect_signature_mismatch_locates_the_first_incompatible_nested_type() {
    let expected = document("kernel 1\nnumbers float\neffect check(input: Record{\"a/b~c\": List(Int)}) -> Int\nmain { finish null }\n").manifest.effects[&effect("check")].signature.clone();
    let provided = document("kernel 1\nnumbers float\neffect check(input: Record{\"a/b~c\": List(Text)}) -> Text\nmain { finish null }\n").manifest.effects[&effect("check")].signature.clone();
    let mismatch = crate::signature_mismatch(&provided, &expected).expect("input mismatch first");
    assert_eq!(
        mismatch,
        crate::SignatureMismatch {
            side: crate::SignatureSide::Input,
            path: "/input/properties/a~1b~0c/items".into(),
            expected: Type::Int,
            provided: Type::Text
        }
    );
    let mut provided = provided;
    provided.params = expected.params.clone();
    provided.params[0].name = Name::new("renamed");
    let mismatch =
        crate::signature_mismatch(&provided, &expected).expect("only result is incompatible");
    assert_eq!(
        mismatch,
        crate::SignatureMismatch {
            side: crate::SignatureSide::Output,
            path: "/output".into(),
            expected: Type::Int,
            provided: Type::Text
        }
    );
}

/// K-EVAL-001/002 and K-FORM-003: both branches contribute facets;
/// the expression binding shadows only in the body, including closures.
#[test]
fn conditional_facets_join_and_expression_let_scopes_do_not_escape() {
    let library = library();
    let doc = document(
        "kernel 1\nnumbers float\nmain { let x = 1 let chosen = let(x = x, if(true, x, \"text\")) print x }",
    );
    let graph = derive(&doc, &library.catalog).unwrap();
    let chosen = graph
        .nodes()
        .iter()
        .find(|node| matches!(node.kind, NodeKind::Expr(ExprNode::If { .. })))
        .unwrap();
    assert_eq!(chosen.facet, Some(Type::Union(vec![Type::Int, Type::Text])));
    let refused = refusals(
        "kernel 1\nnumbers float\nfn f() { return (let(x = 1, x), x) }\nmain { finish null }",
        &environment(&library),
    );
    assert!(refused.iter().any(|error| matches!(&error.reason, RefusalReason::UnboundVariable { name } if name.as_str() == "x")));
}

/// What reaches `web.fetch` through two bodies: `probe.fetch` performs it
/// under contract "3", and `probe.outer` calls `probe.fetch`. Answers the
/// catalog, both identities and the requirement the inner body lists.
fn reached_perform() -> (
    BTreeMap<FunctionId, FunctionDefinition>,
    FunctionId,
    FunctionId,
    lash_kernel_doc::OperationRequirement,
) {
    let mut catalog = BTreeMap::new();
    let fetch = register(
        &mut catalog,
        "function probe.fetch(x: Any) -> Any\nkernel 1\ncharge 1\n\
         effect web.fetch(value?: Any) -> Any contract \"3\"\n\
         body { let y = perform web.fetch(x) as Any return y }\n",
    );
    let outer = register(
        &mut catalog,
        &format!(
            "function probe.outer(x: Any) -> Any\nkernel 1\ncharge 1\n\
             use probe.fetch = @{fetch}\nbody {{ let y = invoke probe.fetch(x) return y }}\n"
        ),
    );
    let requirement = catalog[&fetch].body().unwrap().effects[&effect("web.fetch")].clone();
    (catalog, fetch, outer, requirement)
}

/// `K-DOC-002`: the manifest lists every requirement reached through
/// bodies, and a document that lists it is admitted.
#[test]
fn the_manifest_lists_every_requirement_reached_through_bodies() {
    let (catalog, fetch, outer, requirement) = reached_perform();
    let text = format!(
        "kernel 1\nnumbers float\neffect web.fetch(value?: Any) -> Any contract \"3\"\n\
         use probe.outer = @{outer}\nuse probe.fetch = @{fetch}\n\
         main {{ let x = invoke probe.outer(1) finish x }}\n"
    );
    let derived = requirements(&document(&text), &catalog);
    assert!(derived.effects.is_empty());
    assert_eq!(
        derived.performs,
        BTreeMap::from([(
            effect("web.fetch"),
            BTreeMap::from([(fetch, requirement.clone())])
        )])
    );
    let mut environment = Environment::new(&catalog);
    environment
        .effects
        .insert(effect("web.fetch"), requirement.signature);
    admit(&document(&text), &environment).unwrap_or_else(|refusal| panic!("{refusal}"));
}

/// `K-ADM-007`: a manifest omitting a requirement a reached body lists is
/// refused, naming the body.
#[test]
fn a_manifest_omitting_a_reached_requirement_is_refused() {
    let (catalog, fetch, outer, requirement) = reached_perform();
    let text = format!(
        "kernel 1\nnumbers float\nuse probe.outer = @{outer}\nuse probe.fetch = @{fetch}\n\
         main {{ let x = invoke probe.outer(1) finish x }}\n"
    );
    assert_eq!(
        refusals(&text, &Environment::new(&catalog)),
        [Refused {
            site: None,
            reason: RefusalReason::RequirementNotListed {
                effect: effect("web.fetch"),
                through: fetch,
                requirement,
            },
        }]
    );
}

/// `ADM-REQUIREMENT`: a document whose code and reached bodies require one
/// effect name under two requirements is refused, naming both.
#[test]
fn one_effect_under_two_requirements_is_refused_naming_both() {
    let (catalog, fetch, outer, requirement) = reached_perform();
    // The document's own perform requires the signature's interim contract.
    let text = format!(
        "kernel 1\nnumbers float\neffect web.fetch(value?: Any) -> Any\n\
         use probe.outer = @{outer}\nuse probe.fetch = @{fetch}\n\
         main {{ let own = perform web.fetch(1) as Any let x = invoke probe.outer(own) finish x }}\n"
    );
    let own = document(&text).manifest.effects[&effect("web.fetch")].clone();
    assert_ne!(own, requirement);
    let mut environment = Environment::new(&catalog);
    environment
        .effects
        .insert(effect("web.fetch"), requirement.signature.clone());
    assert_eq!(
        refusals(&text, &environment),
        [Refused {
            site: None,
            reason: RefusalReason::RequirementConflict {
                effect: effect("web.fetch"),
                first: Box::new(crate::RequiredBy {
                    through: None,
                    requirement: own,
                }),
                second: Box::new(crate::RequiredBy {
                    through: Some(fetch),
                    requirement,
                }),
            },
        }]
    );
}
