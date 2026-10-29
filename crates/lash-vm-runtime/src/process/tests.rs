use lash_core::tool_run::{MaterialOwner, MaterialRole};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_kernel_value::names::DocumentId;
use lash_kernel_value::sites::EffectId;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;
use lash_vm_client::wire::OutcomeWire;

use super::advance::{advance, decode, state_format};
use super::state::{
    Admission, Issued, KERNEL_RUN_STEP, KernelRunInput, KernelRunOutput, ParkedPage, Phase,
};
use lash_kernel_doc::KernelVersion;
use lash_kernel_state::{AdmittedRequest, AdmittedWait};

fn process() -> lash_vocabulary::identity::ProcessId {
    lash_core::ProcessIdMint::sequential_id_for_testing(1)
}

fn completed(text: String) -> SettledOutput {
    SettledOutput::Completed(Material::journal_local(
        MaterialOwner::Process {
            process_id: process(),
        },
        MaterialRole::AttemptOutput,
        text,
    ))
}

/// A parked run of this build's kernel version as its state names it: the
/// page a `kernel_run` published.
fn sealed() -> ParkedPage {
    ParkedPage {
        root: lash_core_store::checkpoint_graph::NodeId([7; 32]),
        kernel: crate::LASH_KERNEL_VERSION,
    }
}

/// FIG-6193: every engine state that stands on a run names its page beside
/// its bytes, so the process's root owns it, and an ended run stands on
/// none. A state whose bytes name another page than the root holds is
/// refused.
/// The admission of a `perform` of `fetch` at `identity`.
fn operation_use() -> lash_kernel_value::OperationUse {
    lash_kernel_value::OperationUse {
        link: lash_kernel_value::LinkId::from_bytes([1; 32]),
        slot: (lash_kernel_value::ImportSlot(0)).into(),
    }
}
fn admitted(identity: EffectId) -> Admission {
    Admission(AdmittedWait {
        identity,
        request: AdmittedRequest::Effect {
            use_: operation_use(),
            args: Vec::new(),
        },
    })
}

fn parked(
    issued: Vec<Issued>,
    cancel_requests: Vec<lash_kernel_value::sites::WaitId>,
) -> SettledOutput {
    completed(
        serde_json::to_string(&KernelRunOutput::Parked {
            read_facts: Vec::new(),
            read_count: 0,
            state: sealed(),
            issued,
            cancel_requests,
            ran: Default::default(),
            caught: Vec::new(),
            reads: None,
            start: None,
            requirements: Default::default(),
        })
        .expect("a run output serializes"),
    )
}

/// The effect a fan-out element performs: the one site, run by the task
/// the `element`-th execution of the spawn started.
fn element(element: u64) -> EffectId {
    lash_kernel_value::sites::testkit::effect(
        lash_kernel_value::sites::testkit::spawned_task(
            lash_kernel_value::sites::testkit::root_task(),
            Site::new(Unit::Main, vec![0]),
            element,
        ),
        Site::new(
            Unit::Function(lash_kernel_value::names::Name::new("worker")),
            vec![0],
        ),
        0,
        Vec::new(),
    )
}

fn effect(wait: u64, identity: EffectId) -> Issued {
    Issued::Effect {
        origin: identity.position().site().clone(),
        wait: lash_kernel_value::sites::testkit::wait_id(wait),
        admission: admitted(identity),
        use_: operation_use(),
        input: serde_json::json!({ "wait": wait }),
    }
}

fn settle(
    state: EngineState,
    step: &StepName,
    outcome: SettledOutput,
) -> (EngineState, EngineAction) {
    advance(
        KernelVersion::NEWEST,
        state,
        EngineEvent::StepSettled {
            call_id: None,
            step: step.clone(),
            outcome,
        },
    )
    .expect("a settled step advances")
}

/// The one `kernel_run` an action asks for, with what it delivers, and the
/// effect steps beside it.
fn run_of(action: &EngineAction) -> (StepName, KernelRunInput, Vec<StepRequest>) {
    let EngineAction::Steps { steps, .. } = action else {
        panic!("expected steps, found {action:?}");
    };
    let mut run = None;
    let mut effects = Vec::new();
    for step in steps {
        match step {
            StepRequest::Engine { step, kind, input } => {
                assert_eq!(kind.0, KERNEL_RUN_STEP);
                run = Some((
                    step.clone(),
                    serde_json::from_value(input.clone()).expect("a run input decodes"),
                ));
            }
            tool @ StepRequest::Tool { .. } => effects.push(tool.clone()),
        }
    }
    let (step, input) = run.expect("the action runs the machine");
    (step, input, effects)
}

fn started() -> (EngineState, StepName) {
    let (state, action) = advance(
        KernelVersion::NEWEST,
        EngineState::empty(state_format(KernelVersion::NEWEST)),
        EngineEvent::Started {
            payload: serde_json::json!({
                "document": DocumentId::from_bytes([7; 32]),
                "entry": "main_entry",
            }),
        },
    )
    .expect("a start advances");
    let (run, input, effects) = run_of(&action);
    assert!(input.parked.is_none() && input.deliver.is_empty() && effects.is_empty());
    (state, run)
}

/// FIG-6870: the state holds each parked page, admission and settled
/// outcome with its origin, the run or record a replay reads it back
/// from. A stored state missing any one of them is refused, never resumed
/// with a start recorded whole.
#[test]
fn a_state_missing_the_origin_of_its_page_admission_or_outcome_is_refused() {
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(vec![effect(10, element(0)), effect(11, element(1))], vec![]),
    );
    let EngineAction::Steps { steps, .. } = &action else {
        panic!("a park on two effects asks for two steps, found {action:?}");
    };
    let (first, second) = (steps[0].step().clone(), steps[1].step().clone());
    let (state, _) = settle(state, &second, completed("\"b\"".to_owned()));
    // Settles while that run is in flight: the state holds its outcome.
    let (state, _) = settle(state, &first, completed("\"a\"".to_owned()));
    let (written, _) = decode(KernelVersion::NEWEST, &state).expect("a written state decodes");
    assert!(
        written.parked.is_some() && written.admissions.len() == 2 && written.settled.len() == 1,
        "the state holds a parked page, two admissions and a settled outcome"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&state.bytes).expect("a written state is JSON");
    // Each holder of an origin, by its JSON pointer: the parked run, every
    // held admission and the settled outcome.
    let holders = std::iter::once(("a parked page", "/parked".to_owned(), "by"))
        .chain(
            json["admissions"]
                .as_object()
                .expect("the parked run holds admissions")
                .keys()
                .map(|wait| ("an admission", format!("/admissions/{wait}"), "by")),
        )
        .chain(std::iter::once((
            "a settled outcome",
            "/settled/0".to_owned(),
            "source",
        )));
    for (what, holder, origin) in holders {
        let mut stripped = json.clone();
        stripped
            .pointer_mut(&holder)
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|holder| holder.remove(origin))
            .expect("the written state names the origin");
        let stripped = EngineState {
            bytes: serde_json::to_vec(&stripped).expect("JSON serializes"),
            ..state.clone()
        };
        assert!(
            decode(KernelVersion::NEWEST, &stripped).is_err(),
            "{what} at {holder} without its origin is refused"
        );
    }
}

/// Target 3 and `K-TASK-012`: a body that fans out parks on every element's
/// effect at once, each its own step at the identity the machine computed
/// (the task component tells the elements apart), and each outcome is
/// handed to the machine as it settles, in whatever order: one that settles
/// while a run is in flight is handed to the next run, and none is handed
/// twice. Each run resumes with the admission of every wait the parked run
/// still holds, and no other: the state holds a wait as its identity alone.
#[test]
fn a_fan_out_waits_on_every_element_and_delivers_each_outcome_once_in_any_order() {
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(vec![effect(10, element(0)), effect(11, element(1))], vec![]),
    );
    let EngineAction::Steps { steps, wake: None } = &action else {
        panic!("a park on two effects asks for two steps, found {action:?}");
    };
    let sites: Vec<_> = steps
        .iter()
        .map(|step| {
            step.site()
                .expect("an effect step names its identity")
                .clone()
        })
        .collect();
    assert_eq!(sites, vec![element(0), element(1)]);
    assert_ne!(sites[0].task(), sites[1].task());
    let (first, second) = (steps[0].step().clone(), steps[1].step().clone());

    // The second element settles first: the machine runs with it alone.
    let (state, action) = settle(state, &second, completed("\"b\"".to_owned()));
    let (run1, input, effects) = run_of(&action);
    assert!(effects.is_empty());
    assert_eq!(
        input.deliver.iter().map(|d| d.wait).collect::<Vec<_>>(),
        vec![lash_kernel_value::sites::testkit::wait_id(11)]
    );
    assert!(input.parked.is_some());
    let held = |input: &KernelRunInput| {
        input
            .admissions
            .values()
            .map(|held| (held.admission.identity().clone(), held.by))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        held(&input),
        vec![(element(0), 0), (element(1), 0)],
        "each admission names the run that issued it"
    );

    // The first settles while that run is in flight: nothing new is asked.
    let (state, action) = settle(state, &first, completed("\"a\"".to_owned()));
    assert_eq!(action, EngineAction::Idle);

    // The run parks having asked for nothing: the queued outcome runs it
    // again, and it is the only one delivered.
    let (state, action) = settle(state, &run1, parked(vec![], vec![]));
    let (run2, input, _) = run_of(&action);
    assert_eq!(
        input.deliver.iter().map(|d| d.wait).collect::<Vec<_>>(),
        vec![lash_kernel_value::sites::testkit::wait_id(10)]
    );
    assert_eq!(
        held(&input),
        vec![(element(0), 0)],
        "a consumed wait's admission goes"
    );
    assert_eq!(
        input.deliver[0].outcome,
        OutcomeWire::Completed(lash_kernel_doc::Datum::Text("a".to_owned()))
    );

    let outcome = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!(["a", "b"])),
    );
    let (state, action) = settle(
        state,
        &run2,
        completed(
            serde_json::to_string(&KernelRunOutput::Ended {
                read_facts: Vec::new(),
                outcome: Box::new(outcome.clone()),
                ran: Default::default(),
                caught: Vec::new(),
                reads: None,
                start: None,
            })
            .expect("a run output serializes"),
        ),
    );
    assert_eq!(action, EngineAction::Terminal(outcome));
    assert_eq!(state.pages, None);
    assert_eq!(
        decode(KernelVersion::NEWEST, &state)
            .expect("the state decodes")
            .0
            .phase,
        Phase::Ended
    );
}

/// An effect the boundary refuses and a cancel_requests wait dispatch nothing:
/// the refusal is raised at its `perform` by the next run, a `sleep` is a
/// timer the process wakes for, and the step of a wait the machine
/// withdrew settles into nothing.
/// Workflow admission's local names preserve the wrapper and every checker
/// error; hosts never need to parse a payload or display message for the code.
/// P2: admitting an entry does not require the effects of main or siblings.
/// FIG-6431 / S1: HTTP engine settings round-trip strictly to the recorded
/// settings; an unknown key or invalid typed value is never ignored.
#[test]
fn engine_settings_decode_strictly_to_recorded_settings() {
    use super::KernelRecordedSettings;
    let recorded = KernelRecordedSettings::new(
        "typescript",
        lash_kernel_vm::Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: 10_000,
            memory: 1024 * 1024,
            call_depth: 32,
            live_tasks: 16,
            requests_per_park: 16,
            join_members: 16,
        },
    )
    .observing(lash_kernel_value::observation::RunObservation::Counts);
    let engine = recorded.clone().into_engine_settings();
    assert_eq!(
        KernelRecordedSettings::from_engine_settings(&engine).unwrap(),
        recorded
    );
    let mut missing = engine.value().clone();
    missing.as_object_mut().unwrap().remove("memory_meter");
    assert!(
        KernelRecordedSettings::from_engine_settings(
            &lash_execution_contracts::process::EngineSettings::new(missing)
        )
        .is_err(),
        "recorded policy is required, never defaulted on read"
    );
    let mut unknown = engine.value().clone();
    unknown["unknown"] = serde_json::json!(true);
    assert!(
        KernelRecordedSettings::from_engine_settings(
            &lash_execution_contracts::process::EngineSettings::new(unknown)
        )
        .is_err()
    );
    for (key, value) in [
        ("charge_limit", serde_json::json!(-1)),
        ("observe", serde_json::json!("invalid")),
    ] {
        let mut invalid = engine.value().clone();
        invalid[key] = value;
        assert!(
            KernelRecordedSettings::from_engine_settings(
                &lash_execution_contracts::process::EngineSettings::new(invalid)
            )
            .is_err()
        );
    }
}

/// FIG-7505: an escaped helper uses its defining Link's fill for reads and
/// waited calls, including after a cold checkpoint restore.
#[test]
fn a_helper_called_from_another_link_keeps_its_defining_fills() {
    use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
    use lash_kernel_doc::{Action, Callee, Closure, Datum, Expr, FunctionExport, Rhs, Stmt};
    use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
    use lash_kernel_value::*;
    use lash_kernel_vm::{Host, KernelMachine, Machine, Outcome, Request, Step};
    use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};

    #[derive(Clone, Default)]
    struct Pages(Rc<RefCell<BTreeMap<PageId, PageAnswer>>>);
    impl PageSource for Pages {
        fn read(
            &mut self,
            pages: &[PageRef],
        ) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
            Ok(pages
                .iter()
                .map(|page| {
                    self.0
                        .borrow()
                        .get(&page.id)
                        .cloned()
                        .ok_or(HeapFault::Missing { page: page.id })
                })
                .collect())
        }
        fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
            let mut bytes = body.kind.0.to_be_bytes().to_vec();
            bytes.extend_from_slice(&body.tokens);
            let page = PageRef {
                id: PageId(lash_kernel_doc::heap_values::key_digest(&bytes)),
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
    struct Reads {
        a: LinkId,
        seen: Vec<OperationUse>,
    }
    impl Host for Reads {
        fn print(&mut self, _: &Datum) {}
        fn cancel_requested(&mut self) -> bool {
            false
        }
        fn perform(
            &mut self,
            use_: &OperationUse,
            _: &[Datum],
            _: &lash_kernel_value::sites::PositionLabel,
            _: &lash_kernel_value::sites::Site,
        ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, lash_kernel_vm::HostFault> {
            self.seen.push(*use_);
            Ok(Ok(Datum::Int(
                if use_.link == self.a { 11 } else { 22 }.into(),
            )))
        }
    }
    let helper = lash_kernel_doc::parse_definition("function helper.read() -> Int\nkernel 1\nmetering statements\nimport 0 operation service.read() -> Int\nbody { return perform 0() as Int }").unwrap();
    let helper_id = helper.identity();
    let import = helper.body().unwrap().imports[&ImportSlot(0)].clone();
    let operation = HelperOperationId::of_import(&import);
    let mut factory = lash_kernel_doc::parse_definition(
        "function local.make() -> Any\nkernel 1\nmetering statements\nbody { return null }",
    )
    .unwrap();
    if let lash_kernel_doc::Implementation::Guest { body, .. } = &mut factory.implementation {
        body.functions.insert(helper_id, helper.name.clone());
        body.block = vec![Stmt::Return {
            value: Expr::Closure(Box::new(Closure {
                function: helper_id,
                presentation: None,
            })),
        }];
    }
    let factory_id = factory.identity();
    let mut a = lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    a.main = None;
    a.functions.insert(factory_id, factory.clone());
    a.library.insert(helper_id);
    a.exports.insert(
        Name::new("make"),
        lash_kernel_doc::Export::Function(FunctionExport {
            function: factory_id,
            signature: factory.signature.clone(),
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    let binding = |contract| {
        BTreeMap::from([(
            operation,
            HelperFill {
                import: import.clone(),
                fill: Fill::Operation(OperationRequirement::new(
                    Signature {
                        params: vec![],
                        result: Type::Int,
                    },
                    ContractId::new(contract),
                )),
            },
        )])
    };
    let a_link = Link {
        document: a.identity(),
        fills: BTreeMap::new(),
        helper_fills: binding("context.a"),
    };
    let mut b = lash_kernel_doc::Document::new(
        lash_kernel_doc::NumberPolicy::BySpelling,
        vec![
            Stmt::Let {
                name: Name::new("callback"),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Import(ImportSlot(0)),
                    args: vec![],
                }),
            },
            Stmt::Let {
                name: Name::new("from_a"),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Value(Name::new("callback")),
                    args: vec![],
                }),
            },
            Stmt::Let {
                name: Name::new("from_b"),
                value: Rhs::Action(Action::Call {
                    callee: Callee::Local(helper_id),
                    args: vec![],
                }),
            },
            Stmt::Return {
                value: Expr::List(vec![
                    Expr::Variable(Name::new("from_a")),
                    Expr::Variable(Name::new("from_b")),
                ]),
            },
        ],
    );
    b.library.insert(helper_id);
    b.imports.insert(
        ImportSlot(0),
        Import {
            name: QualifiedName {
                package: PackagePath::parse("library").unwrap(),
                member: Segment::new("make").unwrap(),
                type_member: None,
            },
            kind: ImportKind::Code {
                signature: factory.signature,
                convention: Convention::Direct,
            },
        },
    );
    let a_id = a_link.identity();
    let b_link = Link {
        document: b.identity(),
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Export {
                link: a_id,
                export: ExportName {
                    name: Name::new("make"),
                    member: None,
                },
            },
        )]),
        helper_fills: binding("context.b"),
    };
    let b_id = b_link.identity();
    let mut registry = lash_kernel_doc::FunctionRegistry::default();
    registry.register(helper, None).unwrap();
    let helpers =
        Arc::new(lash_kernel_doc::BuildHelpers::new(registry.clone(), [helper_id]).unwrap());
    let library = lash_kernel_vm::PreparedLibrary::with_helpers(Arc::new(registry), helpers);
    let program = lash_kernel_vm::Program::prepare(
        b_id,
        BTreeMap::from([(a.identity(), Arc::new(a)), (b.identity(), Arc::new(b))]),
        &BTreeMap::from([(a_id, a_link), (b_id, b_link)]),
        PageId([8; 32]),
        library,
        lash_kernel_doc::Selection::Fresh,
    )
    .unwrap();
    let use_a = OperationUse {
        link: a_id,
        slot: OperationSlot::Helper(operation),
    };
    let use_b = OperationUse {
        link: b_id,
        slot: OperationSlot::Helper(operation),
    };
    for read in [false, true] {
        let pages = Pages::default();
        let mut host = Reads {
            a: a_id,
            seen: vec![],
        };
        let mut start = lash_kernel_vm::CellStart {
            owner: PageId([9; 32]),
            target: lash_kernel_vm::Target::Main,
            args: vec![],
            session: None,
            modes: BTreeMap::new(),
            host: vec![],
        };
        if read {
            start
                .modes
                .extend([(use_a, OperationMode::Read), (use_b, OperationMode::Read)]);
        }
        let bounds = lash_kernel_vm::Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: 1_000_000,
            memory: 64 << 20,
            call_depth: 100,
            live_tasks: 10,
            requests_per_park: 10,
            join_members: 10,
        };
        let mut machine =
            KernelMachine::start(program.clone(), bounds, start, Box::new(pages.clone())).unwrap();
        let mut waited = vec![];
        loop {
            match machine.run(&mut host, u64::MAX).unwrap() {
                Step::Slice => {}
                Step::Parked(park) => {
                    let [Request::Effect(request)] = park.requests.as_slice() else {
                        panic!("one helper operation")
                    };
                    waited.push(request.use_);
                    let checkpoint = machine.checkpoint().unwrap().checkpoint();
                    drop(machine);
                    let mut restored = program.clone();
                    restored.selection =
                        lash_kernel_doc::Selection::Restored(lash_kernel_doc::RestoreSelection {
                            live: [a_id, b_id]
                                .into_iter()
                                .map(|link| CallableId {
                                    link,
                                    function: helper_id,
                                })
                                .collect(),
                            pins: [helper_id].into(),
                        });
                    machine =
                        KernelMachine::open(restored, bounds, &checkpoint, Box::new(pages.clone()))
                            .unwrap();
                    machine
                        .deliver(
                            request.wait,
                            Outcome::Completed(Datum::Int(
                                if request.use_.link == a_id { 11 } else { 22 }.into(),
                            )),
                        )
                        .unwrap();
                }
                Step::Ended(lash_kernel_vm::End::Finished(done)) => {
                    assert_eq!(
                        done.result,
                        Datum::List(vec![Datum::Int(11.into()), Datum::Int(22.into())])
                    );
                    assert_eq!(if read { &host.seen } else { &waited }, &[use_a, use_b]);
                    break;
                }
                other => panic!("unexpected helper result: {other:?}"),
            }
        }
    }
}

/// KERNEL-BOUND-FACT: process projection preserves received and missing receipts.
#[test]
fn a_process_charge_bound_retains_limit_observation_and_consumption() {
    for charged in [None, Some(101)] {
        let exceeded = lash_kernel_vm::BoundExceeded {
            bound: lash_kernel_vm::Bound::Charge,
            limit: 100,
            function: None,
            site: None,
        };
        let meters = charged.map(|charged| lash_kernel_vm::Meters {
            charged: Some(charged),
            memory: 0,
            live_tasks: 0,
        });
        let output = super::run::outcome_of_end(
            lash_kernel_vm::End::Error(lash_kernel_vm::RunError::Bound(exceeded)),
            meters,
        );
        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output
        else {
            panic!("a bound ends the process");
        };
        let lash_core::ToolCallOutcome::Failure(failure) = output.outcome else {
            panic!("a bound fails the process");
        };
        let Some(lash_core::ToolFailureCause::ExecutionBound { fact }) = failure.cause.as_deref()
        else {
            panic!("the process retains its bound: {failure:?}");
        };
        assert_eq!(fact.resource, lash_core::CellResource::Charges);
        assert_eq!(fact.limit, 100);
        assert_eq!(fact.observed, charged);
        assert_eq!(fact.consumed, charged.map(|_| 100));
    }
}
