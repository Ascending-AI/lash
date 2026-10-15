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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::{Datum, EffectName, ErrorDatum, Handle, Integer, Name, Timestamp};
use lash_kernel_vm::{
    Bindings, EffectModes, End, Host, HostFault, KernelMachine, Machine, Outcome, Program, Request,
    Start, Step, Target,
};
use lash_vm_client::WorkerTuning;

use crate::embedding::tests::every_dialect;

/// A host that answers the run's one read with `answer`, and nothing else.
struct Answering {
    answer: Option<Result<Datum, ErrorDatum>>,
    prints: Vec<Datum>,
}

impl Host for Answering {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        panic!("the cell reads no projection")
    }

    fn perform(
        &mut self,
        _effect: &EffectName,
        _args: &[Datum],
        _site: &lash_kernel_doc::Site,
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
    let modes = if read {
        EffectModes::reading([effect.clone()])
    } else {
        EffectModes::recorded()
    };
    let start = Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    };
    let mut machine = KernelMachine::start_reading(
        program.clone(),
        lash_kernel_vm::Bounds {
            charge: 1_000_000,
            memory: 16 << 20,
            call_depth: 64,
            live_tasks: 16,
            requests_per_park: 16,
            join_members: 16,
        },
        start,
        modes,
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
                    charged: machine.meters().charged,
                    parks,
                };
            }
            Step::Parked(park) => {
                parks += 1;
                for request in park.requests {
                    let Request::Effect(request) = request else {
                        panic!("the cell sleeps nowhere");
                    };
                    assert_eq!(&request.effect, effect, "the cell performs only its tool");
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
/// `effect`, under `root`.
fn lowered(dialect: &str, source: &str, effect: &EffectName, root: Option<&str>) -> Program {
    let embedding = every_dialect(&WorkerTuning::standard()).expect("an embedding of both");
    let effects = BTreeMap::from([(
        effect.clone(),
        lash_sansio::tool_signature(
            &lash_sansio::JsonSchema::admit(serde_json::json!({
                "type": "object",
                "properties": { "x": { "type": "number" } }
            }))
            .expect("an admissible input schema"),
            &lash_sansio::JsonSchema::admit(serde_json::json!({ "type": "integer" }))
                .expect("an admissible result schema"),
        ),
    )]);
    let tool_roots = root.into_iter().map(Name::new).collect::<BTreeSet<_>>();
    let library = embedding
        .library_for(lash_vm_library::HELPER_RELEASE)
        .expect("the build's helper release");
    let lowered = embedding
        .lower(
            dialect,
            source,
            &lash_kernel_dialect::Environment {
                library,
                effects: &effects,
                tool_roots: &tool_roots,
                controls: &BTreeMap::new(),
                bindings: &BTreeSet::new(),
                functions: &BTreeMap::new(),
            },
        )
        .expect("the dialect is installed")
        .unwrap_or_else(|diagnostic| panic!("the {dialect} cell lowers: {diagnostic:?}"));
    let document = lowered.document;
    let library = embedding
        .prepared_for(document.manifest.kernel)
        .expect("the document's library")
        .clone();
    Program {
        document: Arc::new(document),
        library,
    }
}

#[test]
fn a_read_is_charged_exactly_as_the_same_perform_recorded_in_both_dialects() {
    let typescript = (
        "typescript",
        "let r = 0;\n\
         try { r = await tools.look({ x: 1 }); } catch (e) { r = -1; }\n\
         console.log(r);",
        "tools.look",
        Some("tools"),
    );
    let python = (
        "python",
        "r = 0\n\
         try:\n    r = await tools_look({\"x\": 1})\n\
         except Exception as e:\n    r = -1\n\
         print(r)",
        "tools_look",
        None,
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
    for (dialect, source, effect, root) in [typescript, python] {
        let effect = EffectName::new(effect).expect("a tool's effect name");
        let program = lowered(dialect, source, &effect, root);
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
