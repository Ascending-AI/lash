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

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::checkpoint_graph::NodeId;
use lash_kernel_doc::heap::PageId;
use lash_kernel_doc::{Datum, ErrorDatum, Integer};

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

/// Nothing committed: a fresh cell reads only what it stages.
struct Uncommitted;

impl CommittedPages for Uncommitted {
    fn page(&self, _page: &NodeId) -> Option<Arc<[u8]>> {
        None
    }
}

/// How a run ended, what it printed, what it was charged and how often it
/// parked.
#[derive(Debug, PartialEq)]
struct Ran {
    end: End,
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
    let modes = if read {
        [(use_, lash_kernel_value::OperationMode::Read)].into()
    } else {
        EffectModes::new()
    };
    let start = CellStart {
        owner: PageId([0; 32]),
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
    let mut machine = lash_kernel_vm::testing::start(
        program.clone(),
        bounds,
        start,
        strategy,
        Box::new(LocalPages::new(
            Uncommitted,
            Box::new(spools.spool(next_stage_lease()).expect("spool")),
        )),
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
                return Ran {
                    end,
                    prints: host.prints,
                    charged: machine.meters().charged.expect("exact charge"),
                    parks,
                    calls: host.calls,
                };
            }
            Step::Parked(park) => {
                parks += 1;
                for request in park.requests {
                    let Request::Effect(request) = request;
                    assert_eq!(request.use_, use_, "the cell performs only its tool");
                    host.calls.push(request.args.clone());
                    let outcome = match answer_at(&request.args, answer) {
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
    let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
    let effects = BTreeMap::from([(
        effect.clone(),
        lash_kernel_value::json_schema::tool_signature(
            &lash_kernel_value::JsonSchema::admit(input).expect("an admissible input schema"),
            &lash_kernel_value::JsonSchema::admit(output).expect("an admissible result schema"),
        ),
    )]);
    let library = embedding.library();
    let lowered = embedding
        .lower(
            dialect,
            source,
            &lash_kernel_dialect::Environment::new(
                library,
                &crate::embedding::tests::namespaces(&effects, &Default::default())
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
    cenote_read_loop();
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

// FIG-7513: the cell keeps source operands across the entry helper's calls,
// and executes exactly the reads from its loop and taken branch.
fn cenote_read_loop() {
    const SOURCE: &str = r#"const pages = ['plans', 'seats', 'archive'];
for (const [i, path] of pages.entries()) {
  if (i === 0) await cenote.readDocument({ document: { collection: 'wiki', path: 'pricing' } });
  try { await cenote.readDocument({ document: { collection: 'wiki', path } }); } catch {}
}
if (pages.length > 3) await cenote.readDocument({ document: { collection: 'wiki', path: 'never' } });"#;
    let effect = EffectName::new("cenote.readDocument").expect("the tool name");
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
        let observation = (read.end, read.prints, read.calls, read.charged);
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
