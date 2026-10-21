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
use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer};
use lash_kernel_vm::{
    CellStart, EffectModes, End, Host, HostFault, KernelMachine, Machine, Outcome, Program,
    Request, Step, Target,
};
use lash_vm_broker::kernel::next_stage_lease;
use lash_vm_broker::kernel::pages::{CommittedPages, LocalPages, SpoolRoot};
use lash_vm_client::WorkerTuning;
use lash_vocabulary::legacy_names::EffectName;

use crate::embedding::tests::every_dialect;

/// A host that answers the run's one read with `answer`, and nothing else.
struct Answering {
    answer: Option<Result<Datum, ErrorDatum>>,
    prints: Vec<Datum>,
}

impl Host for Answering {
    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("the cell reads no projection")
    }

    fn perform(
        &mut self,
        _effect: &lash_vocabulary::OperationUse,
        _args: &[Datum],
        _site: &lash_vocabulary::sites::PositionLabel,
        _origin: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
        Ok(self.answer.take().expect("the cell reads once"))
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
}

/// Runs `program` with `effect` bound as a read when `read`, answering its
/// one `perform` with `answer` inside its statement or at its park.
fn run(
    program: &Program,
    effect: &EffectName,
    read: bool,
    answer: &Result<Datum, ErrorDatum>,
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
        [(use_, lash_vocabulary::OperationMode::Read)].into()
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
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds,
        start,
        Box::new(LocalPages::new(
            Uncommitted,
            Box::new(spools.spool(next_stage_lease()).expect("spool")),
        )),
    )
    .expect("the cell starts");
    let mut host = Answering {
        answer: read.then(|| answer.clone()),
        prints: Vec::new(),
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
                };
            }
            Step::Parked(park) => {
                parks += 1;
                for request in park.requests {
                    let Request::Effect(request) = request else {
                        panic!("the cell sleeps nowhere");
                    };
                    assert_eq!(request.use_, use_, "the cell performs only its tool");
                    let outcome = match answer {
                        Ok(value) => Outcome::Completed(value.clone()),
                        Err(error) => Outcome::Failed(error.clone()),
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
    let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
    let effects = BTreeMap::from([(
        effect.clone(),
        lash_protocol_types::tool_signature(
            &lash_protocol_types::JsonSchema::admit(serde_json::json!({
                "type": "object",
                "properties": { "x": { "type": "number" } }
            }))
            .expect("an admissible input schema"),
            &lash_protocol_types::JsonSchema::admit(serde_json::json!({ "type": "integer" }))
                .expect("an admissible result schema"),
        ),
    )]);
    let library = embedding.library();
    let lowered = embedding
        .lower(
            dialect,
            source,
            &lash_kernel_dialect::Environment::new(
                library,
                &lash_kernel_dialect::Namespaces::of_effects(
                    &effects,
                    &lash_vocabulary::operations::ContractId::new("1"),
                    &Default::default(),
                )
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
