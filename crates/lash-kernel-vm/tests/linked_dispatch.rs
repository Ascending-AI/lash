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
    LinkId, Name, OperationRequirement, OperationUse, Param, QualifiedName, Signature, Type,
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
        name: FunctionName::new(name).unwrap(),
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
        match machine.run(&mut Passive, 100_000).unwrap() {
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
            package: lash_vocabulary::Vec1::new(lash_vocabulary::Segment::new("service").unwrap()),
            member: lash_vocabulary::Segment::new("read").unwrap(),
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
                    lash_vocabulary::Segment::new("library").unwrap(),
                ),
                member: lash_vocabulary::Segment::new("run").unwrap(),
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
    .unwrap();
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
        KernelMachine::start(program.clone(), bounds(), start, Box::new(pages.clone())).unwrap();
    let first = request(&mut machine);
    assert_eq!(
        first.use_,
        OperationUse {
            link: b,
            slot: ImportSlot(0)
        }
    );
    let checkpoint = machine.checkpoint().unwrap().checkpoint();
    drop(machine);
    let mut machine = KernelMachine::open(program, bounds(), &checkpoint, Box::new(pages)).unwrap();
    machine
        .deliver(first.wait, Outcome::Completed(Datum::Null))
        .unwrap();
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
        .unwrap();
    loop {
        match machine.run(&mut Passive, 100_000).unwrap() {
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
                package: lash_vocabulary::PackagePath::parse("service").unwrap(),
                member: lash_vocabulary::Segment::new("read").unwrap(),
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
    let program = Program::prepare(
        link_id,
        BTreeMap::from([(document_id, Arc::new(document))]),
        &BTreeMap::from([(link_id, link)]),
        PageId([9; 32]),
        PreparedLibrary::new(Arc::new(registry)),
    )
    .unwrap();
    let target = Target::Callable(Box::new(lash_kernel_doc::FunctionValue {
        callable: lash_vocabulary::CallableId {
            link: link_id,
            function,
        },
        captures: BTreeMap::from([(Name::new("captured"), Datum::Bool(true))]),
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
    .unwrap();
    let asked = request(&mut machine);
    let checkpoint = machine.checkpoint().unwrap().checkpoint();
    drop(machine);
    let mut cold = KernelMachine::open(program, bounds(), &checkpoint, Box::new(pages)).unwrap();
    cold.deliver(asked.wait, Outcome::Completed(Datum::Null))
        .unwrap();
    loop {
        match cold.run(&mut Passive, 100_000).unwrap() {
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
            package: lash_vocabulary::PackagePath::parse("service").unwrap(),
            member: lash_vocabulary::Segment::new("read").unwrap(),
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
                    package: lash_vocabulary::PackagePath::parse("library").unwrap(),
                    member: lash_vocabulary::Segment::new("packed").unwrap(),
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
    .unwrap();
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
    .unwrap();
    for _ in 0..4 {
        let asked = request(&mut machine);
        assert_eq!(
            asked.use_,
            OperationUse {
                link: b,
                slot: ImportSlot(0)
            }
        );
        let checkpoint = machine.checkpoint().unwrap().checkpoint();
        drop(machine);
        machine = KernelMachine::open(
            program.clone(),
            bounds(),
            &checkpoint,
            Box::new(pages.clone()),
        )
        .unwrap();
        machine
            .deliver(asked.wait, Outcome::Completed(Datum::Null))
            .unwrap();
    }
    loop {
        match machine.run(&mut Passive, 100_000).unwrap() {
            Step::Slice => {}
            Step::Ended(End::Finished(done)) => {
                assert_eq!(done.result, Datum::Bool(true));
                break;
            }
            other => panic!("expected settled completion: {other:?}"),
        }
    }
}

/// Public callable signatures constrain the settled value, independently
/// of the private function header and across a parked frame.
#[test]
fn callable_settled_result_must_fit_its_public_signature() {
    let definition = function(
        "test.result",
        Signature {
            params: vec![],
            result: Type::Any,
        },
        BTreeMap::new(),
        vec![Stmt::Return { value: null() }],
    );
    let function = definition.identity();
    let document = document(
        BTreeMap::new(),
        BTreeMap::from([(function, definition)]),
        None,
    );
    let document_id = document.identity();
    let link = Link {
        document: document_id,
        fills: BTreeMap::new(),
    };
    let link_id = link.identity();
    let program = Program::prepare(
        link_id,
        BTreeMap::from([(document_id, Arc::new(document))]),
        &BTreeMap::from([(link_id, link)]),
        PageId([9; 32]),
        PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
    )
    .unwrap();
    let mut machine = KernelMachine::start(
        program,
        bounds(),
        CellStart {
            owner: PageId([8; 32]),
            target: Target::Callable(Box::new(lash_kernel_doc::FunctionValue {
                callable: lash_vocabulary::CallableId {
                    link: link_id,
                    function,
                },
                captures: BTreeMap::new(),
                signature: Signature {
                    params: vec![],
                    result: Type::Bool,
                },
                convention: StartConvention::Direct,
                captions: lash_kernel_doc::Captions::empty(document_id),
            })),
            args: vec![],
            session: None,
            modes: BTreeMap::new(),
            host: vec![],
        },
        Box::new(Pages::default()),
    )
    .unwrap();
    loop {
        match machine.run(&mut Passive, 100_000).unwrap() {
            Step::Slice => {}
            Step::Ended(End::Error(lash_kernel_vm::RunError::Uncaught {
                error: Datum::Error(error),
                ..
            })) => {
                assert_eq!(error.kind, "function_result");
                break;
            }
            other => panic!("public signature mismatch was accepted: {other:?}"),
        }
    }
}

/// K-HOST and K-CHG: a synchronous native export needs no Main or guest
/// body, and its declared formula is the charge on that path.
#[test]
fn native_export_starts_without_main_and_pays_its_formula() {
    struct Answer;
    impl lash_kernel_doc::NativeFunction for Answer {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Ok(lash_kernel_doc::Value::Bool(true))
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
            name: FunctionName::new("test.native").unwrap(),
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
                (kind != 1).then(|| Arc::new(Answer) as Arc<dyn lash_kernel_doc::NativeFunction>),
            )
            .unwrap();
        let program = Program::prepare(
            link_id,
            BTreeMap::from([(document.identity(), Arc::new(document))]),
            &BTreeMap::from([(link_id, link)]),
            PageId([9; 32]),
            PreparedLibrary::new(Arc::new(registry)),
        )
        .unwrap();
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
        .unwrap();
        loop {
            match machine.run(&mut Passive, 100_000).unwrap() {
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
