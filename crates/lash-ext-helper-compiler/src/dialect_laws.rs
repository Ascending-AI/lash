//! Authored dialect laws over real linked helper bodies and ordinary operations.
use crate::library::{Dialect, Library};
use lash_kernel_dialect::{Environment, Library as _, Mode, NamedLibrary, Namespaces};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, Document, ErrorDatum, FunctionCatalog, FunctionRegistry, Handle};

use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, PreparedLibrary, Program,
    Request, Step, Target,
};
use lash_vocabulary::package::{Fill, Link};
use lash_vocabulary::{ImportKind, Name, OperationMode, OperationUse};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, VecDeque},
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

struct Fixture {
    library: NamedLibrary,
    registry: Arc<FunctionRegistry>,
    helpers: Arc<lash_kernel_doc::BuildHelpers>,
}
impl Fixture {
    fn new(dialect: Dialect) -> Self {
        let built = Library::of(dialect).expect("dialect definitions");
        let helpers = Arc::new(
            lash_kernel_doc::BuildHelpers::new(
                built.registry.clone(),
                built.registry.iter().map(|(id, _)| *id),
            )
            .expect("closed helper image"),
        );
        let library = built.names;
        Self {
            registry: Arc::new(built.registry),
            library,
            helpers,
        }
    }
    fn lower(&self, dialect: Dialect, source: &str, mode: Mode<'_>) -> Document {
        let namespaces = Namespaces::default();
        let environment = Environment::new(&self.library, &namespaces);
        match dialect {
            Dialect::TypeScript => lash_dialect_typescript::lower(source, &environment, mode)
                .expect("authored TypeScript"),
            Dialect::Python => {
                lash_dialect_python::lower(source, &environment, mode).expect("authored Python")
            }
        }
        .document
        .into_document()
    }
    fn program(&self, document: Document) -> Program {
        if let Err(error) = lash_kernel_doc::validate_document(&document, &self.library) {
            panic!(
                "lowered invalid {error:?}\n{}",
                lash_kernel_doc::print_document(&document)
            );
        }
        let fills = document
            .imports
            .iter()
            .map(|(slot, import)| {
                assert!(matches!(import.kind, ImportKind::Operation { .. }));
                (
                    *slot,
                    Fill::Operation(
                        lash_kernel_dialect::authoring::service_requirement(&import.name)
                            .expect("service"),
                    ),
                )
            })
            .collect();
        let root = Link {
            document: document.identity(),
            fills,
        };
        Program::prepare(
            root.identity(),
            BTreeMap::from([(document.identity(), Arc::new(document))]),
            &BTreeMap::from([(root.identity(), root)]),
            PageId([7; 32]),
            PreparedLibrary::with_helpers(Arc::clone(&self.registry), Arc::clone(&self.helpers)),
            lash_kernel_doc::Selection::Fresh,
        )
        .expect("prepared linked program")
    }
}
#[derive(Default)]
struct Script {
    draws: VecDeque<u64>,
    uses: BTreeMap<OperationUse, String>,
    printed: Vec<Datum>,
}
impl Host for Script {
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("projection")
    }
    fn perform(
        &mut self,
        use_: &OperationUse,
        _: &[Datum],
        _: &lash_vocabulary::sites::PositionLabel,
        _: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        Ok(Ok(match self.uses[use_].as_str() {
            "typescript.random" => Datum::Bytes(lash_kernel_doc::Bytes::new(
                self.draws
                    .pop_front()
                    .expect("scripted bits")
                    .to_be_bytes()
                    .as_slice(),
            )),
            "typescript.clock" | "python.clock" => Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: lash_kernel_doc::Integer::from(1_234_567_890_i64),
            }),
            other => panic!("unexpected read {other}"),
        }))
    }
    fn print(&mut self, value: &Datum) {
        self.printed.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
fn bounds() -> Bounds {
    Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 10_000_000,
        memory: 64 << 20,
        call_depth: 200,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    }
}
fn start(program: Program, script: &mut Script) -> KernelMachine {
    start_on(program, script, Pages::default())
}
fn start_on(program: Program, script: &mut Script, pages: Pages) -> KernelMachine {
    start_target(program, script, pages, Target::Main)
}
fn start_target(
    program: Program,
    script: &mut Script,
    pages: Pages,
    target: Target,
) -> KernelMachine {
    let mut modes = BTreeMap::new();
    for (link, instance) in &program.instances {
        for (slot, import) in &program.documents[&instance.document].imports {
            if matches!(import.kind, ImportKind::Operation { .. }) {
                let use_ = OperationUse {
                    link: *link,
                    slot: *slot,
                };
                script.uses.insert(
                    use_,
                    format!("{}.{}", import.name.package, import.name.member),
                );
                modes.insert(
                    use_,
                    if import.name.member.as_str() == "sleep" {
                        OperationMode::Waited
                    } else {
                        OperationMode::Read
                    },
                );
            }
        }
    }
    KernelMachine::start(
        program.clone(),
        bounds(),
        CellStart {
            owner: PageId([9; 32]),
            target,
            args: vec![],
            session: None,
            modes,
            host: vec![],
        },
        Box::new(pages),
    )
    .unwrap_or_else(|error| {
        if let lash_kernel_vm::StartError::MissingFunction { function } = &error {
            let found = program
                .documents
                .values()
                .find_map(|document| document.functions.get(function));
            panic!("start {error:?}; definition={found:?}");
        }
        panic!("start {error:?}")
    })
}

/// K-HOST-003: direct and borrowed Math.random use the exact high 53 bits.
#[test]
fn math_random_preserves_the_exact_top_53_bits_for_direct_and_borrowed_calls() {
    let fixture = Fixture::new(Dialect::TypeScript);
    let source = "export function run(): number[] { const draw = Math.random; const values = []; for (let i = 0; i < 4; i++) { values.push(Math.random()); values.push(draw()); } return values; }";
    let document = fixture.lower(Dialect::TypeScript, source, Mode::Module);
    let mut script = Script {
        draws: [0, 1 << 62, 1 << 63, u64::MAX]
            .into_iter()
            .flat_map(|bits| [bits, bits])
            .collect(),
        ..Default::default()
    };
    let mut machine = start_target(
        fixture.program(document),
        &mut script,
        Pages::default(),
        Target::Entry(Name::new("run")),
    );
    let Step::Ended(End::Finished(finished)) = machine.run(&mut script, u64::MAX).unwrap() else {
        panic!("numeric draws")
    };
    let Datum::List(values) = finished.result else {
        panic!("list of numeric draws")
    };
    let expected = [0.0, 0.25, 0.5, 9007199254740991.0 / 9007199254740992.0];
    let draws = values
        .iter()
        .map(|value| {
            let Datum::Float(value) = value else {
                panic!("float draw")
            };
            value.get()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        draws,
        expected
            .into_iter()
            .flat_map(|draw| [draw, draw])
            .collect::<Vec<_>>()
    );
    assert!(script.draws.is_empty());
}

/// G-SOURCE-READABLE: sync and async declarations retain order and identity.
#[test]
fn authored_modules_print_without_kernel_forms_and_roundtrip() {
    for (dialect, source) in [
        (
            Dialect::TypeScript,
            "export function zed(x: number): number { return x; }\nexport async function alpha(x: string): Promise<string> { return x; }",
        ),
        (
            Dialect::Python,
            "def zed(x: int) -> int:\n    return x\n\nasync def alpha(x: str) -> str:\n    return x\n",
        ),
    ] {
        let fixture = Fixture::new(dialect);
        let document = fixture.lower(dialect, source, Mode::Module);
        let printed = match dialect {
            Dialect::TypeScript => lash_dialect_typescript::print(
                &document,
                None,
                &fixture.library,
                Default::default(),
            ),
            Dialect::Python => {
                lash_dialect_python::print(&document, None, &fixture.library, Default::default())
            }
        }
        .expect("authored print");
        assert!(!printed.source.contains("use kernel"), "{}", printed.source);
        assert!(printed.source.find("zed").unwrap() < printed.source.find("alpha").unwrap());
        let (read, _) = match dialect {
            Dialect::TypeScript => lash_dialect_typescript::read(
                &printed.source,
                &document,
                &fixture.library,
                Default::default(),
            )
            .map_err(|error| error.to_string()),
            Dialect::Python => lash_dialect_python::read(
                &printed.source,
                &document,
                &fixture.library,
                Default::default(),
            )
            .map_err(|error| error.to_string()),
        }
        .expect("ordinary source");
        assert_eq!(read.identity(), document.identity(), "{}", printed.source);
    }
}

/// L-DEFERRED-ATTACH: source aliases are spelling; the resulting operation
/// Import retains the declared canonical package and no chosen contract.
#[test]
fn deferred_aliases_lower_to_signature_only_operation_imports() {
    for (dialect, source, member) in [
        (
            Dialect::TypeScript,
            "import { sendItem as deliver } from 'wip'; await deliver({id: 1});",
            "send_item",
        ),
        (
            Dialect::TypeScript,
            "await wip.sendItem({id: 1});",
            "send_item",
        ),
        (
            Dialect::Python,
            "from wip import send_item as deliver\nawait deliver({'id': 1})\n",
            "send_item",
        ),
        (
            Dialect::Python,
            "import wip as pending\nawait pending.send_item({'id': 1})\n",
            "send_item",
        ),
    ] {
        let fixture = Fixture::new(dialect);
        let names = Namespaces::new(
            BTreeMap::new(),
            BTreeSet::from([lash_vocabulary::PackagePath::parse("wip").unwrap()]),
            &|_| false,
        )
        .unwrap();
        let environment = Environment::new(&fixture.library, &names);
        let mode = Mode::Cell(lash_kernel_dialect::CellEnvironment::first());
        let lowered = match dialect {
            Dialect::TypeScript => lash_dialect_typescript::lower(source, &environment, mode)
                .map_err(|error| error.to_string()),
            Dialect::Python => lash_dialect_python::lower(source, &environment, mode)
                .map_err(|error| error.to_string()),
        }
        .expect("declared deferred source path");
        let imported = lowered
            .document
            .imports
            .values()
            .find(|import| import.name.package.to_string() == "wip")
            .expect("operation import");
        assert_eq!(imported.name.member.as_str(), member);
        assert!(matches!(imported.kind, ImportKind::Operation { .. }));
        lash_kernel_doc::validate_document(&lowered.document, &fixture.library).unwrap();
    }
}

/// K-TASK-005/017/024: ordinary sleep deliveries take their queue turn,
/// including failures and a cold continuation after delivery.
#[test]
fn ordinary_sleep_deliveries_keep_their_queue_turn() {
    for (dialect, source) in [
        (
            Dialect::TypeScript,
            "async function child(tag) { try { await sleep(5); console.log(tag); } catch (e) { console.log('caught'); } } await Promise.all([child('a'), child('b')]);",
        ),
        (
            Dialect::Python,
            "import asyncio\nasync def child(tag):\n    try:\n        await asyncio.sleep(0.005)\n        print(tag)\n    except Exception:\n        print('caught')\nawait asyncio.gather(child('a'), child('b'))\n",
        ),
    ] {
        let fixture = Fixture::new(dialect);
        for failed in [false, true] {
            let document = fixture.lower(
                dialect,
                source,
                Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
            );
            let program = fixture.program(document);
            let mut script = Script::default();
            let pages = Pages::default();
            let mut machine = start_on(program.clone(), &mut script, pages.clone());
            let Step::Parked(park) = machine.run(&mut script, u64::MAX).unwrap() else {
                panic!("two ordinary waits")
            };
            assert_eq!(park.requests.len(), 2);
            let mut waits = Vec::new();
            for request in park.requests {
                let Request::Effect(effect) = request;
                assert!(script.uses[&effect.use_].ends_with(".sleep"));
                assert_eq!(effect.args, [Datum::Int(5.into())]);
                waits.push(effect.wait);
            }
            for (index, wait) in waits.into_iter().enumerate() {
                machine
                    .deliver(
                        wait,
                        if failed && index == 0 {
                            Outcome::Failed(ErrorDatum {
                                kind: "sleep_failure".into(),
                                message: "scripted".into(),
                                data: Datum::Null,
                            })
                        } else {
                            Outcome::Completed(Datum::Null)
                        },
                    )
                    .unwrap();
            }
            let sealed = machine.checkpoint().unwrap();
            machine = KernelMachine::open(program, bounds(), &sealed.checkpoint(), Box::new(pages))
                .unwrap();
            assert!(matches!(
                machine.run(&mut script, u64::MAX).unwrap(),
                Step::Ended(End::Finished(_))
            ));
            let lines = script
                .printed
                .iter()
                .map(|v| {
                    let Datum::Text(v) = v else {
                        panic!("printed text")
                    };
                    v.as_str()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                lines,
                if failed {
                    vec!["caught", "b"]
                } else {
                    vec!["a", "b"]
                }
            );
        }
    }
}

/// K-TASK-024: nonpositive Python sleep is a queue yield with no operation.
#[test]
fn nonpositive_python_sleep_yields_without_calling_a_service() {
    let fixture = Fixture::new(Dialect::Python);
    for duration in ["0", "-1", "-0.25"] {
        let source = format!(
            "import asyncio\nasync def child():\n    await asyncio.sleep({duration})\n    print('child')\ntask = asyncio.create_task(child())\nprint('main')\nawait task\n"
        );
        let document = fixture.lower(
            Dialect::Python,
            &source,
            Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        );
        let mut script = Script::default();
        let mut machine = start(fixture.program(document), &mut script);
        assert!(matches!(
            machine.run(&mut script, u64::MAX).unwrap(),
            Step::Ended(End::Finished(_))
        ));
        assert_eq!(
            script.printed,
            [Datum::Text("main".into()), Datum::Text("child".into())]
        );
    }
}

/// K-CHG-HELPER: the replacement service conversion formulas bound the
/// same bodies executed with ordinary statement metering and native charges.
#[test]
fn service_conversion_formulas_bound_their_executed_work() {
    for (dialect, helper, arguments) in [
        (
            Dialect::TypeScript,
            "ts.math.random",
            vec![Datum::Null, Datum::List(vec![])],
        ),
        (
            Dialect::TypeScript,
            "ts.sleep_duration",
            vec![Datum::Float(lash_kernel_doc::Float::new(5.9))],
        ),
        (
            Dialect::TypeScript,
            "ts.sleep_duration",
            vec![Datum::Float(lash_kernel_doc::Float::new(f64::INFINITY))],
        ),
        (Dialect::Python, "py.sleep", vec![Datum::Int(0.into())]),
        (
            Dialect::Python,
            "py.sleep",
            vec![Datum::Float(lash_kernel_doc::Float::new(0.005))],
        ),
    ] {
        let fixture = Fixture::new(dialect);
        let id = fixture.library.resolve(helper).expect("service helper");
        let definition = fixture.library.definition(&id).unwrap();
        let body = definition.body().unwrap();
        let bound = definition.charge().unwrap().evaluate(&mut |_, _| 1);
        let mut document = Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
        document.imports = body.imports.clone();
        let mut main = Vec::new();
        for (parameter, value) in definition.signature.params.iter().zip(arguments) {
            // Host start values are excluded from the measured work.
            let literal = match value {
                Datum::Null => lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Null),
                Datum::Int(n) => lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Int(n)),
                Datum::Float(n) => {
                    lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Float(n))
                }
                Datum::List(values) if values.is_empty() => lash_kernel_doc::Expr::List(vec![]),
                _ => panic!("scalar fixture input"),
            };
            main.push(lash_kernel_doc::Stmt::Let {
                name: parameter.name.clone(),
                value: lash_kernel_doc::Rhs::Expr(literal),
            });
        }
        main.extend(body.block.clone());
        let used = lash_kernel_doc::Node::Block(&main).functions_within();
        document.main = Some(main);
        lash_kernel_dialect::authoring::include_library_dependencies(
            &mut document,
            used,
            &fixture.library,
        )
        .expect("inline body dependencies");
        let mut script = Script {
            draws: VecDeque::from([u64::MAX]),
            ..Default::default()
        };
        let mut machine = start(fixture.program(document), &mut script);
        loop {
            match machine.run(&mut script, u64::MAX).unwrap() {
                Step::Ended(_) => break,
                Step::Parked(park) => {
                    for request in park.requests {
                        let Request::Effect(effect) = request;
                        machine
                            .deliver(effect.wait, Outcome::Completed(Datum::Null))
                            .unwrap();
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        let work = machine.meters().charged.unwrap();
        assert!(
            work <= bound,
            "{helper}: {work} real units exceeds formula {bound}"
        );
    }
}

/// K-TASK-005/017: cancelling an ordinary sleep enqueues the victim's
/// exception after the ready task's next turn.
#[test]
fn cancelling_python_sleep_takes_a_turn_before_the_handler() {
    let fixture = Fixture::new(Dialect::Python);
    let source = "import asyncio\nasync def victim():\n    try:\n        await asyncio.sleep(0.005)\n    except BaseException:\n        print('caught')\nasync def other():\n    await asyncio.sleep(0.007)\n    task.cancel()\n    print('other')\n    await asyncio.sleep(0)\n    print('later')\ntask = asyncio.create_task(victim())\nawait other()\nawait task\n";
    let document = fixture.lower(
        Dialect::Python,
        source,
        Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
    );
    let mut script = Script::default();
    let mut machine = start(fixture.program(document), &mut script);
    let Step::Parked(park) = machine.run(&mut script, u64::MAX).unwrap() else {
        panic!("sleep waits")
    };
    assert_eq!(park.requests.len(), 2);
    for request in park.requests {
        let Request::Effect(effect) = request;
        if effect.args == [Datum::Int(7.into())] {
            machine
                .deliver(effect.wait, Outcome::Completed(Datum::Null))
                .unwrap();
        } else {
            assert_eq!(effect.args, [Datum::Int(5.into())]);
        }
    }
    assert!(matches!(
        machine.run(&mut script, u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    assert_eq!(
        script.printed,
        ["other", "later", "caught"].map(|s| Datum::Text(s.into()))
    );
}
