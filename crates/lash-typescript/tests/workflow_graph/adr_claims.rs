use super::*;

#[test]
fn editable_ir_fields_survive_every_lens_direction() {
    let cases = [
        (
            "function f() { return 1; } const x = f();",
            "function f() { return 1; } const y = f();",
            1,
            "binding",
        ),
        (
            "const x = await tools.echo('a');",
            "const x = tools.echo('a');",
            0,
            "result_steps",
        ),
        (
            "const x = await sleep(1);",
            "const y = await sleep(1);",
            0,
            "binding",
        ),
        (
            "const x = true ? 1 : 2;",
            "const y = true ? 1 : 2;",
            0,
            "binding",
        ),
        (
            "for (const item of [1]) { console.log(2); }",
            "for (const entry of [1]) { console.log(2); }",
            0,
            "binding",
        ),
        ("const x = [1, 2];", "const x = [3, 4];", 0, "expression"),
        ("const x = [1, 2];", "const y = [1, 2];", 0, "binding"),
        ("1 + 2;", "3 + 4;", 0, "expression"),
        (
            "const x = await tools.echo('a');",
            "const y = await tools.echo('a');",
            0,
            "binding",
        ),
        (
            "const x = await tools.echo('a');",
            "const x = await other.echo('a');",
            0,
            "receiver",
        ),
        (
            "const x = await tools.echo('a');",
            "const x = await tools.read('a');",
            0,
            "operation",
        ),
        (
            "const x = await tools.echo('a');",
            "const x = await tools.echo('b');",
            0,
            "arguments",
        ),
        (
            "const x = await tools.echo({ text:'a', rows:[1] });",
            "const x = await tools.echo({ text:'b', rows:[2] });",
            0,
            "arguments",
        ),
        ("console.log('a');", "console.log('b');", 0, "arguments"),
        ("finish(1);", "finish(2);", 0, "expression"),
        (
            "let state = { x: 1, y: 2 }; state.x = 3;",
            "let state = { x: 1, y: 2 }; state.y = 3;",
            1,
            "target",
        ),
        (
            "let state = { x: 1 }; state.x = 3;",
            "let state = { x: 1 }; state.x = 4;",
            1,
            "expression",
        ),
        (
            "let state = { x: 1 }; state.x += 3;",
            "let state = { x: 1 }; state.x *= 3;",
            1,
            "update",
        ),
        (
            "if (true) { console.log(1); } else { console.log(2); }",
            "if (false) { console.log(1); } else { console.log(2); }",
            0,
            "condition",
        ),
        (
            "if (true) { console.log(1); } else { console.log(2); }",
            "if (true) { console.log(3); } else { console.log(2); }",
            0,
            "then_graph",
        ),
        (
            "if (true) { console.log(1); } else { console.log(2); }",
            "if (true) { console.log(1); } else { console.log(4); }",
            0,
            "else_graph",
        ),
        (
            "while (false) { console.log(1); }",
            "while (true) { console.log(1); }",
            0,
            "condition",
        ),
        (
            "while (false) { console.log(1); }",
            "while (false) { console.log(2); }",
            0,
            "body",
        ),
        (
            "for (const item of [1]) { console.log(item); }",
            "for (const item of [2]) { console.log(item); }",
            0,
            "iterable",
        ),
        (
            "for (const item of [1]) { console.log(2); }",
            "for (const item of [1]) { console.log(3); }",
            0,
            "body",
        ),
    ];
    for (source, edited_source, index, field) in cases {
        assert_lens_laws(source);
        assert_lens_laws(edited_source);
        let graph = workflow_graph_from_source(&canonical(source)).expect("base graph");
        let expected = workflow_graph_from_source(&canonical(edited_source)).expect("edited graph");
        let mut document = serde_json::to_value(&graph).expect("serialize");
        let mut edited = serde_json::to_value(&expected).expect("serialize edited");
        if field == "result_steps" {
            // Removing await changes a call node into a pending computation
            // after projection. Its canonical graph is the authored target.
            edited["main"]["nodes"][index]["kind"][field] = serde_json::json!([]);
        }
        assert_ne!(
            document["main"]["nodes"][index]["kind"][field],
            edited["main"]["nodes"][index]["kind"][field],
            "{field}: fixture edits a real field"
        );
        document["main"]["nodes"][index]["kind"][field] =
            edited["main"]["nodes"][index]["kind"][field].clone();
        let mutated = WorkflowGraph::decode_json_value(document).expect("edited IR decodes");
        let rendered = workflow_graph_to_source(&mutated)
            .unwrap_or_else(|error| panic!("{field}/{source}: {error}"));
        assert_eq!(
            rendered,
            canonical(edited_source),
            "{field}/{source}: IR edit reaches source"
        );
        let reprojected = workflow_graph_from_source(&rendered).expect("reproject edit");
        assert_eq!(reprojected, expected, "{field}/{source}: PutGet");
    }
}

#[test]
#[ignore = "FIG-4151: shadowed loop names need authored-binding provenance in the printer"]
fn workflow_projection_preserves_shadow_loop_label_spans() {
    let source = "const scoped = async () => {\n  const item = 'outer';\n  /** @label Loop read */\n  for (const item of [1,2]) {\n    /** @label Inner read */\n    console.log(item);\n  }\n  /** @label Outer read */\n  console.log(item);\n  return item;\n};\n";
    let environment = lashlang::testing::harness::labeled_test_environment();
    let canonical = canonical(source);
    assert_lens_laws(&canonical);
    let graph = workflow_graph_from_source_with_facets(&canonical, Some(&environment))
        .expect("faceted projection");
    let artifact = lash_typescript::link(&canonical, &environment)
        .expect("admit")
        .artifact;
    let runnable = lash_typescript::workflow_graph::workflow_graph_from_artifact(&artifact);
    assert_eq!(graph.source_identity, runnable.source_identity);
    for (label, expected) in [("Inner read", TypeExpr::Int), ("Outer read", TypeExpr::Str)] {
        let node = graph
            .nodes()
            .find(|node| node.name == label)
            .expect("labeled read");
        let span = node.source_span.expect("labeled read has canonical span");
        assert!(
            canonical[span.start..span.end].contains("console.log(item)"),
            "{label}: {span:?}"
        );
        assert_eq!(
            node.type_facets
                .as_ref()
                .expect("facets")
                .available_variables
                .iter()
                .find(|variable| variable.name == "item")
                .map(|variable| &variable.ty),
            Some(&expected),
            "{label}: lexical binder"
        );
        let admitted_node = runnable
            .nodes()
            .find(|candidate| candidate.id == node.id)
            .expect("same admitted owner/path");
        assert_eq!(admitted_node.source_span, node.source_span);
        assert_eq!(admitted_node.execution_sites, node.execution_sites);
        assert!(
            !node.execution_sites.is_empty(),
            "the label names a real print execution site"
        );
    }
    let loop_node = graph
        .nodes()
        .find(|node| node.name == "Loop read")
        .expect("loop label");
    assert!(matches!(
        loop_node.kind,
        WorkflowNodeKind::Container(WorkflowContainer::For { .. })
    ));
}

#[test]
fn facet_echo_changes_no_execution_or_canonical_diff() {
    let source = "const value = [1,2]; if (true) { console.log(value[0]); } finish(value.length);";
    let environment =
        LashlangHostEnvironment::new(LashlangHostCatalog::new(), LashlangAbilities::all());
    let original =
        workflow_graph_from_source_with_facets(source, Some(&environment)).expect("facets");
    let mut wire = serde_json::to_value(&original).expect("wire");
    fn corrupt(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                if object.contains_key("type_facets") {
                    object.insert("type_facets".into(), serde_json::json!({ "available_variables":[{"name":"forged","ty":"Bool"}], "expected_arguments":[{"slot":[{"arg":999}],"ty":"Str"}], "diagnostics":[] }));
                }
                for value in object.values_mut() {
                    corrupt(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    corrupt(value);
                }
            }
            _ => {}
        }
    }
    corrupt(&mut wire);
    for version in [
        WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
        WORKFLOW_TYPE_FACET_SCHEMA_VERSION + 99,
    ] {
        wire["facet_schema_version"] = serde_json::json!(version);
        let echo = WorkflowGraph::decode_json_value(wire.clone()).expect("facet echo");
        if version != WORKFLOW_TYPE_FACET_SCHEMA_VERSION {
            assert_eq!(echo.facet_schema_version, None);
            assert!(
                echo.nodes().all(|node| node.type_facets.is_none()),
                "stale facets are discarded at decode"
            );
        }
        let rendered = workflow_graph_to_source(&echo).expect("render ignores facets");
        assert_eq!(rendered, canonical(source));
        let reprojection = workflow_graph_from_source_with_facets(&rendered, Some(&environment))
            .expect("derive facets again");
        assert_eq!(reprojection, original);
        assert_eq!(
            reconcile(&echo, &reprojection),
            reconcile(&original, &reprojection)
        );
        let before = lash_typescript::link(source, &environment).expect("original artifact");
        let after = lash_typescript::link(&rendered, &environment).expect("echo artifact");
        assert_eq!(before.artifact.module_ref(), after.artifact.module_ref());
        assert_eq!(
            super::super::agent_surface::finished(source),
            super::super::agent_surface::finished(&rendered)
        );
        assert_eq!(
            super::super::agent_surface::finished(&rendered),
            lashlang::Value::Number(2.0)
        );
    }
}
