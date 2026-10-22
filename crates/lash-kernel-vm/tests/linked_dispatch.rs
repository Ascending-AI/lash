//! L-DISPATCH: imported code and escaped callbacks retain their defining Link.
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{
    Action, Atom, Callee, Closure, Datum, Document, ErrorDatum, Expr, FunctionBody,
    FunctionDefinition, FunctionExport, FunctionRegistry, Handle, Implementation, Literal,
    Metering, NumberPolicy, Rhs, StartConvention, Stmt,
};

use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{
    Bounds, CellStart, Charge, End, Host, KernelMachine, Machine, MemoryMeter, Outcome,
    PreparedLibrary, Program, Request, Step, Target,
};
use lash_vocabulary::package::{Fill, Link};
use lash_vocabulary::{
    ContractId, Convention, ExportName, FunctionId, FunctionName, Import, ImportKind, ImportSlot,
    Name, OperationRequirement, OperationUse, Param, QualifiedName, Signature, Type,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::Arc,
};

#[derive(Clone, Default)]
struct Pages(Rc<RefCell<BTreeMap<PageId, PageAnswer>>>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|p| {
                self.0
                    .borrow()
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
struct Passive;
impl Host for Passive {
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("unexpected read")
    }
    fn print(&mut self, _: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
fn signature(params: Vec<Param>) -> Signature {
    Signature {
        params,
        result: Type::Null,
    }
}
fn null() -> Expr {
    Expr::Literal(Literal::Null)
}
fn perform() -> Stmt {
    Stmt::Let {
        name: Name::new("performed"),
        value: Rhs::Action(Action::Perform {
            slot: ImportSlot(0),
            args: vec![],
            result: Type::Null,
            then_yield: true,
        }),
    }
}
fn function(
    name: &str,
    signature: Signature,
    imports: BTreeMap<ImportSlot, Import>,
    block: Vec<Stmt>,
) -> FunctionDefinition {
    FunctionDefinition {
        kernel: lash_kernel_doc::KERNEL_VERSION,
        name: FunctionName::new(name)
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
        signature,
        errors: BTreeSet::new(),
        guard: None,
        implementation: Implementation::Guest {
            body: FunctionBody {
                functions: BTreeMap::new(),
                imports,
                block,
            },
            metering: Metering::Statements,
        },
        native_version: 1,
    }
}
fn document(
    imports: BTreeMap<ImportSlot, Import>,
    functions: BTreeMap<FunctionId, FunctionDefinition>,
    main: Option<Vec<Stmt>>,
) -> Document {
    Document {
        imports,
        functions,
        main,
        exports: BTreeMap::new(),
        requires: lash_vocabulary::claims::NodeRequirements {
            kernels: BTreeSet::from([lash_kernel_doc::KERNEL_VERSION]),
            ..Default::default()
        },
        numbers: NumberPolicy::BySpelling,
        private_bindings: BTreeSet::new(),
    }
}
fn bounds() -> Bounds {
    Bounds {
        memory_meter: MemoryMeter::On,
        charge: Charge::Exact,
        charge_limit: 100_000,
        memory: 64 << 20,
        call_depth: 100,
        live_tasks: 10,
        requests_per_park: 10,
        join_members: 10,
    }
}
fn request(machine: &mut KernelMachine) -> lash_kernel_vm::EffectRequest {
    loop {
        match machine
            .run(&mut Passive, 100_000)
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        {
            Step::Slice => {}
            Step::Parked(park) => {
                let [Request::Effect(request)] = park.requests.as_slice() else {
                    panic!("expected one request: {park:?}")
                };
                return request.clone();
            }
            other => panic!("expected park: {other:?}"),
        }
    }
}

#[test]
fn imported_body_and_callback_keep_their_links_after_cold_restore() {
    let operation = Import {
        name: QualifiedName {
            package: lash_vocabulary::Vec1::new(
                lash_vocabulary::Segment::new("service")
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            ),
            member: lash_vocabulary::Segment::new("read")
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            type_member: None,
        },
        kind: ImportKind::Operation {
            signature: signature(vec![]),
        },
    };
    let operation_imports = BTreeMap::from([(ImportSlot(0), operation)]);
    let callback = function(
        "test.callback",
        signature(vec![]),
        operation_imports.clone(),
        vec![perform(), Stmt::Return { value: null() }],
    );
    let callback_id = callback.identity();
    let body_signature = signature(vec![Param {
        name: Name::new("callback"),
        ty: Type::Any,
        optional: false,
    }]);
    let body = function(
        "test.body",
        body_signature.clone(),
        operation_imports.clone(),
        vec![
            perform(),
            Stmt::Let {
                name: Name::new("called"),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Value(Name::new("callback")),
                    args: vec![],
                }),
            },
            Stmt::Return { value: null() },
        ],
    );
    let body_id = body.identity();
    let mut imported = document(
        operation_imports.clone(),
        BTreeMap::from([(body_id, body.clone())]),
        None,
    );
    imported.exports.insert(
        Name::new("run"),
        lash_kernel_doc::Export::Function(FunctionExport {
            function: body_id,
            signature: body_signature.clone(),
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    let mut caller_imports = operation_imports;
    caller_imports.insert(
        ImportSlot(1),
        Import {
            name: QualifiedName {
                package: lash_vocabulary::Vec1::new(
                    lash_vocabulary::Segment::new("library")
                        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                ),
                member: lash_vocabulary::Segment::new("run")
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                type_member: None,
            },
            kind: ImportKind::Code {
                signature: body_signature,
                convention: Convention::Direct,
            },
        },
    );
    let caller = document(
        caller_imports,
        BTreeMap::from([(callback_id, callback.clone())]),
        Some(vec![
            Stmt::Let {
                name: Name::new("callback"),
                value: Rhs::Expr(Expr::Closure(Box::new(Closure {
                    function: callback_id,
                    presentation: None,
                }))),
            },
            Stmt::Let {
                name: Name::new("called"),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Import(ImportSlot(1)),
                    args: vec![Atom::Variable(Name::new("callback"))],
                }),
            },
            Stmt::Return { value: null() },
        ]),
    );
    let requirement = || {
        Fill::Operation(OperationRequirement::new(
            signature(vec![]),
            ContractId::new("read.v1"),
        ))
    };
    let imported_link = Link {
        document: imported.identity(),
        fills: BTreeMap::from([(ImportSlot(0), requirement())]),
    };
    let b = imported_link.identity();
    let caller_link = Link {
        document: caller.identity(),
        fills: BTreeMap::from([
            (ImportSlot(0), requirement()),
            (
                ImportSlot(1),
                Fill::Export {
                    link: b,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            ),
        ]),
    };
    let a = caller_link.identity();
    let registry = FunctionRegistry::new();
    let prepared = PreparedLibrary::new(Arc::new(registry));
    let program = Program::prepare(
        a,
        BTreeMap::from([
            (caller.identity(), Arc::new(caller)),
            (imported.identity(), Arc::new(imported)),
        ]),
        &BTreeMap::from([(a, caller_link), (b, imported_link)]),
        PageId([9; 32]),
        prepared,
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
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
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    let first = request(&mut machine);
    assert_eq!(
        first.use_,
        OperationUse {
            link: b,
            slot: ImportSlot(0)
        }
    );
    let checkpoint = machine
        .checkpoint()
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        .checkpoint();
    drop(machine);
    let mut machine = KernelMachine::open(program, bounds(), &checkpoint, Box::new(pages))
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    machine
        .deliver(first.wait, Outcome::Completed(Datum::Null))
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    let second = request(&mut machine);
    assert_eq!(
        second.use_,
        OperationUse {
            link: a,
            slot: ImportSlot(0)
        }
    );
    machine
        .deliver(second.wait, Outcome::Completed(Datum::Null))
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    loop {
        match machine
            .run(&mut Passive, 100_000)
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        {
            Step::Slice => {}
            Step::Ended(End::Finished(done)) => {
                assert_eq!(done.result, Datum::Null);
                break;
            }
            other => panic!("expected completion: {other:?}"),
        }
    }
}

/// A retained callable starts with its supplied captures; cold restore keeps them.
#[test]
fn callable_start_installs_the_captured_environment() {
    let imports = BTreeMap::from([(
        ImportSlot(0),
        Import {
            name: QualifiedName {
                package: lash_vocabulary::PackagePath::parse("service")
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                member: lash_vocabulary::Segment::new("read")
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                type_member: None,
            },
            kind: ImportKind::Operation {
                signature: signature(vec![]),
            },
        },
    )]);
    let definition = function(
        "test.captured",
        Signature {
            params: vec![],
            result: Type::Bool,
        },
        imports.clone(),
        vec![
            perform(),
            Stmt::Return {
                value: Expr::Variable(Name::new("captured")),
            },
        ],
    );
    let function = definition.identity();
    let document = document(
        imports,
        BTreeMap::from([(function, definition.clone())]),
        None,
    );
    let document_id = document.identity();
    let link = Link {
        document: document_id,
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Operation(OperationRequirement::new(
                signature(vec![]),
                ContractId::new("read.v1"),
            )),
        )]),
    };
    let link_id = link.identity();
    let registry = FunctionRegistry::new();
    let mut program = Program::prepare(
        link_id,
        BTreeMap::from([(document_id, Arc::new(document))]),
        &BTreeMap::from([(link_id, link)]),
        PageId([9; 32]),
        PreparedLibrary::new(Arc::new(registry)),
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    program.functions.insert(function);
    let target = Target::Callable(Box::new(lash_kernel_doc::FunctionValue {
        callable: lash_vocabulary::CallableId {
            link: link_id,
            function,
        },
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        creation: None,
        token: None,
        captures: BTreeMap::from([(
            Name::new("captured"),
            lash_kernel_doc::CaptureValue::Data(Datum::Bool(true)),
        )]),
        signature: definition.signature,
        convention: StartConvention::Direct,
        captions: lash_kernel_doc::Captions::empty(document_id),
    }));
    let pages = Pages::default();
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds(),
        CellStart {
            owner: PageId([8; 32]),
            target,
            args: vec![],
            session: None,
            modes: BTreeMap::new(),
            host: vec![],
        },
        Box::new(pages.clone()),
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    let asked = request(&mut machine);
    let checkpoint = machine
        .checkpoint()
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        .checkpoint();
    drop(machine);
    let mut cold = KernelMachine::open(program, bounds(), &checkpoint, Box::new(pages))
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    cold.deliver(asked.wait, Outcome::Completed(Datum::Null))
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    loop {
        match cold
            .run(&mut Passive, 100_000)
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        {
            Step::Slice => {}
            Step::Ended(End::Finished(done)) => {
                assert_eq!(done.result, Datum::Bool(true));
                break;
            }
            other => panic!("expected completion: {other:?}"),
        }
    }
}

/// L-DISPATCH: imported public arguments use the callee's ABI, and its
/// settlement helper retains the same Link after a cold resume.
#[test]
fn imported_args_list_is_settled_under_its_defining_link_after_cold_restore() {
    let operation = Import {
        name: QualifiedName {
            package: lash_vocabulary::PackagePath::parse("service")
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            member: lash_vocabulary::Segment::new("read")
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            type_member: None,
        },
        kind: ImportKind::Operation {
            signature: signature(vec![]),
        },
    };
    let imports = BTreeMap::from([(ImportSlot(0), operation)]);
    let param = |name: &str| Param {
        name: Name::new(name),
        ty: Type::Any,
        optional: false,
    };
    let settle = function(
        "test.settle",
        Signature {
            params: vec![param("value")],
            result: Type::Any,
        },
        imports.clone(),
        vec![
            perform(),
            Stmt::Return {
                value: Expr::Variable(Name::new("value")),
            },
        ],
    );
    let settle_id = settle.identity();
    let body = function(
        "test.packed",
        Signature {
            params: vec![param("this"), param("args")],
            result: Type::Any,
        },
        imports.clone(),
        vec![
            perform(),
            Stmt::Return {
                value: Expr::Member(Box::new(lash_kernel_doc::Member::Index {
                    target: Expr::Variable(Name::new("args")),
                    index: Expr::Literal(Literal::Int(0.into())),
                })),
            },
        ],
    );
    let body_id = body.identity();
    let public = Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty: Type::Bool,
            optional: false,
        }],
        result: Type::Bool,
    };
    let mut imported = document(
        imports,
        BTreeMap::from([(body_id, body), (settle_id, settle)]),
        None,
    );
    imported.exports.insert(
        Name::new("packed"),
        lash_kernel_doc::Export::Function(FunctionExport {
            function: body_id,
            signature: public.clone(),
            convention: StartConvention::TsArgsList {
                await_fn: lash_kernel_doc::FunctionTarget::Local(settle_id),
                same_fn: lash_kernel_doc::FunctionTarget::Local(settle_id),
            },
            asynchronous: true,
            rest: false,
        }),
    );
    let imported_link = Link {
        document: imported.identity(),
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Operation(OperationRequirement::new(
                signature(vec![]),
                ContractId::new("read.v1"),
            )),
        )]),
    };
    let b = imported_link.identity();
    let caller = document(
        BTreeMap::from([(
            ImportSlot(0),
            Import {
                name: QualifiedName {
                    package: lash_vocabulary::PackagePath::parse("library")
                        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                    member: lash_vocabulary::Segment::new("packed")
                        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                    type_member: None,
                },
                kind: ImportKind::Code {
                    signature: public,
                    convention: Convention::TsArgsList,
                },
            },
        )]),
        BTreeMap::new(),
        Some(vec![
            Stmt::Let {
                name: Name::new("result"),
                value: Rhs::Expr(Expr::Let {
                    name: Name::new("first"),
                    value: Box::new(Expr::Act(Box::new(lash_kernel_doc::ActExpr {
                        kind: lash_kernel_doc::ActKind::Call(Callee::Import(ImportSlot(0))),
                        args: vec![Expr::Literal(Literal::Bool(true))],
                    }))),
                    body: Box::new(Expr::Act(Box::new(lash_kernel_doc::ActExpr {
                        kind: lash_kernel_doc::ActKind::Call(Callee::Import(ImportSlot(0))),
                        args: vec![Expr::Variable(Name::new("first"))],
                    }))),
                }),
            },
            Stmt::Return {
                value: Expr::Variable(Name::new("result")),
            },
        ]),
    );
    let caller_link = Link {
        document: caller.identity(),
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Export {
                link: b,
                export: ExportName {
                    name: Name::new("packed"),
                    member: None,
                },
            },
        )]),
    };
    let a = caller_link.identity();
    let program = Program::prepare(
        a,
        BTreeMap::from([
            (caller.identity(), Arc::new(caller)),
            (imported.identity(), Arc::new(imported)),
        ]),
        &BTreeMap::from([(a, caller_link), (b, imported_link)]),
        PageId([9; 32]),
        PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    let pages = Pages::default();
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds(),
        CellStart {
            owner: PageId([8; 32]),
            target: Target::Main,
            args: vec![],
            session: None,
            modes: BTreeMap::new(),
            host: vec![],
        },
        Box::new(pages.clone()),
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    for _ in 0..4 {
        let asked = request(&mut machine);
        assert_eq!(
            asked.use_,
            OperationUse {
                link: b,
                slot: ImportSlot(0)
            }
        );
        let checkpoint = machine
            .checkpoint()
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
            .checkpoint();
        drop(machine);
        machine = KernelMachine::open(
            program.clone(),
            bounds(),
            &checkpoint,
            Box::new(pages.clone()),
        )
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
        machine
            .deliver(asked.wait, Outcome::Completed(Datum::Null))
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    }
    loop {
        match machine
            .run(&mut Passive, 100_000)
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        {
            Step::Slice => {}
            Step::Ended(End::Finished(done)) => {
                assert_eq!(done.result, Datum::Bool(true));
                break;
            }
            other => panic!("expected settled completion: {other:?}"),
        }
    }
}

/// An immediate library export's public result failure belongs to the spawned
/// child. The spawner receives its handle and observes that error at join.
#[test]
fn spawned_immediate_export_mismatch_keeps_child_identity() {
    struct Null;
    impl lash_kernel_doc::NativeFunction for Null {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Ok(lash_kernel_doc::Value::Null)
        }
    }
    for native in [false, true] {
        let mut definition = function(
            "test.child",
            Signature {
                params: vec![],
                result: Type::Any,
            },
            BTreeMap::new(),
            vec![Stmt::Return { value: null() }],
        );
        let body = definition.body().expect("guest body").clone();
        let charge = lash_kernel_doc::Formula::Constant(0);
        definition.implementation = if native {
            Implementation::Both { body, charge }
        } else {
            Implementation::Guest {
                body,
                metering: Metering::Formula(charge),
            }
        };
        let function = definition.identity();
        let public = Signature {
            params: vec![],
            result: Type::Bool,
        };
        let mut child = document(
            BTreeMap::new(),
            BTreeMap::from([(function, definition.clone())]),
            None,
        );
        child.exports.insert(
            Name::new("child"),
            lash_kernel_doc::Export::Function(FunctionExport {
                function,
                signature: public.clone(),
                convention: StartConvention::Direct,
                asynchronous: true,
                rest: false,
            }),
        );
        let child_id = child.identity();
        let child_link = Link {
            document: child_id,
            fills: BTreeMap::new(),
        };
        let child_link_id = child_link.identity();
        let root = document(
            BTreeMap::from([(
                ImportSlot(0),
                Import {
                    name: QualifiedName {
                        package: lash_vocabulary::PackagePath::parse("worker").unwrap(),
                        member: lash_vocabulary::Segment::new("child").unwrap(),
                        type_member: None,
                    },
                    kind: ImportKind::Code {
                        signature: public,
                        convention: Convention::Direct,
                    },
                },
            )]),
            BTreeMap::new(),
            Some(vec![
                Stmt::Let {
                    name: Name::new("child"),
                    value: Rhs::Action(Action::Spawn {
                        callee: Callee::Import(ImportSlot(0)),
                        args: vec![],
                    }),
                },
                Stmt::Try(lash_kernel_doc::TryStmt {
                    body: vec![Stmt::Let {
                        name: Name::new("answer"),
                        value: Rhs::Action(Action::Join {
                            task: Atom::Variable(Name::new("child")),
                        }),
                    }],
                    catch: Some(lash_kernel_doc::Catch {
                        binding: Name::new("error"),
                        body: vec![Stmt::Return {
                            value: Expr::Variable(Name::new("error")),
                        }],
                    }),
                    finally: None,
                }),
                Stmt::Return { value: null() },
            ]),
        );
        let root_id = root.identity();
        let link = Link {
            document: root_id,
            fills: BTreeMap::from([(
                ImportSlot(0),
                Fill::Export {
                    link: child_link_id,
                    export: ExportName {
                        name: Name::new("child"),
                        member: None,
                    },
                },
            )]),
        };
        let link_id = link.identity();
        let mut registry = FunctionRegistry::new();
        registry
            .register(
                definition,
                native.then(|| Arc::new(Null) as Arc<dyn lash_kernel_doc::NativeFunction>),
            )
            .expect("child definition");
        let program = Program::prepare(
            link_id,
            BTreeMap::from([(root_id, Arc::new(root)), (child_id, Arc::new(child))]),
            &BTreeMap::from([(link_id, link), (child_link_id, child_link)]),
            PageId([9; 32]),
            PreparedLibrary::new(Arc::new(registry)),
        )
        .expect("linked child");
        let mut machine = KernelMachine::start(
            program,
            bounds(),
            CellStart {
                owner: PageId([8; 32]),
                target: Target::Main,
                args: vec![],
                session: None,
                modes: BTreeMap::new(),
                host: vec![],
            },
            Box::new(Pages::default()),
        )
        .expect("spawn program");
        loop {
            match machine
                .run(&mut Passive, 100_000)
                .expect("child failure stays in guest")
            {
                Step::Slice => {}
                Step::Ended(End::Finished(done)) => {
                    let Datum::Error(error) = done.result else {
                        panic!("join did not observe child error: {done:?}")
                    };
                    assert_eq!(error.kind, "entry_type_mismatch");
                    assert_eq!(
                        error.data,
                        lash_kernel_doc::diagnostic_datum(&lash_vocabulary::EntryTypeMismatch {
                            callable: lash_vocabulary::CallableId {
                                link: child_link_id,
                                function
                            },
                            mismatch: lash_vocabulary::TypeMismatch::new(Type::Bool, Type::Null)
                                .within("output"),
                        })
                        .expect("typed child mismatch")
                    );
                    break;
                }
                other => {
                    panic!("child result failure escaped spawn or left a live child: {other:?}")
                }
            }
        }
    }
}

/// K-HOST and K-CHG: a synchronous native export needs no Main or guest
/// body, and its declared formula is the charge on that path.
#[test]
fn native_export_starts_without_main_and_pays_its_formula() {
    struct Answer(bool);
    impl lash_kernel_doc::NativeFunction for Answer {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Ok(lash_kernel_doc::Value::Bool(self.0))
        }
    }
    for kind in 0..3 {
        let body = FunctionBody {
            functions: BTreeMap::new(),
            imports: BTreeMap::new(),
            block: vec![Stmt::Return {
                value: Expr::Literal(Literal::Bool(true)),
            }],
        };
        let charge = lash_kernel_doc::Formula::Constant(7);
        let implementation = match kind {
            0 => Implementation::Native { charge },
            1 => Implementation::Guest {
                body,
                metering: Metering::Formula(charge),
            },
            _ => Implementation::Both { body, charge },
        };
        let definition = FunctionDefinition {
            kernel: lash_kernel_doc::KERNEL_VERSION,
            name: FunctionName::new("test.native")
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            signature: Signature {
                params: vec![],
                result: Type::Bool,
            },
            errors: BTreeSet::new(),
            guard: None,
            implementation,
            native_version: 1,
        };
        let function = definition.identity();
        let mut document = document(
            BTreeMap::new(),
            BTreeMap::from([(function, definition.clone())]),
            None,
        );
        if kind == 0 {
            document.requires.natives.insert(function);
        }
        document.exports.insert(
            Name::new("answer"),
            lash_kernel_doc::Export::Function(FunctionExport {
                function,
                signature: definition.signature.clone(),
                convention: StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
        let link = Link {
            document: document.identity(),
            fills: BTreeMap::new(),
        };
        let link_id = link.identity();
        let mut registry = FunctionRegistry::new();
        registry
            .register(
                definition,
                (kind != 1).then(|| {
                    Arc::new(Answer(kind == 0)) as Arc<dyn lash_kernel_doc::NativeFunction>
                }),
            )
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
        let program = Program::prepare(
            link_id,
            BTreeMap::from([(document.identity(), Arc::new(document))]),
            &BTreeMap::from([(link_id, link)]),
            PageId([9; 32]),
            PreparedLibrary::new(Arc::new(registry)),
        )
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
        let mut machine = KernelMachine::start(
            program,
            bounds(),
            CellStart {
                owner: PageId([8; 32]),
                target: Target::Entry(Name::new("answer")),
                args: vec![],
                session: None,
                modes: BTreeMap::new(),
                host: vec![],
            },
            Box::new(Pages::default()),
        )
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
        loop {
            match machine
                .run(&mut Passive, 100_000)
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
            {
                Step::Slice => {}
                Step::Ended(End::Finished(done)) => {
                    assert_eq!(done.result, Datum::Bool(true));
                    break;
                }
                other => panic!("export did not complete: {other:?}"),
            }
        }
        assert_eq!(machine.meters().charged, Some(7));
    }
}

/// Native starts obey their defining convention, including a settlement hook that parks.
#[test]
fn native_export_settlement_runs_the_defining_hook_after_cold_restore() {
    struct Answer;
    impl lash_kernel_doc::NativeFunction for Answer {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Ok(lash_kernel_doc::Value::Bool(false))
        }
    }
    for kind in 0..3 {
        let imports = BTreeMap::from([(
            ImportSlot(0),
            Import {
                name: QualifiedName {
                    package: lash_vocabulary::Vec1::new(
                        lash_vocabulary::Segment::new("service")
                            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                    ),
                    member: lash_vocabulary::Segment::new("settle")
                        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                    type_member: None,
                },
                kind: ImportKind::Operation {
                    signature: signature(vec![]),
                },
            },
        )]);
        let hook = function(
            "test.settle",
            Signature {
                params: if kind == 2 {
                    vec![]
                } else {
                    vec![Param {
                        name: Name::new("value"),
                        ty: Type::Any,
                        optional: false,
                    }]
                },
                result: Type::Bool,
            },
            imports.clone(),
            vec![
                perform(),
                Stmt::Return {
                    value: Expr::Literal(Literal::Bool(true)),
                },
            ],
        );
        let hook_id = hook.identity();
        let convention = match kind {
            0 => StartConvention::TsDirect {
                await_fn: lash_kernel_doc::FunctionTarget::Local(hook_id),
            },
            1 => StartConvention::TsArgsList {
                await_fn: lash_kernel_doc::FunctionTarget::Local(hook_id),
                same_fn: lash_kernel_doc::FunctionTarget::Local(hook_id),
            },
            _ => StartConvention::PyDirect {
                cleanup: lash_kernel_doc::FunctionTarget::Local(hook_id),
            },
        };
        let definition = FunctionDefinition {
            kernel: lash_kernel_doc::KERNEL_VERSION,
            name: FunctionName::new("test.answer")
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            signature: Signature {
                params: if kind == 1 {
                    vec![
                        Param {
                            name: Name::new("context"),
                            ty: Type::Any,
                            optional: false,
                        },
                        Param {
                            name: Name::new("args"),
                            ty: Type::Any,
                            optional: false,
                        },
                    ]
                } else {
                    vec![]
                },
                result: Type::Bool,
            },
            errors: BTreeSet::new(),
            guard: None,
            native_version: 1,
            implementation: Implementation::Native {
                charge: lash_kernel_doc::Formula::Constant(7),
            },
        };
        let id = definition.identity();
        let mut document = document(
            imports,
            BTreeMap::from([(id, definition.clone()), (hook_id, hook)]),
            None,
        );
        document.requires.natives.insert(id);
        document.exports.insert(
            Name::new("answer"),
            lash_kernel_doc::Export::Function(FunctionExport {
                function: id,
                signature: Signature {
                    params: vec![],
                    result: Type::Bool,
                },
                convention,
                asynchronous: true,
                rest: false,
            }),
        );
        let link = Link {
            document: document.identity(),
            fills: BTreeMap::from([(
                ImportSlot(0),
                Fill::Operation(OperationRequirement::new(
                    signature(vec![]),
                    ContractId::new("1"),
                )),
            )]),
        };
        let mut registry = FunctionRegistry::new();
        registry
            .register(definition, Some(Arc::new(Answer)))
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
        let links = BTreeMap::from([(link.identity(), link.clone())]);
        let program = Program::prepare(
            link.identity(),
            BTreeMap::from([(document.identity(), Arc::new(document))]),
            &BTreeMap::from([(link.identity(), link)]),
            PageId([9; 32]),
            PreparedLibrary::new(Arc::new(registry)),
        )
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
        for via in 0..3 {
            let (program, target) = if via == 0 {
                (program.clone(), Target::Entry(Name::new("answer")))
            } else {
                let callee = Callee::Import(ImportSlot(0));
                let mut main = vec![Stmt::Let {
                    name: Name::new("answer"),
                    value: Rhs::Action(if via == 1 {
                        Action::Call {
                            callee,
                            args: vec![],
                        }
                    } else {
                        Action::Spawn {
                            callee,
                            args: vec![],
                        }
                    }),
                }];
                if via == 2 {
                    main.push(Stmt::Let {
                        name: Name::new("joined"),
                        value: Rhs::Action(Action::Join {
                            task: Atom::Variable(Name::new("answer")),
                        }),
                    });
                }
                main.push(Stmt::Return {
                    value: Expr::Variable(Name::new(if via == 2 { "joined" } else { "answer" })),
                });
                let descriptor = match &program.document.exports[&Name::new("answer")] {
                    lash_kernel_doc::Export::Function(export) => export,
                    _ => unreachable!(),
                };
                let caller = self::document(
                    BTreeMap::from([(
                        ImportSlot(0),
                        Import {
                            name: QualifiedName {
                                package: lash_vocabulary::PackagePath::parse("worker")
                                    .expect("package"),
                                member: lash_vocabulary::Segment::new("answer").expect("member"),
                                type_member: None,
                            },
                            kind: ImportKind::Code {
                                signature: descriptor.signature.clone(),
                                convention: descriptor.convention.semantic(),
                            },
                        },
                    )]),
                    BTreeMap::new(),
                    Some(main),
                );
                let caller_id = caller.identity();
                let caller_link = Link {
                    document: caller_id,
                    fills: BTreeMap::from([(
                        ImportSlot(0),
                        Fill::Export {
                            link: program.link,
                            export: ExportName {
                                name: Name::new("answer"),
                                member: None,
                            },
                        },
                    )]),
                };
                let root = caller_link.identity();
                let mut links = links.clone();
                links.insert(root, caller_link);
                let mut documents = program.documents.clone();
                documents.insert(caller_id, Arc::new(caller));
                (
                    Program::prepare(
                        root,
                        documents,
                        &links,
                        PageId([9; 32]),
                        program.library.clone(),
                    )
                    .expect("imported native"),
                    Target::Main,
                )
            };
            let pages = Pages::default();
            let mut machine = KernelMachine::start(
                program.clone(),
                bounds(),
                CellStart {
                    owner: PageId([8; 32]),
                    target,
                    args: vec![],
                    session: None,
                    modes: BTreeMap::new(),
                    host: vec![],
                },
                Box::new(pages.clone()),
            )
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
            let requested = request(&mut machine);
            let saved = machine
                .checkpoint()
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
            machine = KernelMachine::open(program, bounds(), &saved.checkpoint(), Box::new(pages))
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
            machine
                .deliver(requested.wait, Outcome::Completed(Datum::Null))
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
            loop {
                match machine
                    .run(&mut Passive, 100_000)
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
                {
                    Step::Slice => {}
                    Step::Ended(End::Finished(done)) => {
                        assert_eq!(done.result, Datum::Bool(kind != 2));
                        break;
                    }
                    other => panic!("native settlement did not complete: {other:?}"),
                }
            }
        }
    }
}

/// Imported arguments fit the public signature before a source ABI packs or
/// drops them; a private Any parameter cannot weaken that public contract.
#[test]
fn imported_arguments_fit_before_private_abi_adaptation() {
    for abi in [
        Convention::Direct,
        Convention::TsDirect,
        Convention::TsArgsList,
        Convention::PyDirect,
    ] {
        let callback_signature = Signature {
            params: vec![],
            result: Type::Bool,
        };
        for (parameter_type, token_case, accepted) in [
            (Type::Bool, None, false),
            (
                Type::Function(Box::new(callback_signature.clone())),
                None,
                false,
            ),
            (
                Type::List(Box::new(Type::Function(Box::new(
                    callback_signature.clone(),
                )))),
                None,
                false,
            ),
            (
                Type::Function(Box::new(callback_signature.clone())),
                Some(("fn-token", callback_signature.clone())),
                true,
            ),
            (
                Type::Function(Box::new(callback_signature.clone())),
                Some(("forged-token", callback_signature.clone())),
                false,
            ),
            (
                Type::Function(Box::new(callback_signature.clone())),
                Some((
                    "fn-token",
                    Signature {
                        params: vec![],
                        result: Type::Null,
                    },
                )),
                false,
            ),
        ] {
            let identity = function(
                "test.identity",
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
            let identity_id = identity.identity();
            let convention = match abi {
                Convention::Direct => StartConvention::Direct,
                Convention::TsDirect => StartConvention::TsDirect {
                    await_fn: lash_kernel_doc::FunctionTarget::Local(identity_id),
                },
                Convention::TsArgsList => StartConvention::TsArgsList {
                    await_fn: lash_kernel_doc::FunctionTarget::Local(identity_id),
                    same_fn: lash_kernel_doc::FunctionTarget::Local(identity_id),
                },
                Convention::PyDirect => StartConvention::PyDirect {
                    cleanup: lash_kernel_doc::FunctionTarget::Local(identity_id),
                },
            };
            let private_count = match abi {
                Convention::TsDirect => 0,
                Convention::TsArgsList => 2,
                _ => 1,
            };
            let body = function(
                "test.public",
                Signature {
                    params: (0..private_count)
                        .map(|index| Param {
                            name: Name::new(format!("arg{index}")),
                            ty: Type::Any,
                            optional: false,
                        })
                        .collect(),
                    result: Type::Any,
                },
                BTreeMap::new(),
                vec![Stmt::Return {
                    value: Expr::Literal(Literal::Bool(true)),
                }],
            );
            let body_id = body.identity();
            let public = Signature {
                params: vec![Param {
                    name: Name::new("value"),
                    ty: parameter_type.clone(),
                    optional: false,
                }],
                result: Type::Bool,
            };
            let mut callee = document(
                BTreeMap::new(),
                BTreeMap::from([(body_id, body), (identity_id, identity)]),
                None,
            );
            callee.exports.insert(
                Name::new("public"),
                lash_kernel_doc::Export::Function(FunctionExport {
                    function: body_id,
                    signature: public.clone(),
                    convention,
                    asynchronous: false,
                    rest: false,
                }),
            );
            let callee_link = Link {
                document: callee.identity(),
                fills: BTreeMap::new(),
            };
            let callee_id = callee_link.identity();
            let dynamic = function(
                "test.dynamic",
                Signature {
                    params: vec![],
                    result: Type::Any,
                },
                BTreeMap::new(),
                vec![Stmt::Return { value: null() }],
            );
            let dynamic_id = dynamic.identity();
            let provided = token_case
                .as_ref()
                .map_or(&callback_signature, |(_, signature)| signature);
            let callback = function(
                "test.callback",
                provided.clone(),
                BTreeMap::new(),
                vec![Stmt::Return {
                    value: if provided.result == Type::Null {
                        null()
                    } else {
                        Expr::Literal(Literal::Bool(true))
                    },
                }],
            );
            let callback_id = callback.identity();
            let input = token_case.map_or_else(
                || {
                    Rhs::Action(Action::Call {
                        callee: Callee::Local(dynamic_id),
                        args: vec![],
                    })
                },
                |(tag, signature)| {
                    Rhs::Expr(Expr::Tuple(vec![
                        Expr::Literal(Literal::Text(tag.into())),
                        Expr::Closure(Box::new(Closure {
                            function: callback_id,
                            presentation: Some(lash_kernel_doc::CallablePresentation {
                                signature,
                                representation: lash_kernel_doc::FunctionRepresentation::Token {
                                    tag: "fn-token".into(),
                                    callable: 1,
                                },
                                convention: StartConvention::Direct,
                            }),
                        })),
                    ]))
                },
            );
            let caller = document(
                BTreeMap::from([(
                    ImportSlot(0),
                    Import {
                        name: QualifiedName {
                            package: lash_vocabulary::PackagePath::parse("library")
                                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                            member: lash_vocabulary::Segment::new("public")
                                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
                            type_member: None,
                        },
                        kind: ImportKind::Code {
                            signature: public,
                            convention: abi,
                        },
                    },
                )]),
                BTreeMap::from([(dynamic_id, dynamic), (callback_id, callback)]),
                Some(vec![
                    Stmt::Let {
                        name: Name::new("input"),
                        value: input,
                    },
                    Stmt::Let {
                        name: Name::new("output"),
                        value: Rhs::Action(Action::Call {
                            callee: Callee::Import(ImportSlot(0)),
                            args: vec![Atom::Variable(Name::new("input"))],
                        }),
                    },
                    Stmt::Return {
                        value: Expr::Variable(Name::new("output")),
                    },
                ]),
            );
            if accepted {
                lash_kernel_check::derive(&caller, &caller.functions)
                    .expect("a valid callable token is admitted at the public Fn boundary");
            }
            let caller_link = Link {
                document: caller.identity(),
                fills: BTreeMap::from([(
                    ImportSlot(0),
                    Fill::Export {
                        link: callee_id,
                        export: ExportName {
                            name: Name::new("public"),
                            member: None,
                        },
                    },
                )]),
            };
            let root = caller_link.identity();
            let program = Program::prepare(
                root,
                BTreeMap::from([
                    (caller.identity(), Arc::new(caller)),
                    (callee.identity(), Arc::new(callee)),
                ]),
                &BTreeMap::from([(root, caller_link), (callee_id, callee_link)]),
                PageId([8; 32]),
                PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
            )
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
            let mut machine = KernelMachine::start(
                program,
                bounds(),
                CellStart {
                    owner: PageId([9; 32]),
                    target: Target::Main,
                    args: vec![],
                    session: None,
                    modes: BTreeMap::new(),
                    host: vec![],
                },
                Box::new(Pages::default()),
            )
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
            loop {
                match machine
                    .run(&mut Passive, u64::MAX)
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
                {
                    Step::Slice => {}
                    Step::Ended(End::Finished(done)) if accepted => {
                        assert_eq!(done.result, Datum::Bool(true));
                        break;
                    }
                    Step::Ended(End::Error(lash_kernel_vm::RunError::Uncaught {
                        error: Datum::Error(error),
                        ..
                    })) if !accepted => {
                        assert_eq!(
                            error.kind, "function_argument",
                            "{abi:?} {parameter_type:?}"
                        );
                        break;
                    }
                    other => panic!(
                        "public argument mismatch accepted under {abi:?} {parameter_type:?}: {other:?}"
                    ),
                }
            }
        }
    }
}
/// L-SUPERSET: full retained declarations do not add native requirements to
/// selected code merely because a body records an unused local alias.
#[test]
fn unused_native_declarations_do_not_enlarge_selected_reach() {
    let unused = FunctionDefinition {
        kernel: lash_kernel_doc::KERNEL_VERSION,
        name: FunctionName::new("unused.native")
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
        signature: Signature {
            params: vec![],
            result: Type::Null,
        },
        errors: BTreeSet::new(),
        guard: None,
        implementation: Implementation::Native {
            charge: lash_kernel_doc::Formula::Constant(1),
        },
        native_version: 1,
    };
    let unused_id = unused.identity();
    let mut selected = function(
        "selected.body",
        Signature {
            params: vec![],
            result: Type::Bool,
        },
        BTreeMap::new(),
        vec![Stmt::Return {
            value: Expr::Literal(Literal::Bool(true)),
        }],
    );
    selected
        .body_mut()
        .unwrap_or_else(|| panic!("selected law fixture has no body"))
        .functions
        .insert(unused_id, unused.name.clone());
    let selected_id = selected.identity();
    let mut doc = document(
        BTreeMap::new(),
        BTreeMap::from([(unused_id, unused), (selected_id, selected)]),
        Some(vec![
            Stmt::Let {
                name: Name::new("answer"),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Local(selected_id),
                    args: vec![],
                }),
            },
            Stmt::Return {
                value: Expr::Variable(Name::new("answer")),
            },
        ]),
    );
    doc.requires.natives.insert(unused_id);
    let link = Link {
        document: doc.identity(),
        fills: BTreeMap::new(),
    };
    let id = link.identity();
    let mut program = Program::prepare(
        id,
        BTreeMap::from([(doc.identity(), Arc::new(doc))]),
        &BTreeMap::from([(id, link)]),
        PageId([8; 32]),
        PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"));
    program.functions = BTreeSet::from([selected_id]);
    let mut machine = KernelMachine::start(
        program,
        bounds(),
        CellStart {
            owner: PageId([9; 32]),
            target: Target::Main,
            args: vec![],
            session: None,
            modes: BTreeMap::new(),
            host: vec![],
        },
        Box::new(Pages::default()),
    )
    .expect("unused declaration adds no runtime requirement");
    loop {
        match machine
            .run(&mut Passive, u64::MAX)
            .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        {
            Step::Slice => {}
            Step::Ended(End::Finished(done)) => {
                assert_eq!(done.result, Datum::Bool(true));
                break;
            }
            other => panic!("selected body: {other:?}"),
        }
    }
}

/// FIG-7143: nested result mismatches retain callable identity and RFC 6901 paths.
#[test]
fn entry_nested_result_mismatch_retains_typed_boundary_facts() {
    use lash_vocabulary::types::{RecordType, RecordTypeField};
    for suspended in [false, true] {
        let signature = Signature {
            params: vec![],
            result: Type::Record(RecordType {
                fields: vec![RecordTypeField {
                    name: "a/b~c".into(),
                    ty: Type::List(Box::new(Type::Text)),
                    optional: false,
                }],
                rest: None,
            }),
        };
        let imports = if suspended {
            BTreeMap::from([(
                ImportSlot(0),
                Import {
                    name: QualifiedName {
                        package: lash_vocabulary::PackagePath::parse("service").expect("package"),
                        member: lash_vocabulary::Segment::new("read").expect("member"),
                        type_member: None,
                    },
                    kind: ImportKind::Operation {
                        signature: self::signature(vec![]),
                    },
                },
            )])
        } else {
            BTreeMap::new()
        };
        let mut block = if suspended { vec![perform()] } else { vec![] };
        block.push(Stmt::Return {
            value: Expr::Record(vec![lash_kernel_doc::RecordEntry {
                field: "a/b~c".into(),
                value: Expr::List(vec![Expr::Literal(Literal::Bool(true))]),
            }]),
        });
        let definition = function("test.wrong", signature.clone(), imports.clone(), block);
        let function = definition.identity();
        let mut document = document(imports, BTreeMap::from([(function, definition)]), None);
        document.exports.insert(
            Name::new("entry"),
            lash_kernel_doc::Export::Function(FunctionExport {
                function,
                signature,
                convention: StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
        let id = document.identity();
        let fills = if suspended {
            BTreeMap::from([(
                ImportSlot(0),
                Fill::Operation(OperationRequirement::new(
                    self::signature(vec![]),
                    ContractId::new("read.v1"),
                )),
            )])
        } else {
            BTreeMap::new()
        };
        let link = Link {
            document: id,
            fills,
        };
        let link_id = link.identity();
        let program = Program::prepare(
            link_id,
            BTreeMap::from([(id, Arc::new(document))]),
            &BTreeMap::from([(link_id, link)]),
            PageId([9; 32]),
            PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
        )
        .expect("entry program");
        let pages = Pages::default();
        let mut machine = KernelMachine::start(
            program.clone(),
            bounds(),
            CellStart {
                owner: PageId([8; 32]),
                target: Target::Entry(Name::new("entry")),
                args: vec![],
                session: None,
                modes: BTreeMap::new(),
                host: vec![],
            },
            Box::new(pages.clone()),
        )
        .expect("start entry");
        if suspended {
            let asked = request(&mut machine);
            let saved = machine.checkpoint().expect("parked boundary");
            machine = KernelMachine::open(program, bounds(), &saved.checkpoint(), Box::new(pages))
                .expect("cold restore");
            machine
                .deliver(asked.wait, Outcome::Completed(Datum::Null))
                .expect("answer");
        }
        loop {
            match machine.run(&mut Passive, 100_000).expect("run entry") {
                Step::Slice => {}
                Step::Ended(End::Error(lash_kernel_vm::RunError::Uncaught {
                    error: Datum::Error(error),
                    ..
                })) => {
                    assert_eq!(error.kind, "entry_type_mismatch");
                    assert_eq!(
                        error.data,
                        lash_kernel_doc::diagnostic_datum(&lash_vocabulary::EntryTypeMismatch {
                            callable: lash_vocabulary::CallableId {
                                link: link_id,
                                function
                            },
                            mismatch: lash_vocabulary::TypeMismatch::new(Type::Text, Type::Bool)
                                .within("0")
                                .within("a/b~c")
                                .within("output"),
                        })
                        .expect("typed nested mismatch")
                    );
                    break;
                }
                other => panic!("wrong result was accepted: {other:?}"),
            }
        }
    }
}

/// FIG-7143: a wrong input refuses before any guest statement or effect runs.
#[test]
fn entry_wrong_input_is_refused_before_body_execution() {
    let signature = signature(vec![Param {
        name: Name::new("event"),
        ty: Type::List(Box::new(Type::Text)),
        optional: false,
    }]);
    let definition = function(
        "test.input",
        signature.clone(),
        BTreeMap::new(),
        vec![Stmt::Throw {
            value: Expr::Literal(Literal::Text("body ran".into())),
        }],
    );
    let function = definition.identity();
    let mut document = document(
        BTreeMap::new(),
        BTreeMap::from([(function, definition)]),
        None,
    );
    document.exports.insert(
        Name::new("entry"),
        lash_kernel_doc::Export::Function(FunctionExport {
            function,
            signature,
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    let id = document.identity();
    let link = Link {
        document: id,
        fills: BTreeMap::new(),
    };
    let link_id = link.identity();
    let program = Program::prepare(
        link_id,
        BTreeMap::from([(id, Arc::new(document))]),
        &BTreeMap::from([(link_id, link)]),
        PageId([9; 32]),
        PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    )
    .expect("entry program");
    let started = KernelMachine::start(
        program,
        bounds(),
        CellStart {
            owner: PageId([8; 32]),
            target: Target::Entry(Name::new("entry")),
            args: vec![Datum::List(vec![Datum::Bool(true)])],
            session: None,
            modes: BTreeMap::new(),
            host: vec![],
        },
        Box::new(Pages::default()),
    );
    let Err(error) = started else {
        panic!("refuse before returning a runnable machine")
    };
    assert_eq!(
        error,
        lash_kernel_vm::StartError::TypeMismatch(Box::new(lash_vocabulary::EntryTypeMismatch {
            callable: lash_vocabulary::CallableId {
                link: link_id,
                function
            },
            mismatch: lash_vocabulary::TypeMismatch::new(Type::Text, Type::Bool)
                .within("0")
                .within("event")
                .within("input"),
        }))
    );
}
