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

/// K-DOC-003, FIG-7143: an adapted import checks the call's result by its
/// own signature, not the export's. An export declaring `Bool` serves an
/// import declaring `Any`; its body returns null, which the import's
/// contract admits, so the call returns null as a call of that contract
/// does.
#[test]
fn an_adapted_import_checks_results_by_its_own_signature() {
    let exported = Signature {
        params: vec![],
        result: Type::Bool,
    };
    let imported = Signature {
        params: vec![],
        result: Type::Any,
    };
    let body = function(
        "entry.unfit",
        imported.clone(),
        BTreeMap::new(),
        vec![Stmt::Return { value: null() }],
    );
    let hook = function(
        "entry.settle",
        Signature {
            params: vec![Param {
                name: Name::new("value"),
                ty: Type::Any,
                optional: false,
            }],
            result: Type::Any,
        },
        BTreeMap::new(),
        vec![Stmt::Return {
            value: Expr::Variable(Name::new("value")),
        }],
    );
    let export = FunctionExport {
        function: body.identity(),
        signature: exported,
        convention: StartConvention::TsDirect {
            await_fn: lash_kernel_doc::FunctionTarget::Local(hook.identity()),
        },
        asynchronous: false,
        rest: false,
    };
    let program = entry_world(
        vec![body, hook],
        vec![("entry", export)],
        vec![("entry", imported)],
        vec![],
        vec![
            called(0, "a", vec![]),
            Stmt::Return {
                value: Expr::Variable(Name::new("a")),
            },
        ],
    );
    let (end, _, _) = entry_run(&program, None);
    assert!(
        matches!(&end, End::Finished(done) if done.result == Datum::Null),
        "{end:?}"
    );
}

/// What a start of `target` of `program` ends with, sliced one unit at a
/// time and, `cold`, sealed and reopened at every slice.
fn started_end(program: &Program, target: Target, cold: bool) -> End {
    let pages = Pages::default();
    let start = CellStart {
        owner: PageId([8; 32]),
        target,
        args: vec![],
        session: None,
        modes: BTreeMap::new(),
        host: vec![],
    };
    let mut machine =
        KernelMachine::start(program.clone(), bounds(), start, Box::new(pages.clone()))
            .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
    loop {
        match machine
            .run(&mut Passive, 1)
            .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"))
        {
            Step::Ended(end) => return end,
            Step::Slice if cold => {
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
            Step::Slice => {}
            Step::Parked(park) => panic!("entry law fixture parked: {park:?}"),
        }
    }
}

/// K-TASK-019, FIG-6810, K-DOC-003: a raise an entry's run-root cleanup
/// held and let go of again stands nowhere in the entry: started
/// directly, the run ends with no site; called through an import, the
/// caller's statement names it, as it names any raise a call leaves
/// with. A raise of the cleanup itself, and a TypeScript body's raise a
/// `finally` held, stand where they do either way. Warm and reopened cold
/// at every slice.
#[test]
fn an_imported_entry_raises_from_the_site_a_plain_call_does() {
    use lash_kernel_value::sites::{Site, Unit as SiteUnit};
    let public = Signature {
        params: vec![],
        result: Type::Bool,
    };
    let throw = |value: bool| Stmt::Throw {
        value: Expr::Literal(Literal::Bool(value)),
    };
    let caller = Site::new(SiteUnit::Main, vec![0]);
    for python in [true, false] {
        for cleanup_raises in [false, true] {
            let (functions, export, sites) = if python {
                let body = function(
                    "entry.raising",
                    public.clone(),
                    BTreeMap::new(),
                    vec![throw(true)],
                );
                let cleanup = function(
                    "entry.cleanup",
                    signature(vec![]),
                    BTreeMap::new(),
                    match cleanup_raises {
                        true => vec![throw(false)],
                        false => vec![Stmt::Return { value: null() }],
                    },
                );
                let raised = Site::new(SiteUnit::Library(cleanup.identity()), vec![0]);
                let export = FunctionExport {
                    function: body.identity(),
                    signature: public.clone(),
                    convention: StartConvention::PyDirect {
                        cleanup: lash_kernel_doc::FunctionTarget::Local(cleanup.identity()),
                    },
                    asynchronous: false,
                    rest: false,
                };
                // (direct, imported)
                let sites = match cleanup_raises {
                    false => (None, Some(caller.clone())),
                    true => (Some(raised.clone()), Some(raised)),
                };
                (vec![body, cleanup], export, sites)
            } else {
                let finally = match cleanup_raises {
                    true => vec![throw(false)],
                    false => vec![bound("z")],
                };
                let body = function(
                    "entry.raising",
                    public.clone(),
                    BTreeMap::new(),
                    vec![tried(vec![throw(true)], None, Some(finally))],
                );
                let held = Site::new(
                    SiteUnit::Library(body.identity()),
                    match cleanup_raises {
                        false => vec![0],
                        true => vec![0, 1, 0],
                    },
                );
                let hook = function(
                    "entry.settle",
                    Signature {
                        params: vec![Param {
                            name: Name::new("value"),
                            ty: Type::Any,
                            optional: false,
                        }],
                        result: Type::Any,
                    },
                    BTreeMap::new(),
                    vec![Stmt::Return {
                        value: Expr::Variable(Name::new("value")),
                    }],
                );
                let export = FunctionExport {
                    function: body.identity(),
                    signature: public.clone(),
                    convention: StartConvention::TsDirect {
                        await_fn: lash_kernel_doc::FunctionTarget::Local(hook.identity()),
                    },
                    asynchronous: true,
                    rest: false,
                };
                (vec![body, hook], export, (Some(held.clone()), Some(held)))
            };
            let program = entry_world(
                functions,
                vec![("entry", export)],
                vec![("entry", public.clone())],
                vec![],
                vec![
                    called(0, "a", vec![]),
                    Stmt::Return {
                        value: Expr::Variable(Name::new("a")),
                    },
                ],
            );
            let mut direct = program.clone();
            let Some((link, instance)) = program
                .instances
                .iter()
                .find(|(link, _)| **link != program.link)
            else {
                panic!("entry law fixture: no defining Link")
            };
            direct.link = *link;
            direct.document = Arc::clone(&program.documents[&instance.document]);
            let error = Datum::Bool(!cleanup_raises);
            for cold in [false, true] {
                let case = format!("python {python}, cleanup raises {cleanup_raises}, cold {cold}");
                for (end, site) in [
                    (
                        started_end(&direct, Target::Entry(Name::new("entry")), cold),
                        &sites.0,
                    ),
                    (started_end(&program, Target::Main, cold), &sites.1),
                ] {
                    let End::Error(lash_kernel_vm::RunError::Uncaught {
                        error: raised,
                        site: at,
                    }) = &end
                    else {
                        panic!("{case}: {end:?}")
                    };
                    assert_eq!((raised, at.as_ref()), (&error, site.as_ref()), "{case}");
                }
            }
        }
    }
}

/// What a run of `program`'s main gives under memory bound `memory`,
/// sliced one unit at a time and, `cold`, sealed and reopened at every
/// slice and at every park, each request settled null: its end and the
/// memory it holds there.
fn bounded_run(program: &Program, memory: u64, cold: bool) -> (End, u64) {
    let limits = Bounds { memory, ..bounds() };
    let pages = Pages::default();
    let start = CellStart {
        owner: PageId([8; 32]),
        target: Target::Main,
        args: vec![],
        session: None,
        modes: BTreeMap::new(),
        host: vec![],
    };
    let reopen = |mut machine: KernelMachine| {
        let saved = machine
            .checkpoint()
            .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
        KernelMachine::open(
            program.clone(),
            limits,
            &saved.checkpoint(),
            Box::new(pages.clone()),
        )
        .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"))
    };
    let mut machine = KernelMachine::start(program.clone(), limits, start, Box::new(pages.clone()))
        .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
    loop {
        match machine
            .run(&mut Passive, 1)
            .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"))
        {
            Step::Ended(end) => return (end, machine.meters().memory),
            Step::Slice if cold => machine = reopen(machine),
            Step::Slice => {}
            Step::Parked(park) => {
                if cold {
                    machine = reopen(machine);
                }
                for Request::Effect(request) in park.requests {
                    machine
                        .deliver(request.wait, Outcome::Completed(Datum::Null))
                        .unwrap_or_else(|error| panic!("entry law fixture: {error:?}"));
                }
            }
        }
    }
}

/// How a bounded run ends, its bound's limit aside: finished with its
/// result, refused naming the bound, function and site, or otherwise.
#[derive(Clone, Debug, PartialEq)]
enum Ending {
    Finished(Datum),
    Refused(String),
    Other(String),
}

fn ending(end: &End) -> Ending {
    match end {
        End::Finished(done) => Ending::Finished(done.result.clone()),
        End::Error(lash_kernel_vm::RunError::Bound(refused)) => Ending::Refused(format!(
            "{:?} {:?} {:?}",
            refused.bound, refused.function, refused.site
        )),
        other => Ending::Other(format!("{other:?}")),
    }
}

/// The error kind a finished run caught, if its result is one.
fn caught(ending: &Ending) -> Option<String> {
    match ending {
        Ending::Finished(Datum::Error(error)) => Some(error.kind.clone()),
        _ => None,
    }
}

/// A parameter of type `ty` named `name`.
fn parameter(name: &str, ty: Type, optional: bool) -> Param {
    Param {
        name: Name::new(name),
        ty,
        optional,
    }
}

/// A call of `callee` with `args`, as an expression.
fn calling(callee: Callee, args: Vec<Expr>) -> Expr {
    Expr::Act(Box::new(lash_kernel_doc::ActExpr {
        kind: lash_kernel_doc::ActKind::Call(callee),
        args,
    }))
}

/// A body taking a `PyArgsList` export's packed arguments that returns
/// `value`.
fn packed_body(value: bool) -> FunctionDefinition {
    function(
        "entry.packed",
        Signature {
            params: ["args", "kwargs", "exc"]
                .into_iter()
                .map(|name| parameter(name, Type::Any, false))
                .collect(),
            result: Type::Any,
        },
        BTreeMap::new(),
        vec![returns(value)],
    )
}

/// K-BND-003, K-DOC-003: a call through an import holds, while it checks
/// and packs its arguments under the export's start convention, only
/// what its caller's frame reserved for it: the validator counts the
/// adaptation's peak in the caller's operand height, which the frame
/// holds for its whole life, and the VM faults an adaptation standing
/// above it (`reserved`), which this law reaches at every bound. Swept
/// over memory bounds, warm and reopened cold at every slice, a run is
/// refused only for a total past its bound (the meter counts the refused
/// demand), finishes holding no more than it, is refused naming the same
/// bound, function and site warm and cold, and a cold run never refuses
/// where the warm run finishes. (A cold run may finish where the warm
/// one is refused: a reopen keeps none of the garbage a loop left.)
///
/// The shapes: `Direct`; zero to four arguments to four optional
/// parameters under `TsDirect` (omitted slots absent), `TsArgsList` with
/// a rest parameter, and `PyArgsList` at every positional cutoff from
/// zero to four, with and without keywords. Keyword names longer than an
/// inline text, a call repeated in a loop, and an adapted call that is
/// another's argument complete the law. A `TsDirect` or `TsArgsList`
/// body raises, which main catches, so the settlement hook, which runs in
/// the entry's frame, is not reached. `PyDirect` packs nothing: its call
/// passes its arguments as any call.
#[test]
fn an_imported_call_holds_only_what_its_frame_reserved() {
    #[derive(Clone, Copy)]
    enum Kind {
        Direct,
        Slots,
        Rest,
        Python(u32),
    }
    #[derive(Clone, Copy, Debug)]
    enum Shape {
        Once,
        Looped,
        Nested,
    }
    let param = |name: &str, optional: bool| parameter(name, Type::Any, optional);
    let yes = || Expr::Literal(Literal::Bool(true));
    let list = |members: Vec<Expr>| Expr::List(members);
    let hook = function(
        "entry.settle",
        Signature {
            params: vec![param("value", false)],
            result: Type::Any,
        },
        BTreeMap::new(),
        vec![Stmt::Return {
            value: Expr::Variable(Name::new("value")),
        }],
    );
    let settle = || lash_kernel_doc::FunctionTarget::Local(hook.identity());
    let names = |names: &[&str]| {
        names
            .iter()
            .map(|name| param(name, false))
            .collect::<Vec<_>>()
    };
    let short = ["k0", "k1", "k2", "k3"];
    let long = [
        "k0",
        "keyword_name_longer_than_any_inline_text_one",
        "keyword_name_longer_than_any_inline_text_two",
        "k3",
    ];
    let mut cases = vec![(Kind::Direct, short, 1, Shape::Once)];
    for argc in 0..=4 {
        cases.push((Kind::Slots, short, argc, Shape::Once));
        cases.push((Kind::Rest, short, argc, Shape::Once));
        for positional in 0..=4 {
            cases.push((Kind::Python(positional), short, argc, Shape::Once));
        }
    }
    cases.extend([
        (Kind::Python(0), long, 2, Shape::Once),
        (Kind::Python(1), long, 3, Shape::Once),
        (Kind::Python(3), short, 4, Shape::Looped),
        (Kind::Python(1), long, 3, Shape::Looped),
        (Kind::Slots, short, 1, Shape::Looped),
        (Kind::Rest, short, 2, Shape::Looped),
        (Kind::Python(1), short, 2, Shape::Nested),
        (Kind::Python(3), short, 4, Shape::Nested),
        (Kind::Python(0), long, 2, Shape::Nested),
    ]);
    // Every case that does not hold, with its first difference.
    let mut failures = Vec::new();
    'cases: for (kind, keys, argc, shape) in cases {
        let case = match kind {
            Kind::Direct => "Direct".to_owned(),
            Kind::Slots => "TsDirect".to_owned(),
            Kind::Rest => "TsArgsList rest".to_owned(),
            Kind::Python(positional) => format!("PyArgsList positional {positional}"),
        };
        let case = format!(
            "{case}, {argc} arguments, {} names, {shape:?}",
            keys[1].len()
        );
        let optional = || keys.iter().map(|key| param(key, true)).collect::<Vec<_>>();
        let (convention, params, block, mut import) = match kind {
            Kind::Direct => (
                StartConvention::Direct,
                names(&["x"]),
                vec![returns(true)],
                names(&["x"]),
            ),
            Kind::Slots => (
                StartConvention::TsDirect { await_fn: settle() },
                optional(),
                vec![Stmt::Throw { value: yes() }],
                optional(),
            ),
            Kind::Rest => (
                StartConvention::TsArgsList {
                    await_fn: settle(),
                    same_fn: settle(),
                },
                names(&["this", "args"]),
                vec![Stmt::Throw { value: yes() }],
                optional(),
            ),
            Kind::Python(positional) => (
                StartConvention::PyArgsList { positional },
                names(&["args", "kwargs", "exc"]),
                vec![returns(true)],
                optional(),
            ),
        };
        let rest = matches!(kind, Kind::Rest);
        if rest && let Some(last) = import.last_mut() {
            last.ty = Type::List(Box::new(Type::Any));
        }
        // A rest parameter's argument is a list.
        let args = (0..argc)
            .map(|at| match rest && at == 3 {
                true => list(vec![]),
                false => yes(),
            })
            .collect::<Vec<_>>();
        let body = function(
            "entry.packed",
            Signature {
                params,
                result: Type::Any,
            },
            BTreeMap::new(),
            block,
        );
        let public = Signature {
            params: import,
            result: Type::Any,
        };
        let export = FunctionExport {
            function: body.identity(),
            signature: public.clone(),
            convention,
            asynchronous: false,
            rest,
        };
        let call = |args: Vec<Expr>| calling(Callee::Import(ImportSlot(0)), args);
        let main = match shape {
            Shape::Once => vec![tried(
                vec![Stmt::Return {
                    value: call(args.clone()),
                }],
                Some(vec![returns(true)]),
                None,
            )],
            Shape::Looped => vec![
                Stmt::For {
                    metered: 0,
                    binding: Name::new("round"),
                    iterable: list(vec![yes(), yes(), yes()]),
                    via: None,
                    body: vec![tried(
                        vec![Stmt::Do {
                            value: call(args.clone()),
                        }],
                        Some(vec![]),
                        None,
                    )],
                    else_block: vec![],
                },
                returns(true),
            ],
            Shape::Nested => {
                let mut outer = args.clone();
                outer[0] = call(args.clone());
                vec![Stmt::Return { value: call(outer) }]
            }
        };
        let program = entry_world(
            vec![body.clone(), hook.clone()],
            vec![("entry", export)],
            vec![("entry", public)],
            vec![],
            main,
        );
        let (mut warm_done, mut cold_done) = (false, false);
        for memory in (0..=8192).step_by(8) {
            let (warm, warm_held) = bounded_run(&program, memory, false);
            let (cold, cold_held) = bounded_run(&program, memory, true);
            let (warm, cold) = (ending(&warm), ending(&cold));
            for (mode, end, held) in [("warm", &warm, warm_held), ("cold", &cold, cold_held)] {
                // A refusal refuses only a demand past the bound; a
                // finished run held no more than it.
                if matches!(end, Ending::Refused(_)) == (held <= memory) {
                    failures.push(format!(
                        "{case}, memory {memory}, {mode}: {end:?} at {held}"
                    ));
                    continue 'cases;
                }
                if let Ending::Finished(result) = end
                    && *result != Datum::Bool(true)
                {
                    failures.push(format!("{case}, memory {memory}, {mode}: {result:?}"));
                    continue 'cases;
                }
            }
            match (&warm, &cold) {
                (Ending::Refused(_), Ending::Refused(_)) if warm != cold => {
                    failures.push(format!(
                        "{case}, memory {memory}: warm {warm:?}, cold {cold:?}"
                    ));
                    continue 'cases;
                }
                (Ending::Finished(_), Ending::Refused(_)) => {
                    failures.push(format!(
                        "{case}, memory {memory}: cold {cold:?} where warm finishes"
                    ));
                    continue 'cases;
                }
                (Ending::Other(_), _) | (_, Ending::Other(_)) => {
                    failures.push(format!(
                        "{case}, memory {memory}: warm {warm:?}, cold {cold:?}"
                    ));
                    continue 'cases;
                }
                _ => {}
            }
            warm_done |= matches!(warm, Ending::Finished(_));
            cold_done |= matches!(cold, Ending::Finished(_));
            if warm_done && cold_done {
                break;
            }
        }
        if !(warm_done && cold_done) {
            failures.push(format!("{case}: no bound up to 8192 completes"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// K-DOC-003, K-BND-003: a call through an import checks its arguments
/// against the import's signature before its adaptation builds anything,
/// as main does, and before the check its caller's frame reserves no more
/// than for the arguments themselves: the keyword names are constants of
/// the caller's code, no operands. So an argument that does not fit
/// raises `function_argument` at exactly the bounds where the adapter
/// before L48 (301789fde0) did, warm and reopened cold at every slice:
/// from 7392 bytes, for every positional cutoff, recorded on that commit
/// (review/l48c-evidence/parent-invalid.json), and below it a refusal of
/// a total past its bound. 128 `Bool` parameters, a null at the first or
/// the last place, cutoffs 0, 124 and 128.
#[test]
fn an_imported_call_checks_its_arguments_before_it_packs_them() {
    const PARAMS: usize = 128;
    const PARENT_FIRST: u64 = 7392;
    let public = Signature {
        params: (0..PARAMS)
            .map(|at| parameter(&format!("k{at}"), Type::Bool, false))
            .collect(),
        result: Type::Any,
    };
    for wrong in [0, PARAMS - 1] {
        for positional in [0, 124, 128] {
            let body = packed_body(false);
            let export = FunctionExport {
                function: body.identity(),
                signature: public.clone(),
                convention: StartConvention::PyArgsList { positional },
                asynchronous: false,
                rest: false,
            };
            let args = (0..PARAMS)
                .map(|at| match at == wrong {
                    true => null(),
                    false => Expr::Literal(Literal::Bool(true)),
                })
                .collect();
            let program = entry_world(
                vec![body],
                vec![("entry", export)],
                vec![("entry", public.clone())],
                vec![],
                vec![tried(
                    vec![Stmt::Return {
                        value: calling(Callee::Import(ImportSlot(0)), args),
                    }],
                    Some(vec![Stmt::Return {
                        value: Expr::Variable(Name::new("caught")),
                    }]),
                    None,
                )],
            );
            for cold in [false, true] {
                for memory in (8..=10304).step_by(8) {
                    let (end, held) = bounded_run(&program, memory, cold);
                    let end = ending(&end);
                    let case = format!("null at {wrong}, positional {positional}, cold {cold}");
                    match memory >= PARENT_FIRST {
                        true => assert_eq!(
                            caught(&end).as_deref(),
                            Some("function_argument"),
                            "{case}, memory {memory}: {end:?}"
                        ),
                        false => assert!(
                            matches!(end, Ending::Refused(_)) && held > memory,
                            "{case}, memory {memory}: {end:?} holding {held}"
                        ),
                    }
                }
            }
        }
    }
}

/// A program whose main calls, or spawns and joins, an imported export
/// of `count` `Bool` parameters with `count` arguments, all `true` but a
/// null in the last place when `wrong`, inside a `try` that returns what
/// it caught. The export is `Direct` when `direct`, its body taking the
/// parameters, else `PyArgsList` packing every argument by name.
fn counted(count: usize, direct: bool, wrong: bool, spawned: bool) -> Program {
    let public = Signature {
        params: (0..count)
            .map(|at| parameter(&format!("k{at}"), Type::Bool, false))
            .collect(),
        result: Type::Any,
    };
    let (body, convention) = match direct {
        true => (
            function(
                "entry.direct",
                public.clone(),
                BTreeMap::new(),
                vec![returns(true)],
            ),
            StartConvention::Direct,
        ),
        false => (
            packed_body(true),
            StartConvention::PyArgsList { positional: 0 },
        ),
    };
    let export = FunctionExport {
        function: body.identity(),
        signature: public.clone(),
        convention,
        asynchronous: false,
        rest: false,
    };
    let argument = |at: usize| match wrong && at == count - 1 {
        true => Literal::Null,
        false => Literal::Bool(true),
    };
    let main = match spawned {
        false => vec![Stmt::Return {
            value: calling(
                Callee::Import(ImportSlot(0)),
                (0..count).map(|at| Expr::Literal(argument(at))).collect(),
            ),
        }],
        true => vec![
            Stmt::Let {
                name: Name::new("child"),
                value: Rhs::Action(Action::Spawn {
                    callee: Callee::Import(ImportSlot(0)),
                    args: (0..count).map(|at| Atom::Literal(argument(at))).collect(),
                }),
            },
            Stmt::Let {
                name: Name::new("answer"),
                value: Rhs::Action(Action::Join {
                    task: Atom::Variable(Name::new("child")),
                }),
            },
            Stmt::Return {
                value: Expr::Variable(Name::new("answer")),
            },
        ],
    };
    entry_world(
        vec![body],
        vec![("entry", export)],
        vec![("entry", public)],
        vec![],
        vec![tried(
            main,
            Some(vec![Stmt::Return {
                value: Expr::Variable(Name::new("caught")),
            }]),
            None,
        )],
    )
}

/// K-DOC-003: a call through an import passes every argument at every
/// count up to the core's limit of 65,535, as a plain call does: the
/// keyword names it packs are no operands, so no count of them is added
/// to the arguments' and wrapped (a site counting 65,536 operands held
/// none). A `Direct` export, whose body takes every parameter, and a
/// `PyArgsList` one packing every argument by name, called and spawned,
/// warm and cold: every argument `true` finishes `true`, and a null in
/// the last place raises `function_argument`, which only a check of
/// every argument finds.
#[test]
fn an_imported_call_passes_every_argument_at_every_count() {
    for count in [32768, usize::from(u16::MAX)] {
        for direct in [false, true] {
            for wrong in [false, true] {
                for spawned in [false, true] {
                    let program = counted(count, direct, wrong, spawned);
                    for cold in [false, true] {
                        let (end, _) = bounded_run(&program, 1 << 30, cold);
                        let end = ending(&end);
                        let case = format!(
                            "{count} arguments, direct {direct}, spawned {spawned}, cold {cold}"
                        );
                        match wrong {
                            false => {
                                assert_eq!(end, Ending::Finished(Datum::Bool(true)), "{case}")
                            }
                            true => assert_eq!(
                                caught(&end).as_deref(),
                                Some("function_argument"),
                                "{case}: {end:?}"
                            ),
                        }
                    }
                }
            }
        }
    }
}

/// The bytecode core holds at most 65,535 arguments of one call or
/// spawn and parameters of one function (a documented limit, which main
/// does not have). A document past it is refused as one the core cannot
/// compile, a typed start error naming the count and the limit, never
/// wrapped and never a panic: a `Direct` export of 65,536 parameters and
/// a `PyArgsList` one, called or spawned with 65,536 arguments.
#[test]
fn a_count_past_the_cores_limit_is_refused_as_uncompilable() {
    for direct in [true, false] {
        for spawned in [false, true] {
            let program = counted(65536, direct, false, spawned);
            let start = CellStart {
                owner: PageId([8; 32]),
                target: Target::Main,
                args: vec![],
                session: None,
                modes: BTreeMap::new(),
                host: vec![],
            };
            let case = format!("direct {direct}, spawned {spawned}");
            match KernelMachine::start(program, bounds(), start, Box::new(Pages::default())) {
                Err(lash_kernel_vm::StartError::Linked { problem }) => assert!(
                    problem.contains("65536") && problem.contains("limit of 65535"),
                    "{case}: {problem}"
                ),
                Err(other) => panic!("{case}: {other:?}"),
                Ok(_) => panic!("{case}: admitted"),
            }
        }
    }
}

/// K-BND-003, K-DOC-003: a keyword argument whose expression parks,
/// reopened cold at every slice and at the park, ends as it does warm at
/// every memory bound, holding the same at a refusal: the arguments
/// stand on the operand stack until the call checks and packs them in
/// one operation, so no collection of them is saved and reloaded across
/// the park. The keyword's name is longer than an inline text.
#[test]
fn a_parked_keyword_argument_ends_alike_warm_and_cold() {
    let long = "keyword_name_longer_than_the_inline_text_limit";
    let public = Signature {
        params: vec![
            parameter("a", Type::Any, false),
            parameter(long, Type::Any, false),
        ],
        result: Type::Any,
    };
    let body = packed_body(true);
    let pause = function(
        "entry.pause",
        signature(vec![]),
        pause_import(),
        vec![perform(), Stmt::Return { value: null() }],
    );
    let packed = FunctionExport {
        function: body.identity(),
        signature: public.clone(),
        convention: StartConvention::PyArgsList { positional: 1 },
        asynchronous: false,
        rest: false,
    };
    let paused = FunctionExport {
        function: pause.identity(),
        signature: signature(vec![]),
        convention: StartConvention::Direct,
        asynchronous: false,
        rest: false,
    };
    let call = calling(
        Callee::Import(ImportSlot(0)),
        vec![
            Expr::Literal(Literal::Bool(true)),
            calling(Callee::Import(ImportSlot(1)), vec![]),
        ],
    );
    let program = entry_world(
        vec![body, pause],
        vec![("entry", packed), ("pause", paused)],
        vec![("entry", public), ("pause", signature(vec![]))],
        vec![],
        vec![Stmt::Return { value: call }],
    );
    for memory in (0..=8192).step_by(8) {
        let (warm, warm_held) = bounded_run(&program, memory, false);
        let (cold, cold_held) = bounded_run(&program, memory, true);
        let (warm, cold) = (ending(&warm), ending(&cold));
        assert_eq!(warm, cold, "memory {memory}");
        match warm {
            Ending::Finished(result) => {
                assert_eq!(result, Datum::Bool(true));
                return;
            }
            Ending::Refused(_) => {
                assert_eq!(warm_held, cold_held, "memory {memory}");
                assert!(
                    warm_held > memory,
                    "memory {memory}: refused at {warm_held}"
                );
            }
            Ending::Other(other) => panic!("memory {memory}: {other}"),
        }
    }
    panic!("no bound up to 8192 completes");
}

/// K-BND-003: a call through an import of a `PyArgsList` export keys its
/// map by names that are constants of the caller's code, read after the
/// check, so a long name allocates nothing at the call: with names longer than an inline text
/// the run ends as with short ones at every memory bound, warm and
/// reopened cold, holding the same.
#[test]
fn a_long_keyword_name_allocates_nothing_at_the_call() {
    let program = |names: [&str; 3]| {
        let public = Signature {
            params: names
                .iter()
                .map(|name| parameter(name, Type::Any, false))
                .collect(),
            result: Type::Any,
        };
        let body = packed_body(true);
        let export = FunctionExport {
            function: body.identity(),
            signature: public.clone(),
            convention: StartConvention::PyArgsList { positional: 1 },
            asynchronous: false,
            rest: false,
        };
        let yes = || Expr::Literal(Literal::Bool(true));
        entry_world(
            vec![body],
            vec![("entry", export)],
            vec![("entry", public)],
            vec![],
            vec![Stmt::Return {
                value: calling(Callee::Import(ImportSlot(0)), vec![yes(), yes(), yes()]),
            }],
        )
    };
    let short = program(["k0", "k1", "k2"]);
    let long = program([
        "k0",
        "keyword_name_longer_than_any_inline_text_one",
        "keyword_name_longer_than_any_inline_text_two",
    ]);
    for memory in (0..=8192).step_by(8) {
        let mut finished = true;
        for cold in [false, true] {
            let (end, held) = bounded_run(&short, memory, cold);
            let (long_end, long_held) = bounded_run(&long, memory, cold);
            let (end, long_end) = (ending(&end), ending(&long_end));
            assert_eq!(long_end, end, "memory {memory}, cold {cold}");
            if !matches!(end, Ending::Finished(_)) {
                assert_eq!(long_held, held, "memory {memory}, cold {cold}");
                finished = false;
            }
        }
        if finished {
            return;
        }
    }
    panic!("no bound up to 8192 completes");
}
