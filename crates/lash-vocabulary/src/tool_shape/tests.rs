use serde_json::{Value, json};

use super::*;

fn document(schema: Value) -> SchemaDocument {
    SchemaDocument::from_json_schema(&schema, 8).expect("representable schema")
}
fn field<'a>(shape: &'a SchemaShape, name: &str) -> &'a SchemaShape {
    &shape
        .fields()
        .iter()
        .find(|field| field.name == name)
        .expect("field")
        .shape
}
fn reference(shape: &SchemaShape) -> DefinitionId {
    let ShapeKind::Ref(id) = shape.kind else {
        panic!("expected reference: {shape:?}")
    };
    id
}

#[test]
fn self_referencing_filter_preserves_recursive_edge() {
    let doc = document(json!({"type":"object", "properties": {
        "and":{"type":"array", "items":{"$ref":"#"}}
    }}));
    let ShapeKind::List(item) = &field(&doc.roots[0], "and").kind else {
        panic!("array")
    };
    let id = reference(item);
    assert_eq!(doc.definitions[id.0].shape, doc.roots[0]);
    assert_eq!(doc.reachable_closure(&doc.roots), [id]);
    assert_eq!(
        doc.definitions.len(),
        1,
        "a cycle is an edge, not unrolled nodes"
    );
}

#[test]
fn local_references_preserve_nodes_and_edges() {
    let doc = document(json!({
        "$defs":{"Node":{"type":"object", "description":"A node.",
            "properties":{"next":{"$ref":"#/$defs/Node"}}}},
        "type":"object", "properties":{"head":{"$ref":"#/$defs/Node"}}
    }));
    let id = reference(field(&doc.roots[0], "head"));
    assert_eq!(reference(field(&doc.definitions[id.0].shape, "next")), id);
    assert_eq!(
        doc.definitions[id.0].shape.description.as_deref(),
        Some("A node.")
    );
    assert_eq!(doc.definitions.len(), 2);
    assert_eq!(
        document(json!({"$ref":"lash.ProcessRecord"})).roots[0].kind,
        ShapeKind::Named("lash.ProcessRecord".into())
    );
}

#[test]
fn mutual_recursion_reserves_each_target_before_importing_its_body() {
    let doc = document(json!({"$defs":{
        "A":{"type":"object", "properties":{"b":{"$ref":"#/$defs/B"}}},
        "B":{"type":"object", "properties":{"a":{"$ref":"#/$defs/A"}}}
    },"$ref":"#/$defs/A"}));
    let a = reference(&doc.roots[0]);
    let b = reference(field(&doc.definitions[a.0].shape, "b"));
    assert_ne!(a, b);
    assert_eq!(reference(field(&doc.definitions[b.0].shape, "a")), a);
    assert_eq!(doc.reachable_closure(&doc.roots), [a, b]);
}

#[test]
fn shared_enum_references_keep_one_definition_and_exact_literals() {
    let doc = document(json!({"definitions":{"Status":{"enum":["open", "closed"]}},
    "type":"object", "properties":{
        "before":{"$ref":"#/definitions/Status"},
        "after":{"$ref":"#/definitions/Status"}
    }}));
    let id = reference(field(&doc.roots[0], "before"));
    assert_eq!(reference(field(&doc.roots[0], "after")), id);
    assert_eq!(
        doc.definitions[id.0].shape.kind,
        ShapeKind::Literals(vec![json!("open"), json!("closed")])
    );
    assert_eq!(doc.reachable_closure(&doc.roots), [id]);
}

#[test]
fn arbitrary_local_pointers_decode_tilde_and_slash_escapes() {
    let doc = document(json!({"type":"object", "properties":{
        "a~/b":{"type":"string"},
        "copy":{"$ref":"#/properties/a~0~1b"}
    }}));
    let id = reference(field(&doc.roots[0], "copy"));
    assert_eq!(doc.definitions[id.0].suggested_name, "a~/b");
    assert_eq!(doc.definitions[id.0].shape.kind, ShapeKind::Str);
    assert_eq!(ReadableNameAllocator::default().allocate(&doc, id), "a__b");
}

#[test]
fn use_site_annotation_overrides_do_not_mutate_shared_definition_notes() {
    let doc = document(json!({"$defs":{"Status":{"enum":["open", "closed"],
    "description":"Shared status", "default":"open"}},
    "type":"object", "properties":{
        "before":{"$ref":"#/$defs/Status", "description":"Previous", "default":"closed"},
        "after":{"$ref":"#/$defs/Status"}
    }}));
    let before = field(&doc.roots[0], "before");
    let after = field(&doc.roots[0], "after");
    let id = reference(before);
    assert_eq!(reference(after), id);
    assert_eq!(before.description.as_deref(), Some("Previous"));
    assert_eq!(before.default, Some(json!("closed")));
    assert_eq!(after.description, None, "an edge owns only its overlay");
    assert_eq!(
        doc.definitions[id.0].shape.description.as_deref(),
        Some("Shared status")
    );
    assert_eq!(doc.definitions[id.0].shape.default, Some(json!("open")));
    let rows = doc.note_rows();
    assert_eq!(
        rows.len(),
        2,
        "notes are finite and definition notes appear once"
    );
    assert_eq!(rows[0].path, "$0.before");
    assert_eq!(rows[1].path, "#/$defs/Status");
}

/// Value refinements retain their own paths without changing the base type.
#[test]
fn value_narrowing_keywords_keep_path_notes() {
    for (kind, keyword, value) in [
        ("object", "minProperties", json!(1)),
        ("object", "maxProperties", json!(3)),
        ("number", "multipleOf", json!(0.5)),
        ("array", "uniqueItems", json!(true)),
        ("array", "contains", json!({"type":"integer"})),
        ("array", "minContains", json!(1)),
        ("array", "maxContains", json!(2)),
        ("object", "propertyNames", json!({"pattern":"^x"})),
        (
            "object",
            "patternProperties",
            json!({"^x":{"type":"string"}}),
        ),
        ("string", "not", json!({"const":"forbidden"})),
    ] {
        let expected = vec![format!("{keyword} {value}")];
        for schema in [
            json!({"type":kind}),
            json!({"$ref":"#/$defs/Value"}),
            json!({"anyOf":[{"type":kind},{"type":"null"}]}),
            json!({"allOf":[{"type":kind}]}),
        ] {
            let mut root = json!({"type":"object", "properties":{"value":schema},
                "$defs":{"Value":{"type":kind}}});
            let base = document(root.clone());
            root["properties"]["value"][keyword] = value.clone();
            let doc = document(root);
            let shape = field(&doc.roots[0], "value");
            assert_eq!(
                std::mem::discriminant(&shape.kind),
                std::mem::discriminant(&field(&base.roots[0], "value").kind),
                "{keyword} preserves the base type"
            );
            assert_eq!(shape.constraints.notes(), expected, "{keyword}");
            let rows = doc.note_rows();
            assert_eq!(rows.len(), 1, "{keyword}");
            assert_eq!(rows[0].path, "$0.value");
            assert_eq!(rows[0].shape.constraints.notes(), expected);
        }
    }
    let doc = document(json!({"allOf":[{"type":"string", "not":{"const":"inner"}}],
        "not":{"const":"outer"}}));
    assert_eq!(
        doc.roots[0].constraints.notes(),
        [r#"not {"const":"outer"}"#, r#"not {"const":"inner"}"#,],
        "a composition must retain both refinements of the same keyword"
    );
    let doc = document(json!({"patternProperties":{"^x":{"type":"string"}},
        "additionalProperties":false}));
    let ShapeKind::Object(object) = &doc.roots[0].kind else {
        panic!("pattern properties describe an object")
    };
    assert!(matches!(&object.extra_keys, ExtraKeys::Open(_)));
    let doc = document(json!({"type":"string", "x-host":{"anything":true}}));
    assert_eq!(doc.roots[0], SchemaShape::from(ShapeKind::Str));
}

#[test]
fn canonical_closure_keeps_notes_edges_and_array_order_but_ignores_unreachable_definitions() {
    let base = json!({"$ref":"#/$defs/Filter", "$defs":{
        "Filter":{"type":"object", "properties":{"next":{"$ref":"#/$defs/Filter"}},
            "description":"Recursive", "default":{"$defs":{"data":1}}, "required":["next"]}
    }});
    let identity = |schema| {
        let doc = document(schema);
        doc.canonical_identity(reference(&doc.roots[0]))
    };
    let expected = identity(base.clone());
    let mut extended = base.clone();
    extended["$defs"]["Unused"] = json!({"not":{}});
    assert_eq!(identity(extended), expected);
    let mut noted = base.clone();
    noted["$defs"]["Filter"]["description"] = json!("Different");
    assert_ne!(identity(noted), expected);
    let mut data = base.clone();
    data["$defs"]["Filter"]["default"]["$defs"]["data"] = json!(2);
    assert_ne!(
        identity(data),
        expected,
        "data called $defs must not be stripped"
    );
    let first = document(json!({"$ref":"#/$defs/E", "$defs":{"E":{"enum":[1,2]}}}));
    let second = document(json!({"$defs":{"E":{"enum":[2,1]}}, "$ref":"#/$defs/E"}));
    assert_ne!(
        first.canonical_identity(reference(&first.roots[0])),
        second.canonical_identity(reference(&second.roots[0]))
    );
    // Map insertion order is irrelevant even under serde_json's preserve_order feature.
    let reordered: Value = serde_json::from_str(r##"{"$defs":{"Filter":{"required":["next"],"default":{"$defs":{"data":1}},"description":"Recursive","properties":{"next":{"$ref":"#/$defs/Filter"}},"type":"object"}},"$ref":"#/$defs/Filter"}"##).unwrap();
    assert_eq!(identity(reordered), expected);
}

#[test]
fn readable_names_deduplicate_closures_and_preserve_the_append_prefix() {
    let one = document(json!({"$ref":"#/$defs/Filter", "$defs":{"Filter":{"type":"string"}}}));
    let two = document(json!({"$ref":"#/$defs/Filter", "$defs":{"Filter":{"type":"number"}}}));
    let three = document(json!({"$ref":"#/$defs/Filter", "$defs":{"Filter":{"type":"boolean"}}}));
    let mut names = ReadableNameAllocator::new(["Filter", "Filter_2", "Host", "tools"]);
    let first = names.allocate(&one, reference(&one.roots[0]));
    assert_eq!(first, "Filter_3");
    assert_eq!(names.allocate(&two, reference(&two.roots[0])), "Filter_4");
    assert_eq!(
        names.allocate(&three, reference(&three.roots[0])),
        "Filter_5"
    );
    assert_eq!(names.allocate(&one, reference(&one.roots[0])), first);
    let mut unrelated = one.clone();
    unrelated.definitions[0].source["$defs"]["Unused"] = json!({});
    assert_eq!(
        names.allocate(&unrelated, reference(&unrelated.roots[0])),
        first
    );
}

#[test]
fn notes_keep_union_branch_meaning_and_stop_at_recursive_edges() {
    let doc = document(json!({"anyOf":[
        {"type":"object", "properties":{"v":{"type":"integer", "description":"Count"}}},
        {"type":"object", "properties":{"v":{"type":"string", "description":"Name"}}}
    ]}));
    let rows = doc.note_rows();
    assert_eq!(
        rows.iter()
            .map(|row| (row.path.as_str(), row.shape.description.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("$0|object.v", Some("Count")),
            ("$0|object (alternative 2).v", Some("Name"))
        ]
    );
}

#[test]
fn lash_process_and_fixed_tuple_contracts_are_preserved() {
    assert_eq!(
        document(json!({"x-lash":{"kind":"process_unknown"}})).roots[0].kind,
        ShapeKind::Process(None)
    );
    let doc = document(json!({"x-lash":{"kind":"process", "signature":{
        "params":[{"name":"event", "schema":{"type":"string"}}], "output":{"type":"integer"}
    }}}));
    assert_eq!(
        doc.roots[0].kind,
        ShapeKind::Process(Some(ProcessShape {
            params: vec![ProcessParamShape {
                name: "event".into(),
                shape: ShapeKind::Str.into()
            }],
            output: Box::new(ShapeKind::Int.into())
        }))
    );
    assert_eq!(
        document(
            json!({"type":"array", "prefixItems":[{"type":"string"},{"type":"integer"}],
        "items":false, "minItems":2})
        )
        .roots[0]
            .kind,
        ShapeKind::Tuple(vec![ShapeKind::Str.into(), ShapeKind::Int.into()])
    );
}
