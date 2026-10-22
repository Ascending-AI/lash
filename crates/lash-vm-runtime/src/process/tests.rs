use lash_core::tool_run::{MaterialOwner, MaterialRole};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_vm_client::wire::OutcomeWire;
use lash_vocabulary::names::DocumentId;
use lash_vocabulary::sites::EffectId;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::advance::{advance, decode, state_format};
use super::state::{
    Admission, Issued, KERNEL_RUN_STEP, KernelRunInput, KernelRunOutput, ParkedPage, ParkedRun,
    Phase, refused,
};
use lash_kernel_doc::KernelVersion;
use lash_kernel_state::{AdmittedRequest, AdmittedWait};
use std::collections::BTreeMap;

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
fn operation_use() -> lash_vocabulary::OperationUse {
    lash_vocabulary::OperationUse {
        link: lash_vocabulary::LinkId::from_bytes([1; 32]),
        slot: lash_vocabulary::ImportSlot(0),
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

fn parked(issued: Vec<Issued>, withdrawn: Vec<lash_vocabulary::sites::WaitId>) -> SettledOutput {
    completed(
        serde_json::to_string(&KernelRunOutput::Parked {
            state: sealed(),
            issued,
            withdrawn,
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
    lash_vocabulary::sites::testkit::effect(
        lash_vocabulary::sites::testkit::spawned_task(
            lash_vocabulary::sites::testkit::root_task(),
            Site::new(Unit::Main, vec![0]),
            element,
        ),
        Site::new(
            Unit::Function(lash_vocabulary::names::Name::new("worker")),
            vec![0],
        ),
        0,
        Vec::new(),
    )
}

fn effect(wait: u64, identity: EffectId) -> Issued {
    Issued::Effect {
        origin: identity.position().site().clone(),
        wait: lash_vocabulary::sites::testkit::wait_id(wait),
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
        vec![lash_vocabulary::sites::testkit::wait_id(11)]
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
        vec![lash_vocabulary::sites::testkit::wait_id(10)]
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

/// An effect the boundary refuses and a withdrawn wait dispatch nothing:
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
    .observing(lash_vocabulary::observation::RunObservation::Counts);
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

/// FIG-6447: a host reads every migration refusal as owned typed data,
/// both directly and after it has crossed the engine and actor boundaries.
fn migration_refusal_round_trip(
    refusal: super::KernelMigrationRefusal,
    tag: &str,
    cause: Option<&str>,
) {
    let encoded = serde_json::to_string(&refusal).expect("a refusal encodes");
    let decoded: super::KernelMigrationRefusal =
        serde_json::from_str(&encoded).expect("a host deserializes an owned refusal");
    assert_eq!(decoded, refusal);
    let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(value["refused"], tag);
    if let Some(cause) = cause {
        assert_eq!(value["refusal"]["reason"], cause);
    }
    let engine = lash_execution_contracts::process::EngineStateRefusal {
        code: "kernel_migration".into(),
        detail: serde_json::to_value(&refusal).unwrap(),
        message: refusal.to_string(),
    };
    let encoded = serde_json::to_string(&engine).unwrap();
    assert_eq!(
        serde_json::from_str::<lash_execution_contracts::process::EngineStateRefusal>(&encoded)
            .unwrap(),
        engine
    );
    let park = lash_execution_contracts::park::ProcessParkReason::MigrationRefused {
        kind: "lashvm".to_owned(),
        version: 1,
        refusal: engine.clone(),
        message: refusal.to_string(),
    };
    let decoded: lash_execution_contracts::park::ProcessParkReason =
        serde_json::from_str(&park.encode()).unwrap();
    assert_eq!(decoded, park);
    let lash_execution_contracts::park::ProcessParkReason::MigrationRefused {
        refusal: found, ..
    } = decoded
    else {
        panic!("a migration refusal stays a migration refusal");
    };
    assert_eq!(found, engine);
}

macro_rules! migration_refusal_law {
    ($name:ident, $variant:ident { $($field:ident: $value:expr),* $(,)? }, $tag:literal) => {
        #[test]
        fn $name() {
            migration_refusal_round_trip(
                super::KernelMigrationRefusal::$variant { $($field: $value),* },
                $tag,
                None,
            );
        }
    };
    ($name:ident, $outer:ident, $inner:ident, $variant:ident { $($field:ident: $value:expr),* $(,)? }, $tag:literal, $cause:literal) => {
        #[test]
        fn $name() {
            let refusal = crate::$inner::$variant { $($field: $value),* };
            let encoded = serde_json::to_string(&refusal).unwrap();
            assert_eq!(serde_json::from_str::<crate::$inner>(&encoded).unwrap(), refusal);
            migration_refusal_round_trip(
                super::KernelMigrationRefusal::$outer {
                    document: DocumentId::from_bytes([1; 32]),
                    refusal,
                },
                $tag,
                Some($cause),
            );
        }
    };
}

migration_refusal_law!(
    migration_document_missing_round_trips,
    DocumentMissing {
        document: DocumentId::from_bytes([1; 32])
    },
    "document_missing"
);
migration_refusal_law!(
    migration_not_admitted_round_trips,
    NotAdmitted {
        document: DocumentId::from_bytes([1; 32]),
        reason: "missing library".to_owned()
    },
    "not_admitted"
);
migration_refusal_law!(
    migration_state_round_trips,
    State {
        message: "invalid state".to_owned()
    },
    "state"
);
migration_refusal_law!(
    migration_document_version_round_trips,
    Document,
    DocumentRefusal,
    Version { found: 2, from: 1 },
    "document",
    "version"
);
migration_refusal_law!(
    migration_document_function_not_held_round_trips,
    Document,
    DocumentRefusal,
    FunctionNotHeld {
        function: lash_vocabulary::names::FunctionId::from_bytes([2; 32])
    },
    "document",
    "function_not_held"
);
migration_refusal_law!(
    migration_document_function_retired_round_trips,
    Document,
    DocumentRefusal,
    FunctionRetired {
        function: lash_vocabulary::names::FunctionId::from_bytes([2; 32]),
        name: lash_vocabulary::names::FunctionName::try_from("retired.function".to_owned())
            .unwrap(),
    },
    "document",
    "function_retired"
);
migration_refusal_law!(
    migration_document_form_not_carried_round_trips,
    Document,
    DocumentRefusal,
    FormNotCarried {
        site: Site::new(Unit::Main, vec![2, 1]),
        detail: "unsupported node".to_owned(),
    },
    "document",
    "form_not_carried"
);
migration_refusal_law!(
    migration_document_correspondence_round_trips,
    Document,
    DocumentRefusal,
    Correspondence {},
    "document",
    "correspondence"
);
migration_refusal_law!(
    migration_parked_version_round_trips,
    Parked,
    ParkedRefusal,
    Version { found: 2, from: 1 },
    "parked",
    "version"
);
migration_refusal_law!(
    migration_parked_document_round_trips,
    Parked,
    ParkedRefusal,
    Document {
        parked: DocumentId::from_bytes([1; 32]),
        rewritten: DocumentId::from_bytes([3; 32])
    },
    "parked",
    "document"
);
migration_refusal_law!(
    migration_parked_site_not_carried_round_trips,
    Parked,
    ParkedRefusal,
    SiteNotCarried {
        site: Site::new(Unit::Main, vec![2, 1]),
        held: "task".into(),
    },
    "parked",
    "site_not_carried"
);
migration_refusal_law!(
    migration_parked_function_not_carried_round_trips,
    Parked,
    ParkedRefusal,
    FunctionNotCarried {
        function: lash_vocabulary::names::FunctionId::from_bytes([2; 32])
    },
    "parked",
    "function_not_carried"
);
migration_refusal_law!(
    migration_parked_state_not_carried_round_trips,
    Parked,
    ParkedRefusal,
    StateNotCarried {
        detail: "heap pages cannot be carried".to_owned()
    },
    "parked",
    "state_not_carried"
);

/// FN-UPGRADE: a function value a host holds, written in the version
/// before the synthetic successor, upgrades as a stored workflow's document
/// does: its code is rewritten onto the successor's helpers and its
/// identity follows. A value whose code calls a helper the build does not
/// hold is refused, typed, rather than rebound to a helper of that name.
#[cfg(feature = "synthetic-next")]

/// A withdrawn operation wait never receives the later step's outcome.
#[test]
fn withdrawn_operation_waits_drop_late_outcomes() {
    let (state, run) = started();
    let (state, action) = settle(
        state,
        &run,
        parked(vec![effect(1, element(0)), effect(2, element(1))], vec![]),
    );
    assert_eq!(state.pages, Some(sealed().root));
    let EngineAction::Steps { steps, .. } = action else {
        panic!("operation steps");
    };
    let late = steps[0].step().clone();
    let live = steps[1].step().clone();
    let (state, action) = settle(state, &live, completed("null".into()));
    let (run, _, _) = run_of(&action);
    let (state, _) = settle(
        state,
        &run,
        parked(vec![], vec![lash_vocabulary::sites::testkit::wait_id(1)]),
    );
    assert_eq!(state.pages, Some(sealed().root));
    let (state, action) = settle(state, &late, completed("null".into()));
    assert!(!matches!(action, EngineAction::Steps { .. }));
    assert_eq!(state.pages, Some(sealed().root));
}
