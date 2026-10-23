//! MATERIALIZE rules: exact kinds, graph identity, bounds and representation refusals.
use super::*;

fn fixture() -> (LinkedContent, FunctionValue) {
    let definition = parse_definition(
        "function app.saved() -> Any\nkernel 1\nmetering statements\nbody { return value }",
    )
    .unwrap();
    let mut document = Document::new(NumberPolicy::BySpelling, vec![]);
    document
        .functions
        .insert(definition.identity(), definition.clone());
    let link = lash_vocabulary::package::Link {
        document: document.identity(),
        fills: BTreeMap::new(),
    };
    let function = FunctionValue {
        callable: CallableId {
            link: link.identity(),
            function: definition.identity(),
        },
        signature: definition.signature,
        convention: StartConvention::Direct,
        representation: FunctionRepresentation::Bare,
        token: None,
        creation: None,
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: vec![],
        captures: [(Name::new("value"), CaptureValue::Data(Datum::Null))].into(),
        captions: Captions::empty(document.identity()),
    };
    (
        LinkedContent {
            documents: [(document.identity(), document)].into(),
            links: [(link.identity(), link)].into(),
            ..Default::default()
        },
        function,
    )
}
fn helpers() -> BuildHelpers {
    let mut functions = FunctionRegistry::declarations();
    let ids = [
        "function error.new(kind: Text, message: Text, data: Any) -> Error\nkernel 1\ncharge 1\nnative",
        "function timestamp.from_nanoseconds(value: Int) -> Timestamp\nkernel 1\ncharge 1\nnative",
    ].into_iter().map(|s| functions.register(parse_definition(s).unwrap(), None).unwrap()).collect::<Vec<_>>();
    BuildHelpers::new(functions, ids).unwrap()
}
fn open(
    content: &LinkedContent,
    function: &FunctionValue,
    helpers: &BuildHelpers,
) -> Result<MaterializedWorkflow, MaterializeRefusal> {
    materialize_workflow(
        content,
        &WorkflowReference::Callable(function.callable),
        Some(function),
        None,
        helpers,
    )
}
/// MATERIALIZE-VALUES: scalar and ordered container kinds survive; large/live/opaque paths refuse.
#[test]
fn materialize_values_keeps_exact_kinds_and_refuses_unrepresentable_paths() {
    let (content, mut function) = fixture();
    let helpers = helpers();
    let samples = vec![
        (Datum::Null, Expr::Literal(Literal::Null)),
        (Datum::Absent, Expr::Literal(Literal::Absent)),
        (Datum::Bool(true), Expr::Literal(Literal::Bool(true))),
        (Datum::Int(7.into()), Expr::Literal(Literal::Int(7.into()))),
        (
            Datum::Float(Float::new(-0.0)),
            Expr::Literal(Literal::Float(Float::new(-0.0))),
        ),
        (
            Datum::Number(NumberToken::new("7").unwrap()),
            Expr::Literal(Literal::Int(7.into())),
        ),
        (
            Datum::Text("data".into()),
            Expr::Literal(Literal::Text("data".into())),
        ),
        (
            Datum::Bytes(Bytes::parse_hex("00ff").unwrap()),
            Expr::Literal(Literal::Bytes(Bytes::parse_hex("00ff").unwrap())),
        ),
        (
            Datum::Tuple(vec![Datum::Absent]),
            Expr::Tuple(vec![Expr::Literal(Literal::Absent)]),
        ),
        (
            Datum::List(vec![Datum::Null]),
            Expr::List(vec![Expr::Literal(Literal::Null)]),
        ),
        (
            Datum::Set(vec![Datum::Int(2.into()), Datum::Int(1.into())]),
            Expr::Set(vec![
                Expr::Literal(Literal::Int(2.into())),
                Expr::Literal(Literal::Int(1.into())),
            ]),
        ),
        (
            Datum::Record(vec![("z".into(), Datum::Absent)]),
            Expr::Record(vec![RecordEntry {
                field: "z".into(),
                value: Expr::Literal(Literal::Absent),
            }]),
        ),
        (
            Datum::Map(vec![(Datum::Int(1.into()), Datum::Absent)]),
            Expr::Map(vec![MapEntry {
                key: Expr::Literal(Literal::Int(1.into())),
                value: Expr::Literal(Literal::Absent),
            }]),
        ),
        (
            Datum::Timestamp(Timestamp {
                nanoseconds: Integer::parse("123456789012345678901234567890").unwrap(),
            }),
            Expr::Call {
                function: helpers
                    .functions()
                    .named("timestamp.from_nanoseconds")
                    .next()
                    .unwrap(),
                args: vec![Expr::Literal(Literal::Int(
                    Integer::parse("123456789012345678901234567890").unwrap(),
                ))],
            },
        ),
        (
            Datum::Error(Box::new(ErrorDatum {
                kind: "kind".into(),
                message: "message".into(),
                data: Datum::Absent,
            })),
            Expr::Call {
                function: helpers.functions().named("error.new").next().unwrap(),
                args: vec![
                    Expr::Literal(Literal::Text("kind".into())),
                    Expr::Literal(Literal::Text("message".into())),
                    Expr::Literal(Literal::Absent),
                ],
            },
        ),
    ];
    for (datum, expected) in samples {
        function
            .captures
            .insert(Name::new("value"), CaptureValue::Data(datum));
        let opened = open(&content, &function, &helpers).unwrap();
        assert!(
            opened
                .document
                .functions
                .values()
                .any(|f| f.body().is_some_and(|b| b.block.iter().any(
                    |s| matches!(s, Stmt::Let { value: Rhs::Expr(value), .. } if value == &expected)
                )))
        );
    }
    for (value, expected) in [
        (
            CaptureValue::Tuple(vec![CaptureValue::Reference(0)]),
            Expr::Tuple(vec![Expr::Variable(Name::new("saved_callable_0"))]),
        ),
        (
            CaptureValue::List(vec![CaptureValue::Reference(0)]),
            Expr::List(vec![Expr::Variable(Name::new("saved_callable_0"))]),
        ),
        (
            CaptureValue::Set(vec![CaptureValue::Reference(0)]),
            Expr::Set(vec![Expr::Variable(Name::new("saved_callable_0"))]),
        ),
        (
            CaptureValue::Map(vec![(
                CaptureValue::Data(Datum::Absent),
                CaptureValue::Reference(0),
            )]),
            Expr::Map(vec![MapEntry {
                key: Expr::Literal(Literal::Absent),
                value: Expr::Variable(Name::new("saved_callable_0")),
            }]),
        ),
        (
            CaptureValue::Record(vec![("root".into(), CaptureValue::Reference(0))]),
            Expr::Record(vec![RecordEntry {
                field: "root".into(),
                value: Expr::Variable(Name::new("saved_callable_0")),
            }]),
        ),
        (
            CaptureValue::Error {
                kind: "kind".into(),
                message: "message".into(),
                data: Box::new(CaptureValue::Reference(0)),
            },
            Expr::Call {
                function: helpers.functions().named("error.new").next().unwrap(),
                args: vec![
                    Expr::Literal(Literal::Text("kind".into())),
                    Expr::Literal(Literal::Text("message".into())),
                    Expr::Variable(Name::new("saved_callable_0")),
                ],
            },
        ),
    ] {
        function.captures.insert(Name::new("value"), value);
        let opened = open(&content, &function, &helpers).unwrap();
        assert!(
            opened
                .document
                .functions
                .values()
                .any(|f| f.body().is_some_and(|b| b.block.iter().any(
                    |s| matches!(s, Stmt::Let { value: Rhs::Expr(value), .. } if value == &expected)
                )))
        );
    }
    function.captures.insert(
        Name::new("value"),
        CaptureValue::Data(Datum::Text("x".repeat(65537))),
    );
    assert!(matches!(
        open(&content, &function, &helpers),
        Err(MaterializeRefusal::LimitExceeded { .. })
    ));
    for (datum, reason) in [
        (
            Datum::Handle(Handle {
                kind: "live".into(),
                id: "id".into(),
            }),
            UnrepresentableValue::LiveHandle,
        ),
        (
            Datum::Function(function.callable),
            UnrepresentableValue::OpaqueValue,
        ),
    ] {
        function.captures.insert(
            Name::new("value"),
            CaptureValue::Record(vec![("nested".into(), CaptureValue::Data(datum))]),
        );
        assert_eq!(
            open(&content, &function, &helpers).err(),
            Some(MaterializeRefusal::UnrepresentableValue {
                node: 0,
                path: "captures.value.nested".into(),
                reason
            })
        );
    }
}

/// MATERIALIZE-GRAPH: bound instances remain distinct; nested closures, root/back references and surplus nodes stay editable.
#[test]
fn materialize_graph_preserves_instances_cycles_nested_code_and_surplus() {
    let (mut content, mut root) = fixture();
    let link = content.links.remove(&root.callable.link).unwrap();
    let mut document = content.documents.remove(&link.document).unwrap();
    let nested = parse_definition(
        "function app.nested() -> Any\nkernel 1\nmetering statements\nbody { return value }",
    )
    .unwrap();
    let old = root.callable.function;
    let mut definition = document.functions.remove(&old).unwrap();
    let body = definition.body_mut().unwrap();
    body.functions
        .insert(nested.identity(), nested.name.clone());
    body.block.insert(
        0,
        binding(
            Name::new("nested"),
            Expr::Closure(Box::new(Closure {
                function: nested.identity(),
                presentation: None,
            })),
        ),
    );
    body.block.insert(
        1,
        Stmt::If {
            condition: Expr::Literal(Literal::Bool(true)),
            then_block: vec![
                binding(Name::new("value"), Expr::Literal(Literal::Int(11.into()))),
                binding(
                    Name::new("shadowed"),
                    Expr::Closure(Box::new(Closure {
                        function: nested.identity(),
                        presentation: None,
                    })),
                ),
            ],
            else_block: vec![],
        },
    );
    document.functions.insert(nested.identity(), nested);
    root.callable.function = definition.identity();
    document.functions.insert(definition.identity(), definition);
    document.main = Some(vec![Stmt::Print {
        value: Expr::Literal(Literal::Null),
    }]);
    root.captions = Captions::new(
        &document,
        vec![
            CaptionEntry::site(
                Site::new(Unit::Library(root.callable.function), []),
                Caption::new("Copied body", None).unwrap(),
            ),
            CaptionEntry::site(
                Site::new(Unit::Main, [0]),
                Caption::new("Discarded cell main", None).unwrap(),
            ),
        ],
    )
    .unwrap();
    let link = lash_vocabulary::package::Link {
        document: document.identity(),
        fills: BTreeMap::new(),
    };
    root.callable.link = link.identity();
    let mut first = root.clone();
    first
        .captures
        .insert(Name::new("value"), CaptureValue::Data(Datum::Int(7.into())));
    first
        .captures
        .insert(Name::new("back"), CaptureValue::Reference(0));
    let mut second = first.clone();
    second
        .captures
        .insert(Name::new("value"), CaptureValue::Data(Datum::Int(9.into())));
    root.captures
        .insert(Name::new("first"), CaptureValue::Function(Box::new(first)));
    root.captures.insert(
        Name::new("second"),
        CaptureValue::Function(Box::new(second)),
    );
    root.captures
        .insert(Name::new("self"), CaptureValue::Reference(0));
    let mut surplus = root.clone();
    surplus.captures = [(Name::new("value"), CaptureValue::Reference(1))].into();
    root.captures.insert(
        Name::new("surplus"),
        CaptureValue::Function(Box::new(surplus)),
    );
    content.documents.insert(document.identity(), document);
    content.links.insert(link.identity(), link);
    let opened = open(&content, &root, &BuildHelpers::empty()).unwrap();
    let graph = lash_kernel_check::derive(&opened.document, &opened.document.functions).unwrap();
    let capture_values: Vec<_> = graph
        .nodes()
        .iter()
        .filter_map(|n| {
            if let lash_kernel_check::NodeKind::Expr(lash_kernel_check::ExprNode::Literal(
                Literal::Int(v),
            )) = &n.kind
            {
                Some(v.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(capture_values.contains(&7.into()) && capture_values.contains(&9.into()));
    assert!(graph.session_reads().is_empty());
    let mut root_references = 0;
    for binding in graph
        .bindings()
        .iter()
        .filter(|b| b.name.as_str() == "self" || b.name.as_str().starts_with("back"))
    {
        let lash_kernel_check::NodeKind::Stmt(lash_kernel_check::StmtNode::Let { value, .. }) =
            graph.node(binding.declared_at).kind
        else {
            continue;
        };
        if let lash_kernel_check::NodeKind::Expr(lash_kernel_check::ExprNode::Variable(
            lash_kernel_check::Reference::Binding(target),
        )) = graph.node(value).kind
        {
            assert_eq!(
                graph.binding(target).name,
                Name::new("saved_callable_0"),
                "root/back references share the same callable binding"
            );
            root_references += 1;
        }
    }
    assert_eq!(root_references, 3);
    let mut nested_captures = BTreeMap::<FunctionId, Vec<Vec<lash_kernel_check::Reference>>>::new();
    for node in graph.nodes() {
        if let lash_kernel_check::NodeKind::Expr(lash_kernel_check::ExprNode::Closure {
            function,
            captures,
            ..
        }) = &node.kind
        {
            nested_captures
                .entry(*function)
                .or_default()
                .push(captures.clone());
        }
    }
    assert_eq!(
        nested_captures
            .values()
            .filter(|captures| captures.len() == 2 && captures[0] != captures[1])
            .count(),
        4,
        "each environment's repeated nested body keeps both lexical shadows"
    );
    assert_eq!(
        opened.captions.entries().len(),
        4,
        "one copied-body caption per bound instance"
    );
    assert_eq!(
        opened.dropped,
        vec![CaptionTarget::Site(Site::new(Unit::Main, [0]))]
    );
    assert_eq!(opened.document.exports.len(), 1);
    assert!(
        graph
            .bindings()
            .iter()
            .any(|b| b.name.as_str().starts_with("surplus"))
    );
    let closures = graph
        .nodes()
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                lash_kernel_check::NodeKind::Expr(lash_kernel_check::ExprNode::Closure { .. })
            )
        })
        .count();
    assert!(closures >= 8);
    root.captures
        .insert(Name::new("value"), CaptureValue::Reference(99));
    assert!(matches!(
        open(&content, &root, &BuildHelpers::empty()),
        Err(MaterializeRefusal::InvalidReference { node: 0, index: 99 })
    ));
}
