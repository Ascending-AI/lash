//! READ-EXACT-CHARGE (ADR 0144, `K-EFF-012`): a `perform` the run binds as
//! a read is charged exactly as the same `perform` recorded, in both
//! dialects, whether it answers, raises its error or answers what its
//! declared type refuses. Each cell is lowered as a host lowers it and run
//! twice on the machine: once with its tool's effect bound as a read and
//! answered inside its statement, once recorded and answered at its park.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a law fails on a broken fixture assumption"
)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use lash_core_store::checkpoint_graph::NodeId;
use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_doc::{Datum, ErrorDatum, Integer};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};

use lash_kernel_value::names::EffectName;
use lash_kernel_vm::{
    CellStart, EffectModes, End, Host, HostFault, Machine, Outcome, Program, Request, Step, Target,
};
use lash_vm_broker::kernel::next_stage_lease;
use lash_vm_broker::kernel::pages::{CommittedPages, LocalPages, SpoolRoot};
use lash_vm_client::WorkerTuning;

use crate::embedding::tests::every_dialect;

/// A host that answers the run's one read with `answer`, and nothing else.
struct Answering {
    answer: Option<Result<Datum, ErrorDatum>>,
    prints: Vec<Datum>,
    calls: Vec<Vec<Datum>>,
}

impl Host for Answering {
    fn perform(
        &mut self,
        _effect: &lash_kernel_value::OperationUse,
        args: &[Datum],
        _site: &lash_kernel_value::sites::PositionLabel,
        _origin: &lash_kernel_value::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
        self.calls.push(args.to_vec());
        Ok(answer_at(
            args,
            self.answer.as_ref().expect("the cell binds its read"),
        ))
    }

    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// Admitted code nodes are committed; execution pages are freshly staged.
#[derive(Default)]
struct Committed(BTreeMap<NodeId, Arc<[u8]>>);

impl CommittedPages for Committed {
    fn page(&self, page: &NodeId) -> Option<Arc<[u8]>> {
        self.0.get(page).cloned()
    }
}

#[derive(Clone)]
struct SharedPages(Rc<RefCell<LocalPages<Committed>>>);
impl PageSource for SharedPages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.0.borrow_mut().read(pages)
    }
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        self.0.borrow_mut().seal(page)
    }
}

/// How a run ended, what it printed, what it was charged and how often it
/// parked.
#[derive(Debug, PartialEq)]
struct Ran {
    end: End,
    carry: Option<lash_kernel_state::semantic::Checkpoint>,
    prints: Vec<Datum>,
    charged: u64,
    parks: usize,
    calls: Vec<Vec<Datum>>,
}

/// Runs `program` with `effect` bound as a read when `read`, answering its
/// one `perform` with `answer` inside its statement or at its park.
fn run(
    program: &Program,
    effect: &EffectName,
    read: bool,
    answer: &Result<Datum, ErrorDatum>,
) -> Ran {
    run_in(
        program,
        effect,
        read,
        answer,
        lash_kernel_vm::testing::ExecutionStrategy::Optimized,
    )
}
fn run_in(
    program: &Program,
    effect: &EffectName,
    read: bool,
    answer: &Result<Datum, ErrorDatum>,
    strategy: lash_kernel_vm::testing::ExecutionStrategy,
) -> Ran {
    run_scripted(
        program,
        effect,
        read,
        answer,
        strategy,
        &BTreeMap::new(),
        false,
    )
}
fn run_scripted(
    program: &Program,
    effect: &EffectName,
    read: bool,
    answer: &Result<Datum, ErrorDatum>,
    strategy: lash_kernel_vm::testing::ExecutionStrategy,
    replies: &BTreeMap<EffectName, Result<Datum, ErrorDatum>>,
    reopen: bool,
) -> Ran {
    let instance = &program.instances[&program.link];
    let slot = program
        .document
        .imports
        .iter()
        .find_map(|(slot, import)| (import.name.to_string() == effect.as_str()).then_some(*slot))
        .expect("tool import");
    let use_ = match &instance.slots[&slot] {
        lash_kernel_vm::PreparedSlot::Operation(use_) => *use_,
        _ => panic!("operation slot"),
    };
    let scripted: Vec<_> = replies
        .iter()
        .map(|(name, reply)| {
            let slot = program
                .document
                .imports
                .iter()
                .find_map(|(slot, import)| {
                    (import.name.to_string() == name.as_str()).then_some(*slot)
                })
                .expect("scripted tool import");
            let lash_kernel_vm::PreparedSlot::Operation(use_) = instance.slots[&slot] else {
                panic!("scripted operation slot")
            };
            (use_, reply)
        })
        .collect();
    let modes = if read {
        [(use_, lash_kernel_value::OperationMode::Read)].into()
    } else {
        EffectModes::new()
    };
    let start = CellStart {
        owner: if reopen {
            program.closure
        } else {
            PageId([0; 32])
        },
        target: Target::Main,
        args: Vec::new(),
        session: None,
        modes,
        host: Vec::new(),
    };
    let spools = tempfile::tempdir().expect("spool directory");
    let spools = SpoolRoot::open(spools.path(), 1 << 30).expect("spools");
    let bounds = lash_kernel_vm::Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 1_000_000,
        memory: 16 << 20,
        call_depth: 64,
        live_tasks: 16,
        requests_per_park: 16,
        join_members: 16,
    };
    let committed = if reopen {
        let stored = program
            .documents
            .values()
            .flat_map(|document| document.functions.iter())
            .map(|(id, definition)| (*id, Arc::new(definition.clone())))
            .collect();
        let closure = lash_vm_broker::kernel::closure::derive(
            program
                .functions
                .iter()
                .chain(&program.document.requires.natives)
                .copied(),
            program.library.registry(),
            &stored,
            program.helpers(),
            &program.document.library,
        )
        .expect("the admitted closure derives");
        assert_eq!(closure.reference().0, program.closure.0);
        Committed(
            closure
                .nodes()
                .iter()
                .map(|(id, node)| (*id, Arc::clone(&node.bytes)))
                .collect(),
        )
    } else {
        Committed::default()
    };
    let pages = SharedPages(Rc::new(RefCell::new(LocalPages::new(
        committed,
        Box::new(spools.spool(next_stage_lease()).expect("spool")),
    ))));
    let mut machine = lash_kernel_vm::testing::start(
        program.clone(),
        bounds,
        start,
        strategy,
        Box::new(pages.clone()),
    )
    .expect("the cell starts");
    let mut host = Answering {
        answer: read.then(|| answer.clone()),
        prints: Vec::new(),
        calls: Vec::new(),
    };
    let mut parks = 0;
    loop {
        match machine.run(&mut host, u64::MAX).expect("the run steps") {
            Step::Slice => {}
            Step::Ended(end) => {
                let carry = match &end {
                    End::Finished(finished) => finished.carry.as_ref().map(|root| {
                        lash_kernel_state::semantic::session(root, Box::new(pages.clone()))
                            .expect("the carried session projects")
                    }),
                    _ => None,
                };
                return Ran {
                    end,
                    carry,
                    prints: host.prints,
                    charged: machine.meters().charged.expect("exact charge"),
                    parks,
                    calls: host.calls,
                };
            }
            Step::Parked(park) => {
                parks += 1;
                if reopen {
                    let sealed = machine
                        .seal(lash_kernel_state::paged::SealBudget {
                            pages: u64::MAX,
                            bytes: u64::MAX,
                        })
                        .expect("the parked cell seals")
                        .expect("unbounded seal");
                    machine = lash_kernel_vm::testing::open(
                        program.clone(),
                        bounds,
                        &sealed.checkpoint(),
                        Box::new(pages.clone()),
                        strategy,
                    )
                    .expect("the discovery cell reopens");
                }
                for request in park.requests {
                    let Request::Effect(request) = request;
                    host.calls.push(request.args.clone());
                    let reply = if request.use_ == use_ {
                        answer_at(&request.args, answer)
                    } else {
                        scripted
                            .iter()
                            .find(|(use_, _)| *use_ == request.use_)
                            .expect("the cell performs only a declared scripted tool")
                            .1
                            .clone()
                    };
                    let outcome = match reply {
                        Ok(value) => Outcome::Completed(value),
                        Err(error) => Outcome::Failed(error),
                    };
                    machine
                        .deliver(request.wait, outcome)
                        .expect("the outcome is delivered");
                }
            }
        }
    }
}

/// The cell `source`, written in `dialect`, lowered against one tool,
/// `effect`.
fn lowered(dialect: &str, source: &str, effect: &EffectName) -> Program {
    lowered_with_schemas(
        dialect,
        source,
        effect,
        serde_json::json!({"type":"object", "properties":{"x":{"type":"number"}}}),
        serde_json::json!({"type":"integer"}),
    )
}
fn lowered_with_schemas(
    dialect: &str,
    source: &str,
    effect: &EffectName,
    input: serde_json::Value,
    output: serde_json::Value,
) -> Program {
    let effects = BTreeMap::from([(
        effect.clone(),
        lash_kernel_value::json_schema::tool_signature(
            &lash_kernel_value::JsonSchema::admit(input).expect("an admissible input schema"),
            &lash_kernel_value::JsonSchema::admit(output).expect("an admissible result schema"),
        ),
    )]);
    lowered_against(dialect, source, &effects, &BTreeMap::new())
}
fn lowered_against(
    dialect: &str,
    source: &str,
    effects: &BTreeMap<EffectName, lash_kernel_value::Signature>,
    controls: &BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>,
) -> Program {
    let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
    let library = embedding.library();
    let lowered = embedding
        .lower(
            dialect,
            source,
            &lash_kernel_dialect::Environment::new(
                library,
                &crate::embedding::tests::namespaces(effects, controls)
                    .expect("the test's effects are operations"),
            ),
            lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first()),
        )
        .expect("the dialect is installed")
        .unwrap_or_else(|diagnostic| panic!("the {dialect} cell lowers: {diagnostic:?}"));
    let document = lowered.document.into_document();
    let library = embedding
        .prepared_library()
        .expect("the document's library")
        .clone();
    crate::embedding::tests::linked(document, library)
}

#[test]
fn a_read_is_charged_exactly_as_the_same_perform_recorded_in_both_dialects() {
    cenote_parallel_discovery();
    cenote_read_loop();
    figments_workflow_loop();
    let typescript = (
        "typescript",
        "let r = 0;\n\
         try { r = await tools.look({ x: 1 }); } catch (e) { r = -1; }\n\
         console.log(r);",
        "tools.look",
    );
    let python = (
        "python",
        "r = 0\n\
         try:\n    r = await tools.look({\"x\": 1})\n\
         except Exception as e:\n    r = -1\n\
         print(r)",
        "tools.look",
    );
    let answers = [
        ("an answer", Ok(Datum::Int(Integer::from(7)))),
        (
            "an error",
            Err(ErrorDatum {
                kind: "tool_failed".to_owned(),
                message: "the read failed".to_owned(),
                data: Datum::Null,
            }),
        ),
        (
            "an answer its type refuses",
            Ok(Datum::Text("seven".to_owned())),
        ),
    ];
    for (dialect, source, effect) in [typescript, python] {
        let effect = EffectName::new(effect).expect("a tool's effect name");
        let program = lowered(dialect, source, &effect);
        for (what, answer) in &answers {
            let read = run(&program, &effect, true, answer);
            let recorded = run(&program, &effect, false, answer);
            assert_eq!(read.calls.len(), 1, "{dialect}, {what}: one actual read");
            assert_eq!(
                recorded.calls.len(),
                1,
                "{dialect}, {what}: one recorded call"
            );
            assert_eq!(read.parks, 0, "{dialect}, {what}: a read does not park");
            assert_eq!(
                recorded.parks, 1,
                "{dialect}, {what}: a recorded perform parks"
            );
            assert_eq!(
                (&read.end, &read.prints, read.charged),
                (&recorded.end, &recorded.prints, recorded.charged),
                "{dialect}, {what}: a read ends, prints and is charged as the same perform recorded"
            );
        }
    }
}

// Cenote's parallel discovery cells must retain the same source owners when
// their spawned calls resume from a cold checkpoint.
fn cenote_parallel_discovery() {
    let describe = EffectName::new("tools.describe").expect("describe operation");
    let find = EffectName::new("tools.find").expect("find operation");
    let signature = |input, output| {
        lash_kernel_value::json_schema::tool_signature(
            &lash_kernel_value::JsonSchema::admit(input).expect("input schema"),
            &lash_kernel_value::JsonSchema::admit(output).expect("output schema"),
        )
    };
    let effects = BTreeMap::from([
        (
            describe.clone(),
            signature(
                serde_json::json!({"type":"string"}),
                serde_json::json!({"type":"object", "properties":{"contract":{"type":"string"}}, "required":["contract"]}),
            ),
        ),
        (
            find.clone(),
            signature(
                serde_json::json!({"type":"object", "properties":{"query":{"type":"string"}}, "required":["query"]}),
                serde_json::json!({"type":"object", "properties":{"found":{"type":"array", "items":{"type":"string"}}}, "required":["found"]}),
            ),
        ),
    ]);
    let described = Ok(Datum::Record(vec![(
        "contract".into(),
        Datum::Text("contract".into()),
    )]));
    let replies = BTreeMap::from([(
        find,
        Ok(Datum::Record(vec![(
            "found".into(),
            Datum::List(vec![Datum::Text("workflows.sdk".into())]),
        )])),
    )]);
    let cells = [
        (
            "const details = await Promise.all([tools.describe('blocks.add'), tools.find({query:'workflows.sdk'})]); console.log(JSON.stringify(details));",
            vec![Datum::Text(
                r#"[{"contract":"contract"},{"found":["workflows.sdk"]}]"#.into(),
            )],
            replies,
        ),
        (
            "const descs = await Promise.all([tools.describe('cenote.startBlockRun'),tools.describe('cenote.readBlock')]); for (const d of descs) { const s=d.contract; console.log(s.slice(0,5000)); }",
            vec![Datum::Text("contract".into()); 2],
            BTreeMap::new(),
        ),
    ];
    for (source, prints, replies) in cells {
        let program = lowered_against("typescript", source, &effects, &BTreeMap::new());
        // A synchronous read may settle a spawned task before its join, so
        // compare exact source charges within each scheduling mode.
        let mut baselines = [None, None];
        for strategy in [
            lash_kernel_vm::testing::ExecutionStrategy::Optimized,
            lash_kernel_vm::testing::ExecutionStrategy::Interpreted,
        ] {
            let mut program = program.clone();
            program.library = lash_kernel_vm::testing::library(&program.library, strategy);
            for as_read in [true, false] {
                let ran = run_scripted(
                    &program, &describe, as_read, &described, strategy, &replies, true,
                );
                let End::Finished(finished) = ran.end else {
                    panic!("{strategy:?}, read={as_read}: {:?}", ran.end)
                };
                assert_eq!(
                    ran.calls.len(),
                    2,
                    "both discovery calls execute exactly once"
                );
                assert_eq!(ran.prints, prints);
                let observation = (
                    finished.result,
                    finished.finish,
                    finished.not_carried,
                    ran.carry,
                    ran.prints,
                    ran.calls,
                    ran.charged,
                );
                let baseline = &mut baselines[usize::from(as_read)];
                if let Some(baseline) = baseline {
                    assert_eq!(
                        &observation, baseline,
                        "parallel discovery preserves exact charge across execution strategies"
                    );
                } else {
                    *baseline = Some(observation);
                }
            }
        }
    }
}

// FIG-7513: the cell keeps source operands across the entry helper's calls,
// and executes exactly the reads from its loop and taken branch.
fn cenote_read_loop() {
    const SOURCE: &str = r#"const pages = ['plans', 'seats', 'archive'];
for (const [i, path] of pages.entries()) {
  if (i === 0) await cenote.readDocument({ document: { collection: 'wiki', path: 'pricing' } });
  try { await cenote.readDocument({ document: { collection: 'wiki', path } }); } catch {}
}
if (pages.length > 3) await cenote.readDocument({ document: { collection: 'wiki', path: 'never' } });"#;
    let effect = EffectName::new("cenote.read_document").expect("the tool name");
    let program = lowered_with_schemas(
        "typescript",
        SOURCE,
        &effect,
        serde_json::json!({"type":"object", "properties":{"document":{"type":"object", "properties":{"collection":{"type":"string"}, "path":{"type":"string"}}, "required":["collection","path"]}}, "required":["document"]}),
        serde_json::json!({"type":"object"}),
    );
    let page = Ok(Datum::Record(vec![(
        "ok".into(),
        Datum::Record(vec![
            ("version".into(), Datum::Int(Integer::from(1))),
            (
                "text".into(),
                Datum::Text("Pricing is per seat, billed monthly.".into()),
            ),
        ]),
    )]));
    let mut baseline = None;
    for strategy in [
        lash_kernel_vm::testing::ExecutionStrategy::Optimized,
        lash_kernel_vm::testing::ExecutionStrategy::Interpreted,
    ] {
        let mut program = program.clone();
        program.library = lash_kernel_vm::testing::library(&program.library, strategy);
        let read = run_in(&program, &effect, true, &page, strategy);
        let recorded = run_in(&program, &effect, false, &page, strategy);
        assert!(
            matches!(read.end, End::Finished(_)),
            "{strategy:?}: {read:?}"
        );
        assert_eq!(
            read.calls
                .iter()
                .map(|args| read_path(args).expect("document path"))
                .collect::<Vec<_>>(),
            ["pricing", "plans", "seats", "archive"]
        );
        assert_eq!(read.parks, 0);
        assert_eq!(recorded.parks, 4);
        assert_eq!(
            (&read.end, &read.prints, &read.calls, read.charged),
            (
                &recorded.end,
                &recorded.prints,
                &recorded.calls,
                recorded.charged
            )
        );
        let End::Finished(finished) = read.end else {
            panic!("the read loop finishes")
        };
        let observation = (
            finished.result,
            finished.finish,
            finished.not_carried,
            read.carry,
            read.prints,
            read.calls,
            read.charged,
        );
        if let Some(baseline) = &baseline {
            assert_eq!(&observation, baseline, "register and reference cells agree");
        } else {
            baseline = Some(observation);
        }
    }
}

fn read_path(args: &[Datum]) -> Option<&str> {
    let Datum::Record(fields) = args.first()? else {
        return None;
    };
    let Datum::Record(document) = &fields.iter().find(|(key, _)| key == "document")?.1 else {
        return None;
    };
    let Datum::Text(path) = &document.iter().find(|(key, _)| key == "path")?.1 else {
        return None;
    };
    Some(path)
}
fn answer_at(args: &[Datum], answer: &Result<Datum, ErrorDatum>) -> Result<Datum, ErrorDatum> {
    if read_path(args) == Some("archive") {
        Err(ErrorDatum {
            kind: "tool_failed".into(),
            message: "page unavailable".into(),
            data: Datum::Null,
        })
    } else {
        answer.clone()
    }
}

// FIG-7513: a member of an awaited result becomes the loop's iterable; its
// commands must run once each after the read, before the explicit finish.
fn figments_workflow_loop() {
    const SOURCE: &str = r#"const w = await workflows.read({workflow_id:'9e4adc8b-c31c-4f9d-9a60-6548bc0621b3'});
for (const trigger of w.triggers) {
  await workflows.removeTrigger({workflow_id:'9e4adc8b-c31c-4f9d-9a60-6548bc0621b3',trigger_id:trigger.trigger_id});
}
await control.finish("WORKFLOW_CHECK_PASSED");"#;
    let read = EffectName::new("workflows.read").expect("read operation");
    let remove = EffectName::new("workflows.remove_trigger").expect("remove operation");
    let signature = |input, output| {
        lash_kernel_value::json_schema::tool_signature(
            &lash_kernel_value::JsonSchema::admit(input).expect("input schema"),
            &lash_kernel_value::JsonSchema::admit(output).expect("output schema"),
        )
    };
    let mut effects = BTreeMap::from([
        (
            read.clone(),
            signature(
                serde_json::json!({"type":"object", "properties":{"workflow_id":{"type":"string"}}, "required":["workflow_id"]}),
                serde_json::json!({"type":"object", "properties":{"triggers":{"type":"array", "items":{"type":"object", "properties":{"trigger_id":{"type":"string"}}, "required":["trigger_id"]}}}, "required":["triggers"]}),
            ),
        ),
        (
            remove.clone(),
            signature(
                serde_json::json!({"type":"object", "properties":{"workflow_id":{"type":"string"}, "trigger_id":{"type":"string"}}, "required":["workflow_id","trigger_id"]}),
                serde_json::json!({"type":"null"}),
            ),
        ),
    ]);
    let finish = EffectName::new("control.finish").expect("finish operation");
    effects.insert(
        finish.clone(),
        signature(serde_json::json!({}), serde_json::json!({})),
    );
    let controls = BTreeMap::from([(
        finish.clone(),
        BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
    )]);
    let program = lowered_against("typescript", SOURCE, &effects, &controls);
    let workflow = Ok(Datum::Record(vec![(
        "triggers".into(),
        Datum::List(
            ["schedule-1", "schedule-2"]
                .into_iter()
                .map(|id| Datum::Record(vec![("trigger_id".into(), Datum::Text(id.into()))]))
                .collect(),
        ),
    )]));
    let replies = BTreeMap::from([(remove, Ok(Datum::Null)), (finish, Ok(Datum::Null))]);
    let mut baseline = None;
    for strategy in [
        lash_kernel_vm::testing::ExecutionStrategy::Optimized,
        lash_kernel_vm::testing::ExecutionStrategy::Interpreted,
    ] {
        let mut program = program.clone();
        program.library = lash_kernel_vm::testing::library(&program.library, strategy);
        for as_read in [true, false] {
            let ran = run_scripted(
                &program, &read, as_read, &workflow, strategy, &replies, false,
            );
            let End::Finished(finished) = ran.end else {
                panic!("{strategy:?}, read={as_read}: {:?}", ran.end)
            };
            assert_eq!(finished.result, Datum::Null);
            assert!(finished.finish);
            assert_eq!(ran.parks, if as_read { 3 } else { 4 });
            assert_eq!(ran.calls.len(), 4);
            assert_eq!(ran.calls[3], [Datum::Text("WORKFLOW_CHECK_PASSED".into())]);
            assert_eq!(
                ran.calls[..3]
                    .iter()
                    .map(|args| (
                        argument_text(args, "workflow_id").expect("workflow id"),
                        argument_text(args, "trigger_id"),
                    ))
                    .collect::<Vec<_>>(),
                [
                    ("9e4adc8b-c31c-4f9d-9a60-6548bc0621b3", None),
                    ("9e4adc8b-c31c-4f9d-9a60-6548bc0621b3", Some("schedule-1")),
                    ("9e4adc8b-c31c-4f9d-9a60-6548bc0621b3", Some("schedule-2")),
                ]
            );
            let observation = (
                finished.result,
                finished.finish,
                finished.not_carried,
                ran.carry,
                ran.prints,
                ran.calls,
                ran.charged,
            );
            if let Some(baseline) = &baseline {
                assert_eq!(
                    &observation, baseline,
                    "workflow loop strategies and read modes agree"
                );
            } else {
                baseline = Some(observation);
            }
        }
    }
}
fn argument_text<'a>(args: &'a [Datum], name: &str) -> Option<&'a str> {
    let Datum::Record(fields) = args.first()? else {
        return None;
    };
    let Datum::Text(value) = &fields.iter().find(|(key, _)| key == name)?.1 else {
        return None;
    };
    Some(value)
}
