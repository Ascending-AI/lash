//! The workflow-graph lens's print → reparse → admit round trip over the
//! shapes FIG-3635 and FIG-3663 found open: hoisted `var` declarations
//! beside function declarations, and opaque nodes that read the session's
//! globals.

use std::collections::BTreeSet;

use lash_typescript::parse;
use lash_typescript::workflow_graph::{
    TypeScriptSourceError, parse_typescript_expression, typescript_expression_source,
    typescript_program_source, workflow_graph_from_source, workflow_graph_to_source,
    workflow_graph_to_source_in_session,
};
use lashlang::WorkflowNodeKind;

fn canonical(source: &str) -> String {
    typescript_program_source(&parse(source).expect("fixture parses"))
        .expect("a parsed fixture prints back as TypeScript")
}

#[test]
fn sparse_array_literals_preserve_elisions() {
    let cases = [
        "[, 1]",
        "[1, ,]",
        "[1, , , 4]",
        "[, ,]",
        "[,]",
        "[undefined, , 1]",
        "[[, 1], ,]",
    ];
    for source in cases {
        let globals = BTreeSet::new();
        let locals = BTreeSet::new();
        let expression =
            parse_typescript_expression(source, &globals, &locals).expect("sparse literal lowers");
        let printed = typescript_expression_source(&expression).expect("sparse literal prints");
        assert_eq!(printed, source);
        assert_eq!(
            parse_typescript_expression(&printed, &globals, &locals)
                .expect("printed literal parses"),
            expression,
            "hole positions and stored undefined values survive"
        );
        assert_lens_laws(&format!("finish({source});\n"));
    }
}

#[test]
fn malformed_sparse_array_helpers_are_typed_refusals() {
    use lashlang::Expr;
    let cases = [
        (vec![Expr::Absent], vec![Expr::Number(1.0)]),
        (vec![], vec![Expr::Number(0.0)]),
        (
            vec![Expr::Absent; 2],
            vec![Expr::Number(1.0), Expr::Number(0.0)],
        ),
        (
            vec![Expr::Absent],
            vec![Expr::Number(0.0), Expr::Number(0.0)],
        ),
        (vec![Expr::Absent], vec![Expr::Number(-1.0)]),
        (vec![Expr::Absent], vec![Expr::Number(-0.0)]),
        (vec![Expr::Absent], vec![Expr::Number(0.5)]),
        (vec![Expr::Absent], vec![Expr::Number(f64::NAN)]),
        (vec![Expr::Absent], vec![Expr::Number(f64::INFINITY)]),
        (vec![Expr::Absent], vec![Expr::String("0".into())]),
        (vec![Expr::Number(1.0)], vec![Expr::Number(0.0)]),
        (vec![Expr::Absent], vec![]),
    ];
    for (values, holes) in cases {
        let helper = Expr::BuiltinCall {
            name: "__lashlang_stdlib".into(),
            args: vec![
                Expr::String("Lash.SparseArray".into()),
                Expr::List(values),
                Expr::List(holes),
            ],
        };
        assert!(
            matches!(
                typescript_expression_source(&helper),
                Err(TypeScriptSourceError::MalformedSparseArray { .. })
            ),
            "malformed helper must refuse: {helper:?}"
        );
    }
    for operands in [
        vec![],
        vec![Expr::Absent],
        vec![Expr::List(vec![]), Expr::List(vec![]), Expr::Absent],
    ] {
        let mut args = vec![Expr::String("Lash.SparseArray".into())];
        args.extend(operands);
        assert!(matches!(
            typescript_expression_source(&Expr::BuiltinCall {
                name: "__lashlang_stdlib".into(),
                args,
            }),
            Err(TypeScriptSourceError::MalformedSparseArray { .. })
        ));
    }
}

fn assert_json_round_trip(source: &str) {
    assert_lens_laws(source);
    let original = lashlang::ModuleArtifact::from_program(parse(source).expect("source parses"))
        .expect("source admits");
    let printed = typescript_program_source(original.ir()).expect("JSON call prints");
    let readmitted = lashlang::ModuleArtifact::from_program(parse(&printed).expect("text parses"))
        .expect("text admits");
    assert_eq!(readmitted.module_ref(), original.module_ref());
    assert_eq!(readmitted.source_identity(), original.source_identity());
}

#[test]
fn plain_json_stringify_round_trips() {
    assert_json_round_trip("const value = {a: [1, 2]}; finish(JSON.stringify(value));");
    assert_json_round_trip("const value = {a: [1, 2]}; JSON.stringify(value); finish(value);");
}

#[test]
fn json_stringify_iife_round_trips() {
    assert_json_round_trip(
        "finish(JSON.stringify((function () { const a = [1, ,]; return {length: a.length, value: a}; })()));",
    );
}

#[test]
fn nested_json_stringify_round_trips() {
    assert_json_round_trip("finish(JSON.stringify({text: JSON.stringify({values: [, 1, ,]})}));");
}

#[test]
fn unmarked_json_traversal_is_not_sugared() {
    let expression =
        parse_typescript_expression("JSON.stringify({a: 1})", &BTreeSet::new(), &BTreeSet::new())
            .expect("JSON call lowers");
    let unmarked = match expression {
        lashlang::Expr::Role { expr, .. } => *expr,
        expression => expression,
    };
    assert!(typescript_expression_source(&unmarked).is_err());
}

#[test]
fn json_traversal_printing_does_not_depend_on_binding_names() {
    use lashlang::{AstString, Expr, ExprFolder, fold_expr_children};
    struct Rename;
    impl ExprFolder for Rename {
        fn fold_expr(&mut self, mut expression: Expr) -> Expr {
            let rename = |name: &mut AstString| *name = format!("renamed_{name}").into();
            match &mut expression {
                Expr::Variable(name) => rename(name),
                Expr::Assign { target, .. } => rename(&mut target.root),
                Expr::For { binding, .. } => rename(binding),
                Expr::Function(function) => {
                    for name in function
                        .name
                        .iter_mut()
                        .chain(function.receiver.iter_mut())
                        .chain(function.params.iter_mut())
                        .chain(function.captures.iter_mut())
                    {
                        rename(name);
                    }
                }
                _ => {}
            }
            fold_expr_children(self, expression)
        }
    }
    let expression =
        parse_typescript_expression("JSON.stringify({a: 1})", &BTreeSet::new(), &BTreeSet::new())
            .expect("JSON call lowers");
    let expected = typescript_expression_source(&expression).expect("original traversal prints");
    assert_eq!(
        typescript_expression_source(&Rename.fold_expr(expression))
            .expect("renamed traversal prints"),
        expected
    );
}

#[test]
fn malformed_json_traversal_roles_are_refused() {
    use lashlang::Expr;
    let mut expression =
        parse_typescript_expression("JSON.stringify(1)", &BTreeSet::new(), &BTreeSet::new())
            .expect("JSON call lowers");
    let Expr::Role { expr, .. } = &mut expression else {
        panic!("JSON role");
    };
    **expr = Expr::Absent;
    let mut program = parse("finish(1);").expect("program parses");
    program.main = expression.clone();
    assert!(matches!(
        lashlang::validate_ast(&program),
        Err(lashlang::InvalidAst::MalformedRole {
            role: "json_traversal",
            ..
        })
    ));
    assert!(typescript_expression_source(&expression).is_err());
}

#[test]
fn json_traversal_roles_preserve_execution_identities() {
    use lashlang::{Expr, ExprFolder, fold_expr_children};
    struct ExecutionBody;
    impl ExprFolder for ExecutionBody {
        fn fold_expr(&mut self, expression: Expr) -> Expr {
            match expression {
                Expr::Role { role, expr } if role.name() == "json_traversal" => {
                    self.fold_expr(*expr)
                }
                expression => fold_expr_children(self, expression),
            }
        }
    }
    for source in [
        "finish(JSON.stringify({a: 1}));",
        "const a = JSON.stringify({text: JSON.stringify([1])}); finish(a);",
    ] {
        let program = parse(source).expect("source parses");
        let mut legacy = program.clone();
        legacy.main = ExecutionBody.fold_expr(legacy.main);
        let original =
            lashlang::ModuleArtifact::from_program(legacy.clone()).expect("legacy admits");
        let marked =
            lashlang::ModuleArtifact::from_program(program.clone()).expect("marked admits");
        assert_eq!(marked.module_ref(), original.module_ref());
        assert_eq!(marked.source_identity(), original.source_identity());
        assert_eq!(
            lashlang::lifted_process_identity(&program.main, &[2, 3]),
            lashlang::lifted_process_identity(&legacy.main, &[2, 3])
        );
        let compile = lashlang::testing::harness::compile_program;
        let marked_compiled = compile(&program);
        let legacy_compiled = compile(&legacy);
        assert_eq!(
            lashlang::testing::harness::compiled_execution_sites(&marked_compiled),
            lashlang::testing::harness::compiled_execution_sites(&legacy_compiled)
        );
        let linked = lashlang::testing::harness::link_labeled(program);
        let legacy_linked = lashlang::testing::harness::link_labeled(legacy);
        assert_eq!(
            linked.artifact.module_ref(),
            legacy_linked.artifact.module_ref()
        );
    }
}

#[test]
fn edited_json_traversals_are_refused() {
    use lashlang::Expr;
    for edit in 0..5 {
        let mut expression = parse_typescript_expression(
            "JSON.stringify({a: 1})",
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .expect("JSON call lowers");
        let Expr::Role { expr, .. } = &mut expression else {
            panic!("JSON role")
        };
        let Expr::Block(prefix) = expr.as_mut() else {
            panic!("JSON prefix")
        };
        if edit == 4 {
            let Expr::If { condition, .. } = prefix.last_mut().expect("dispatch") else {
                panic!("JSON dispatch")
            };
            let Expr::CoercingBinary { left, .. } = condition.as_mut() else {
                panic!("JSON dispatch condition")
            };
            let Expr::BuiltinCall { args, .. } = left.as_mut() else {
                panic!("container classification")
            };
            args[1] = Expr::Variable("binding_0".into());
            assert!(
                typescript_expression_source(&expression).is_err(),
                "an unbound name must not alias a normalized binding"
            );
            continue;
        }
        let Expr::If { else_block, .. } = prefix.last_mut().expect("dispatch") else {
            panic!("JSON dispatch")
        };
        let Expr::Block(traversal) = else_block.as_mut() else {
            panic!("JSON traversal")
        };
        match edit {
            0 => {
                let Expr::If { then_block, .. } = &mut traversal[0] else {
                    panic!("cycle guard")
                };
                **then_block = Expr::Print(Box::new(Expr::String("edited".into())));
            }
            1 => {
                let Expr::Assign { expr, .. } = &mut traversal[3] else {
                    panic!("transform binding")
                };
                let Expr::Function(transformer) = expr.as_mut() else {
                    panic!("transform function")
                };
                *transformer.body = Expr::Absent;
            }
            2 => {
                let Expr::If { condition, .. } = &mut traversal[6] else {
                    panic!("final cycle guard")
                };
                **condition = Expr::Bool(false);
            }
            _ => {
                let Expr::If { condition, .. } = &mut traversal[6] else {
                    panic!("final cycle guard")
                };
                let Expr::Index { index, .. } = condition.as_mut() else {
                    panic!("cycle index")
                };
                **index = Expr::Number(-0.0);
            }
        }
        assert!(
            typescript_expression_source(&expression).is_err(),
            "edited traversal {edit} must survive or refuse"
        );
    }
}

#[test]
fn contextual_binding_names_round_trip() {
    for name in [
        "of",
        "as",
        "from",
        "type",
        "async",
        "get",
        "set",
        "readonly",
        "number",
        "constructor",
    ] {
        assert_lens_laws(&format!(
            "const {name} = [1]; for (const value of {name}) {{ console.log({name}, value); }} finish({name});"
        ));
    }
}

#[test]
fn property_presence_queries_round_trip() {
    for source in [
        "finish([0 in [, 1], 1 in [, 1]]);",
        "finish([, 1].hasOwnProperty('0'));",
        "const a = [, 1]; finish([a.hasOwnProperty('0'), a.hasOwnProperty('1')]);",
        "const a = {hasOwnProperty: (key) => key === 'x'}; finish(a.hasOwnProperty('x'));",
        "finish(({x: 1}).hasOwnProperty('x'));",
        "const a = {hasOwnProperty: (key) => key === 'x'}; a.hasOwnProperty('x'); finish(a);",
    ] {
        assert_json_round_trip(source);
    }
    let mut edited = parse_typescript_expression(
        "({x: 1}).hasOwnProperty(0)",
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
    .expect("own-property guard lowers");
    let lashlang::Expr::Block(items) = &mut edited else {
        panic!("own-property receiver")
    };
    let lashlang::Expr::If { else_block, .. } = &mut items[1] else {
        panic!("own-property guard")
    };
    let lashlang::Expr::BuiltinCall { args, .. } = else_block.as_mut() else {
        panic!("own-property fallback")
    };
    args[2] = lashlang::Expr::Number(-0.0);
    assert!(
        typescript_expression_source(&edited).is_err(),
        "guard keys with distinct artifact identities must refuse"
    );
}

/// The language-neutral IR projection, with TypeScript opaque-statement text.
fn workflow_graph_from_program(program: &lashlang::Program) -> lashlang::WorkflowGraph {
    lashlang::workflow_graph_from_program(
        program,
        &lash_typescript::workflow_graph::TypeScriptStatementText,
    )
}

/// Every lens law over one fixture.
fn assert_lens_laws(source: &str) {
    let canonical = canonical(source);
    let graph = workflow_graph_from_source(&canonical).expect("canonical source projects");
    let rendered = workflow_graph_to_source(&graph).expect("graph renders");
    assert_eq!(rendered, canonical, "GetPut");
    assert_eq!(
        parse(&rendered).expect("rendered source parses"),
        parse(&canonical).expect("canonical source parses"),
    );
    assert_eq!(
        workflow_graph_from_source(&rendered).expect("rendered source reprojects"),
        graph,
        "PutGet",
    );
}

#[test]
fn hoisted_vars_round_trip_through_global_this() {
    // The same hoisting, with the function reading the `var` through the
    // session global `globalThis` addresses it as.
    assert_lens_laws("var x = 1;\nfunction f() {\n  return globalThis.x;\n}\nfinish(f());\n");
}

#[test]
fn template_literals_round_trip_their_cooked_escapes() {
    // FIG-3720: the lens stores the cooked text and the printer re-escapes it
    // — a newline prints `\n`, a literal `${` prints `\${` — so the printed
    // template reparses to the same cooked value.
    assert_lens_laws("finish(`a\\nb\\t${1}x\\u{1F600}\\${y}`);\n");
    // A literal newline cooks to LF and prints back escaped; the reparse
    // produces the same cooked text again.
    assert_lens_laws("finish(`a\nb`);\n");
}

#[test]
fn opaque_statements_read_session_globals() {
    // FIG-3663: an opaque statement may read a session global the program
    // itself linked against — the corpus's Test262 cells throw
    // `Test262Error`, which the session bound before the cell ran.
    let globals: BTreeSet<String> = ["Test262Error".to_string()].into_iter().collect();
    let program = lash_typescript::parse_with_globals("throw Test262Error(\"no\");\n", &globals)
        .expect("source parses against the session");
    let graph = workflow_graph_from_program(&program);
    let rendered = workflow_graph_to_source_in_session(&graph, &globals)
        .expect("the graph renders against the session's globals");
    assert_eq!(rendered, "throw Test262Error(\"no\");\n");
    assert!(
        workflow_graph_to_source(&graph).is_err(),
        "without the session's globals the opaque source still refuses"
    );
}

#[test]
fn opaque_statements_reject_globals_the_program_cannot_see() {
    // The session's globals add the names the cell linked against, no more:
    // an opaque statement that reads anything else still fails its reparse.
    let globals: BTreeSet<String> = ["Test262Error".to_string()].into_iter().collect();
    let program = lash_typescript::parse_with_globals("throw Test262Error(\"no\");\n", &globals)
        .expect("source parses against the session");
    let mut graph = workflow_graph_from_program(&program);
    let source = graph
        .main
        .nodes
        .iter_mut()
        .find_map(|node| match &mut node.kind {
            WorkflowNodeKind::Opaque { source } => Some(source),
            _ => None,
        })
        .expect("the program projects an opaque node");
    *source = "throw NotBoundHere(\"no\");".to_string();
    let error = workflow_graph_to_source_in_session(&graph, &globals)
        .expect_err("a name in neither the node nor the session refuses");
    assert_eq!(error.code(), "invalid_opaque_source");
}

#[test]
fn host_descriptor_constructors_spell_registered_paths() {
    use lashlang::Expr;
    // A host descriptor constructor keeps its module path as its type name,
    // so `host.Timer(..)` spells it and re-links the same constructor.
    let constructor = Expr::HostDescriptorConstructor {
        type_name: "host.Timer".into(),
        input: Box::new(Expr::Record(vec![(
            "expr".into(),
            Expr::String("*".into()),
        )])),
    };
    assert_eq!(
        typescript_expression_source(&constructor).expect("path-named constructor prints"),
        "host.Timer({ expr: \"*\" })"
    );
    // A constructor whose type name is not a module path keeps its typed
    // refusal: the link resolved the path and the IR does not keep it.
    for type_name in ["Schedule", "not a path", "__lashlang_0_x.y"] {
        assert!(
            matches!(
                typescript_expression_source(&Expr::HostDescriptorConstructor {
                    type_name: type_name.into(),
                    input: Box::new(Expr::Null),
                }),
                Err(TypeScriptSourceError::UnknownHostDescriptorConstructor { .. })
            ),
            "{type_name} has no constructor path to spell"
        );
    }
}

#[test]
fn this_call_prints_as_function_call_builtin() {
    use lashlang::Expr;
    // A builtin's explicit receiver spells `f["call"](t, ..)` — evaluation-
    // equal, re-lowering to a computed method call rather than this node.
    let call = Expr::ThisCall {
        this: Box::new(Expr::Variable("t".into())),
        function: Box::new(Expr::Variable("f".into())),
        args: vec![Expr::Number(1.0)],
    };
    assert_eq!(
        typescript_expression_source(&call).expect("ThisCall prints"),
        "f[\"call\"](t, 1)"
    );
    assert!(
        parse_typescript_expression(
            "f[\"call\"](t, 1)",
            &BTreeSet::new(),
            &["f".to_string(), "t".to_string()].into_iter().collect()
        )
        .is_ok(),
        "the spelling re-parses"
    );
}

#[test]
fn collection_transform_operands_round_trip() {
    // FIG-4846: a transform whose call carries arguments past the callback
    // prints them back — `reduce`'s initial value, a predicate's `thisArg`,
    // `Array.from`'s mapper arguments, evaluated excess arguments.
    for source in [
        "const xs = [1, 2]; const t = {a: 1}; finish(xs.map(function (v) { return v + this.a; }, t));",
        "const xs = [3, 1]; finish(xs.reduce((a, v) => a + v, 0));",
        "const xs = [3, 1]; finish(xs.toSorted((a, b) => a - b));",
        "const xs = [1, 2]; finish(Array.from(xs, (v) => v * 2));",
        "const xs = [1, 2]; const t = {a: 1}; finish(Array.from(xs, function (v) { return v + this.a; }, t));",
        "const xs = [1, 2]; finish(xs.every((v) => v > 0, undefined));",
        "const o = {map: (f) => 1}; finish(o.map((v) => v, {a: 1}));",
    ] {
        assert_json_round_trip(source);
    }
}

#[test]
fn bare_map_intrinsic_prints_as_map_call() {
    use lashlang::{Expr, FunctionExpr};
    // A `Map` outside its owning shape — AST-only, never lowered from
    // source — prints the evaluation-equal `.map` spelling.
    let map = Expr::Map {
        items: Box::new(Expr::List(vec![Expr::Number(1.0)])),
        function: Box::new(Expr::Function(Box::new(FunctionExpr {
            name: None,
            js_name: None,
            receiver: None,
            params: vec!["v".into()],
            captures: Vec::new(),
            body: Box::new(Expr::Block(vec![Expr::FunctionReturn(Box::new(
                Expr::Variable("v".into()),
            ))])),
        }))),
    };
    assert_eq!(
        typescript_expression_source(&map).expect("bare map prints"),
        "[1].map((v) => (v))"
    );
}

#[test]
fn destructured_parameters_round_trip() {
    // FIG-4846: a destructured, defaulted or rest parameter lowers to a
    // generated slot plus a prologue that binds the authored names; the
    // signature prints back from the prologue's shape.
    for source in [
        "finish([1, 2].map(([k, v]) => k));",
        "const f = ([a, b]) => a + b; finish(f([1, 2]));",
        "const f = ([a, b = 4]) => a + b; finish(f([1]));",
        "const f = ([a, ...rest]) => rest.length; finish(f([1, 2, 3]));",
        "const f = ({x, y}) => x; finish(f({x: 1, y: 2}));",
        "const f = ({x: named}) => named; finish(f({x: 1}));",
        "const f = ([a], {x}) => a + x; finish(f([1], {x: 2}));",
        "const f = ([a, [b, c]]) => b; finish(f([1, [2, 3]]));",
        "const f = ({x: {y}}) => y; finish(f({x: {y: 3}}));",
        "const f = ({x = 2}) => x; finish(f({}));",
        "const f = ({x, ...rest}) => rest; finish(f({x: 1, y: 2}));",
        "const f = (a = 1) => a; finish(f());",
        "const f = (a, b = a + 1, ...r) => r.length; finish(f(1, undefined, 3, 4));",
        "function f(a, b = 0) { return a + b; } finish(f(1));",
        "const f = ({x: y = 3} = {}) => y; finish(f());",
        "finish(((a, b = 1) => a + b).length);",
    ] {
        assert_json_round_trip(source);
    }
}

#[test]
fn arguments_reading_functions_round_trip() {
    // A function that mentions `arguments` binds the object in a prologue
    // ahead of its parameters — consumed into the signature — and reads it
    // through a generated slot that displays as `arguments`; a mention the
    // shallow scan misses lowers to the argv read inline and prints back as
    // the identifier.
    for source in [
        "const f = function () { return arguments; }; finish(f(1, 2));",
        "const f = function () { return arguments.length; }; finish(f(1, 2));",
        "const f = function (x) { return arguments[x]; }; finish(f(1, 2));",
        "const f = function () { const g = () => arguments; return g(); }; finish(f(1, 2));",
    ] {
        assert_json_round_trip(source);
    }
}

#[test]
fn promise_all_settled_round_trips() {
    // `await Promise.allSettled(items)` lowers to the aggregate await plus a
    // generated mapper folding each outcome into a settled record; the pair
    // prints back as the authored call.
    assert_json_round_trip("const v = await Promise.allSettled([tools.lookup({key: \"x\"})]);");
    assert_json_round_trip("finish(await Promise.allSettled([tools.lookup({key: \"x\"})]));");
}

#[test]
fn classic_for_prints_back_in_every_head_condition_and_update_form() {
    // FIG-3706: a classic `for` lowers to its head's statements and a
    // `while` whose body ends with the update; the lens prints that shape
    // back as the loop, and a `continue` (the update, then the jump) as the
    // bare `continue`.
    let cases = [
        "for (let i = 0, j = 10; (i < j); i++) {\n  j = (j - 1);\n  if ((i === 2)) {\n    continue;\n  }\n}\nfinish(1);\n",
        "let k = 0;\nfor (k = 3; (k > 0); k--) {}\nfinish(k);\n",
        "var v;\nfor (var v = 7;;) {\n  break;\n}\nfinish(v);\n",
        "for (;;) {\n  break;\n}\nfinish(1);\n",
        "for (let i = 0; (i < 9); i = (i * 2)) {}\nfinish(1);\n",
        "for (let i = 0, f = () => (i); (i < 3); i++) {\n  for (let n = 0;; n++) {\n    if ((n > i)) {\n      break;\n    }\n    continue;\n  }\n}\nfinish(1);\n",
    ];
    for source in cases {
        assert_eq!(canonical(source), source, "the loop prints back as itself");
        assert_lens_laws(source);
    }
}
