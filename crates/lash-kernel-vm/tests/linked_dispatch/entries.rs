//! Laws for calls that enter an export through an import.

use super::*;

/// The `service.pause` operation at slot 0.
fn pause_import() -> BTreeMap<ImportSlot, Import> {
    BTreeMap::from([(
        ImportSlot(0),
        Import {
            name: QualifiedName {
                package: lash_kernel_value::PackagePath::parse("service")
                    .unwrap_or_else(|error| panic!("entry law fixture: {error:?}")),
                member: lash_kernel_value::Segment::new("pause")
                    .unwrap_or_else(|error| panic!("entry law fixture: {error:?}")),
                type_member: None,
            },
            kind: ImportKind::Operation {
                signature: signature(vec![]),
            },
        },
    )])
}

/// A program of two Links: a defining one over `functions` (slot 0 the
/// pause operation) with `exports`, and a root whose main calls them
/// through `imports`, slot `i` filled by the named export under the
/// import signature given, its convention the export's.
fn entry_world(
    functions: Vec<FunctionDefinition>,
    exports: Vec<(&str, FunctionExport)>,
    imports: Vec<(&str, Signature)>,
    root_functions: Vec<FunctionDefinition>,
    main: Vec<Stmt>,
) -> Program {
    let mut child = document(
        pause_import(),
        functions.into_iter().map(|f| (f.identity(), f)).collect(),
        None,
    );
    let mut conventions = BTreeMap::new();
    for (name, export) in exports {
        conventions.insert(name.to_owned(), export.convention.semantic());
        child
            .exports
            .insert(Name::new(name), lash_kernel_doc::Export::Function(export));
    }
    let child_id = child.identity();
    let child_link = Link {
        helper_fills: Default::default(),
        document: child_id,
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Operation(OperationRequirement::new(
                signature(vec![]),
                ContractId::new("1"),
            )),
        )]),
    };
    let child_link_id = child_link.identity();
    let mut slots = BTreeMap::new();
    let mut fills = BTreeMap::new();
    for (at, (name, contract)) in imports.into_iter().enumerate() {
        let slot = ImportSlot(at as u32);
        slots.insert(
            slot,
            Import {
                name: QualifiedName {
                    package: lash_kernel_value::PackagePath::parse("worker")
                        .unwrap_or_else(|error| panic!("entry law fixture: {error:?}")),
                    member: lash_kernel_value::Segment::new(name)
                        .unwrap_or_else(|error| panic!("entry law fixture: {error:?}")),
                    type_member: None,
                },
                kind: ImportKind::Code {
                    signature: contract,
                    convention: conventions[name],
                },
            },
        );
        fills.insert(
            slot,
            Fill::Export {
                link: child_link_id,
                export: ExportName {
                    name: Name::new(name),
                    member: None,
                },
            },
        );
    }
    let root = document(
        slots,
        root_functions
            .into_iter()
            .map(|f| (f.identity(), f))
            .collect(),
        Some(main),
    );
    let root_id = root.identity();
    let root_link = Link {
        helper_fills: Default::default(),
        document: root_id,
        fills,
    };
    Program::prepare(
        root_link.identity(),
        BTreeMap::from([(root_id, Arc::new(root)), (child_id, Arc::new(child))]),
        &BTreeMap::from([
            (root_link.identity(), root_link),
            (child_link_id, child_link),
        ]),
        PageId([9; 32]),
        PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    )
    .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"))
}

/// What a run of `program`'s main observably gives: its end, how many
/// effects it requested, and its charge. Each request is answered null;
/// the run is sealed and reopened cold at request `cold` (counted from 0).
fn entry_run(program: &Program, cold: Option<usize>) -> (End, usize, Option<u64>) {
    let pages = Pages::default();
    let start = CellStart {
        owner: PageId([8; 32]),
        target: Target::Main,
        args: vec![],
        session: None,
        modes: BTreeMap::new(),
        host: vec![],
    };
    let mut machine =
        KernelMachine::start(program.clone(), bounds(), start, Box::new(pages.clone()))
            .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
    let mut requests = 0;
    loop {
        match machine
            .run(&mut Passive, 100_000)
            .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"))
        {
            Step::Slice => {}
            Step::Ended(end) => return (end, requests, machine.meters().charged),
            Step::Parked(park) => {
                let [Request::Effect(request)] = park.requests.as_slice() else {
                    panic!("expected one request: {park:?}")
                };
                let wait = request.wait;
                if cold == Some(requests) {
                    let saved = machine
                        .checkpoint()
                        .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
                    machine = KernelMachine::open(
                        program.clone(),
                        bounds(),
                        &saved.checkpoint(),
                        Box::new(pages.clone()),
                    )
                    .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
                }
                requests += 1;
                machine
                    .deliver(wait, Outcome::Completed(Datum::Null))
                    .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
            }
        }
    }
}

fn returns(value: bool) -> Stmt {
    Stmt::Return {
        value: Expr::Literal(Literal::Bool(value)),
    }
}

fn tried(body: Vec<Stmt>, catch: Option<Vec<Stmt>>, finally: Option<Vec<Stmt>>) -> Stmt {
    Stmt::Try(lash_kernel_doc::TryStmt {
        body,
        catch: catch.map(|body| lash_kernel_doc::Catch {
            binding: Name::new("caught"),
            body,
        }),
        finally,
    })
}

fn bound(name: &str) -> Stmt {
    Stmt::Let {
        name: Name::new(name),
        value: Rhs::Expr(null()),
    }
}

fn called(slot: u32, name: &str, args: Vec<Atom>) -> Stmt {
    Stmt::Let {
        name: Name::new(name),
        value: Rhs::Action(Action::Call {
            callee: Callee::Import(ImportSlot(slot)),
            args,
        }),
    }
}

/// K-DOC-003, K-ERR-003, K-TASK-019: a call through an import is a call of
/// the export under its start convention, so a raise of the export's
/// settlement or run-root cleanup reaches the caller's handler, whatever
/// handlers the body entered, left or still held when it returned. Each
/// case's main catches that raise and returns true; a hook that parks
/// first gives the same end and charge reopened cold at its park.
#[test]
fn an_imported_entry_leaves_only_its_own_handlers() {
    let own = || Some(vec![returns(false)]);
    let quiet = || Some(vec![]);
    let bodies = [
        ("no try", vec![returns(false)]),
        (
            "a completed try",
            vec![tried(vec![bound("x")], quiet(), None), returns(false)],
        ),
        (
            "completed and nested tries",
            vec![
                tried(
                    vec![
                        tried(vec![bound("x")], quiet(), None),
                        tried(vec![bound("y")], quiet(), Some(vec![bound("z")])),
                    ],
                    quiet(),
                    None,
                ),
                tried(vec![bound("w")], None, Some(vec![])),
                returns(false),
            ],
        ),
        (
            "a return inside a try",
            vec![tried(vec![returns(false)], own(), None), returns(false)],
        ),
        (
            "an early return inside nested tries and a finally",
            vec![
                tried(
                    vec![tried(
                        vec![Stmt::If {
                            condition: Expr::Literal(Literal::Bool(true)),
                            then_block: vec![returns(false)],
                            else_block: vec![],
                        }],
                        own(),
                        None,
                    )],
                    None,
                    Some(vec![bound("z")]),
                ),
                returns(false),
            ],
        ),
    ];
    let public = Signature {
        params: vec![],
        result: Type::Bool,
    };
    let main = vec![
        tried(
            vec![called(0, "a", vec![])],
            Some(vec![returns(true)]),
            None,
        ),
        returns(false),
    ];
    for (case, body) in bodies {
        for parks in [false, true] {
            for python in [false, true] {
                let mut hook_block = vec![Stmt::Throw {
                    value: Expr::Literal(Literal::Bool(true)),
                }];
                if parks {
                    hook_block.insert(0, perform());
                }
                let hook_params = match python {
                    true => vec![],
                    false => vec![Param {
                        name: Name::new("value"),
                        ty: Type::Any,
                        optional: false,
                    }],
                };
                let hook = function(
                    "entry.hook",
                    Signature {
                        params: hook_params,
                        result: Type::Any,
                    },
                    pause_import(),
                    hook_block,
                );
                let target = lash_kernel_doc::FunctionTarget::Local(hook.identity());
                let convention = match python {
                    true => StartConvention::PyDirect { cleanup: target },
                    false => StartConvention::TsDirect { await_fn: target },
                };
                let body = function("entry.body", public.clone(), pause_import(), body.clone());
                let export = FunctionExport {
                    function: body.identity(),
                    signature: public.clone(),
                    convention,
                    asynchronous: !python,
                    rest: false,
                };
                let program = entry_world(
                    vec![body, hook],
                    vec![("entry", export)],
                    vec![("entry", public.clone())],
                    vec![],
                    main.clone(),
                );
                let warm = entry_run(&program, None);
                let End::Finished(done) = &warm.0 else {
                    panic!("{case}, parks {parks}, python {python}: {:?}", warm.0)
                };
                assert_eq!(
                    done.result,
                    Datum::Bool(true),
                    "{case}, parks {parks}, python {python}"
                );
                if parks {
                    let cold = entry_run(&program, Some(0));
                    let End::Finished(again) = &cold.0 else {
                        panic!("{case}, python {python}, cold: {:?}", cold.0)
                    };
                    assert_eq!(
                        (&again.result, cold.1, cold.2),
                        (&done.result, warm.1, warm.2),
                        "{case}, python {python}, cold"
                    );
                }
            }
        }
    }
}

/// K-DOC-003, FIG-7143: an export may serve a narrower import with a wider
/// signature, and a call through the import holds to the import's, under
/// every start convention: a null for the import's `y: Bool` is refused
/// at `/input/y` before the body runs (its first statement parks, so no
/// request is made), as a plain call of a `Direct` export refuses it. An
/// admitted argument reaches the body; a packed Python keyword argument
/// by the export's own parameter name, which the import does not share.
#[test]
fn an_adapted_import_holds_to_the_imports_narrower_signature() {
    let any = |name: &str| Param {
        name: Name::new(name),
        ty: Type::Any,
        optional: false,
    };
    let wide = Signature {
        params: vec![any("x")],
        result: Type::Any,
    };
    let narrow = Signature {
        params: vec![Param {
            name: Name::new("y"),
            ty: Type::Bool,
            optional: false,
        }],
        result: Type::Any,
    };
    let read = |target: &str, index: Literal| {
        Expr::Member(Box::new(lash_kernel_doc::Member::Index {
            target: Expr::Variable(Name::new(target)),
            index: Expr::Literal(index),
        }))
    };
    let identity = function(
        "entry.identity",
        Signature {
            params: vec![any("value")],
            result: Type::Any,
        },
        BTreeMap::new(),
        vec![Stmt::Return {
            value: Expr::Variable(Name::new("value")),
        }],
    );
    let cleanup = function("entry.cleanup", signature(vec![]), BTreeMap::new(), vec![]);
    let settle = lash_kernel_doc::FunctionTarget::Local(identity.identity());
    let direct = (vec![any("x")], Expr::Variable(Name::new("x")));
    let ts_listed = (
        vec![any("this"), any("args")],
        read("args", Literal::Int(0.into())),
    );
    let py_listed = (
        vec![any("args"), any("kwargs"), any("exc")],
        read("args", Literal::Int(0.into())),
    );
    let py_named = (
        vec![any("args"), any("kwargs"), any("exc")],
        read("kwargs", Literal::Text("x".into())),
    );
    let cases = [
        ("Direct", StartConvention::Direct, direct.clone()),
        (
            "TsDirect",
            StartConvention::TsDirect {
                await_fn: settle.clone(),
            },
            direct.clone(),
        ),
        (
            "TsArgsList",
            StartConvention::TsArgsList {
                await_fn: settle.clone(),
                same_fn: settle.clone(),
            },
            ts_listed,
        ),
        (
            "PyArgsList positional",
            StartConvention::PyArgsList { positional: 1 },
            py_listed,
        ),
        (
            "PyArgsList keyword",
            StartConvention::PyArgsList { positional: 0 },
            py_named,
        ),
        (
            "PyDirect",
            StartConvention::PyDirect {
                cleanup: lash_kernel_doc::FunctionTarget::Local(cleanup.identity()),
            },
            direct,
        ),
    ];
    let dynamic = function(
        "entry.dynamic",
        Signature {
            params: vec![],
            result: Type::Any,
        },
        BTreeMap::new(),
        vec![Stmt::Return { value: null() }],
    );
    let refused = vec![
        Stmt::Let {
            name: Name::new("n"),
            value: Rhs::Action(Action::Call {
                callee: Callee::Local(dynamic.identity()),
                args: vec![],
            }),
        },
        called(0, "a", vec![Atom::Variable(Name::new("n"))]),
        Stmt::Return {
            value: Expr::Variable(Name::new("a")),
        },
    ];
    let admitted = vec![
        called(0, "a", vec![Atom::Literal(Literal::Bool(true))]),
        Stmt::Return {
            value: Expr::Variable(Name::new("a")),
        },
    ];
    let mismatch = Datum::Record(vec![
        ("actual".into(), Datum::Text("null".into())),
        ("expected".into(), Datum::Text("bool".into())),
        ("path".into(), Datum::Text("/input/y".into())),
    ]);
    for (case, convention, (params, value)) in cases {
        let body = function(
            "entry.wide",
            Signature {
                params,
                result: Type::Any,
            },
            pause_import(),
            vec![perform(), Stmt::Return { value }],
        );
        let export = FunctionExport {
            function: body.identity(),
            signature: wide.clone(),
            convention,
            asynchronous: false,
            rest: false,
        };
        let world = |main: &Vec<Stmt>| {
            entry_world(
                vec![body.clone(), identity.clone(), cleanup.clone()],
                vec![("wide", export.clone())],
                vec![("wide", narrow.clone())],
                vec![dynamic.clone()],
                main.clone(),
            )
        };
        let (end, requests, _) = entry_run(&world(&refused), None);
        let End::Error(lash_kernel_vm::RunError::Uncaught {
            error: Datum::Error(error),
            ..
        }) = &end
        else {
            panic!("{case}: a null for y: Bool is refused: {end:?}")
        };
        let Datum::Record(data) = error.data() else {
            panic!("{case}: {error:?}")
        };
        let found = data
            .iter()
            .find(|(key, _)| key == "mismatch")
            .map(|(_, at)| at);
        assert_eq!(
            (error.kind.as_str(), found, requests),
            ("function_argument", Some(&mismatch), 0),
            "{case}"
        );
        let (end, requests, _) = entry_run(&world(&admitted), None);
        assert!(
            matches!(&end, End::Finished(done) if done.result == Datum::Bool(true)),
            "{case}: {end:?}"
        );
        assert_eq!(requests, 1, "{case}");
    }
}

/// K-DOC-003, contract §2: two exports of one body under different start
/// conventions are two entries, and a sealed frame of one reopens as that
/// one. Exports `first` and `second` settle one parking body through hooks
/// that park and answer true and false; main calls both, in either order,
/// and returns `first`'s answer, true. Reopened cold at each of the four
/// parks (each body's and each hook's), the run ends, requests and is
/// charged as the warm run does.
#[test]
fn two_conventions_of_one_body_reopen_as_their_own_entries() {
    let public = Signature {
        params: vec![],
        result: Type::Bool,
    };
    let body = function(
        "entry.shared",
        public.clone(),
        pause_import(),
        vec![perform(), returns(false)],
    );
    let hooks: Vec<_> = [("entry.first", true), ("entry.second", false)]
        .into_iter()
        .map(|(name, answer)| {
            function(
                name,
                Signature {
                    params: vec![Param {
                        name: Name::new("value"),
                        ty: Type::Any,
                        optional: false,
                    }],
                    result: Type::Bool,
                },
                pause_import(),
                vec![perform(), returns(answer)],
            )
        })
        .collect();
    let exports = ["first", "second"]
        .into_iter()
        .zip(&hooks)
        .map(|(name, hook)| {
            (
                name,
                FunctionExport {
                    function: body.identity(),
                    signature: public.clone(),
                    convention: StartConvention::TsDirect {
                        await_fn: lash_kernel_doc::FunctionTarget::Local(hook.identity()),
                    },
                    asynchronous: true,
                    rest: false,
                },
            )
        })
        .collect::<Vec<_>>();
    for order in [["first", "second"], ["second", "first"]] {
        let imports = order.iter().map(|name| (*name, public.clone())).collect();
        let main = vec![
            called(0, order[0], vec![]),
            called(1, order[1], vec![]),
            Stmt::Return {
                value: Expr::Variable(Name::new("first")),
            },
        ];
        let mut functions = vec![body.clone()];
        functions.extend(hooks.iter().cloned());
        let program = entry_world(functions, exports.clone(), imports, vec![], main);
        let warm = entry_run(&program, None);
        assert!(
            matches!(&warm.0, End::Finished(done) if done.result == Datum::Bool(true)),
            "{order:?}: {:?}",
            warm.0
        );
        assert_eq!(warm.1, 4, "{order:?}");
        for park in 0..4 {
            assert_eq!(
                entry_run(&program, Some(park)),
                warm,
                "{order:?}, cold at park {park}"
            );
        }
    }
}
