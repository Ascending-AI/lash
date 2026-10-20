use lash_core::tool_run::{MaterialOwner, MaterialRole};
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::EngineEvent;
use lash_execution_contracts::process::EngineState;
use lash_execution_contracts::process::StepName;
use lash_execution_contracts::process::StepRequest;
use lash_kernel_doc::{DocumentId, EffectId, Site, Unit};
use lash_vm_client::wire::OutcomeWire;

use super::advance::{advance, decode, state_format};
use super::state::{
    Admission, Issued, KERNEL_RUN_STEP, KernelRunInput, KernelRunOutput, ParkedPage, ParkedRun,
    Phase, refused,
};
use lash_kernel_doc::KernelVersion;
use lash_kernel_state::{AdmittedRequest, AdmittedWait};
use std::collections::BTreeMap;

fn process() -> lash_core::ProcessId {
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
#[test]
fn every_engine_state_holding_a_run_names_its_page_to_the_process_root() {
    let mut written = Vec::new();
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(
            vec![
                effect(1, element(0)),
                Issued::Sleep {
                    wait: lash_kernel_doc::testkit::wait_id(2),
                    admission: admitted(element(1)),
                    until_ms: 5_000,
                },
            ],
            vec![],
        ),
    );
    written.push(state.clone());
    let EngineAction::Steps { steps, .. } = &action else {
        panic!("a park on an effect asks for its step, found {action:?}");
    };
    let effect_step = steps[0].step().clone();
    let (state, action) = settle(state, &effect_step, completed("\"a\"".to_owned()));
    let (run1, input, _) = run_of(&action);
    assert_eq!(
        input.parked,
        Some(ParkedRun {
            page: sealed(),
            by: 0
        }),
        "the run resumes from the page the run that parked it published"
    );
    written.push(state.clone());
    let (state, action) = settle(state, &run1, parked(vec![], vec![]));
    assert!(matches!(action, EngineAction::Sleep { .. }));
    written.push(state.clone());
    let (state, action) =
        advance(KernelVersion::NEWEST, state, EngineEvent::Woke).expect("a wake advances");
    let (run2, _, _) = run_of(&action);
    written.push(state.clone());
    let outcome = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!("done")),
    );
    let (ended, _) = settle(
        state,
        &run2,
        completed(
            serde_json::to_string(&KernelRunOutput::Ended {
                outcome: Box::new(outcome),
                ran: Default::default(),
                caught: Vec::new(),
                reads: None,
                start: None,
            })
            .expect("a run output serializes"),
        ),
    );
    assert_eq!(ended.pages, None, "an ended run stands on no page");
    for state in written {
        assert_eq!(state.pages, Some(sealed().root), "the state names its page");
        let (decoded, _) = decode(KernelVersion::NEWEST, &state)
            .unwrap_or_else(|error| panic!("a written state decodes: {error:?}"));
        assert_eq!(decoded.parked.map(|parked| parked.page), Some(sealed()));
        let mut other = state.clone();
        other.pages = Some(lash_core_store::checkpoint_graph::NodeId([8; 32]));
        decode(KernelVersion::NEWEST, &other)
            .expect_err("a state naming another page than its root holds is refused");
    }
}

/// The admission of a `perform` of `fetch` at `identity`.
fn admitted(identity: EffectId) -> Admission {
    Admission(AdmittedWait {
        identity,
        request: AdmittedRequest::Effect {
            effect: lash_kernel_doc::EffectName::new("fetch").expect("a name"),
            args: Vec::new(),
        },
    })
}

fn parked(issued: Vec<Issued>, withdrawn: Vec<lash_protocol_types::WaitId>) -> SettledOutput {
    completed(
        serde_json::to_string(&KernelRunOutput::Parked {
            state: sealed(),
            issued,
            withdrawn,
            ran: Default::default(),
            caught: Vec::new(),
            reads: None,
            start: None,
        })
        .expect("a run output serializes"),
    )
}

/// The effect a fan-out element performs: the one site, run by the task
/// the `element`-th execution of the spawn started.
fn element(element: u64) -> EffectId {
    lash_kernel_doc::testkit::effect(
        lash_kernel_doc::testkit::spawned_task(
            lash_kernel_doc::testkit::root_task(),
            Site::new(Unit::Main, vec![0]),
            element,
        ),
        Site::new(
            Unit::Function(lash_kernel_doc::Name::new("worker")),
            vec![0],
        ),
        0,
        Vec::new(),
    )
}

fn effect(wait: u64, identity: EffectId) -> Issued {
    Issued::Effect {
        wait: lash_kernel_doc::testkit::wait_id(wait),
        admission: admitted(identity),
        effect: lash_kernel_doc::EffectName::new("tools.fetch").expect("an effect name"),
        contract: lash_kernel_doc::ContractId::new("1"),
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
        vec![lash_kernel_doc::testkit::wait_id(11)]
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
        vec![lash_kernel_doc::testkit::wait_id(10)]
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
#[test]
fn a_refused_effect_is_raised_without_a_step_and_a_withdrawn_wait_is_never_delivered() {
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(
            vec![
                effect(1, element(0)),
                Issued::Refused {
                    wait: lash_kernel_doc::testkit::wait_id(2),
                    admission: admitted(element(1)),
                    kind: super::EFFECT_UNKNOWN.to_owned(),
                    message: "no such effect".to_owned(),
                },
                Issued::Sleep {
                    wait: lash_kernel_doc::testkit::wait_id(3),
                    admission: admitted(element(2)),
                    until_ms: 5_000,
                },
            ],
            vec![],
        ),
    );
    let (run1, input, effects) = run_of(&action);
    assert_eq!(effects.len(), 1);
    let withdrawn_step = effects[0].step().clone();
    assert_eq!(
        input
            .deliver
            .iter()
            .map(|d| (d.wait, d.outcome.clone()))
            .collect::<Vec<_>>(),
        vec![(
            lash_kernel_doc::testkit::wait_id(2),
            refused(super::EFFECT_UNKNOWN, "no such effect".to_owned())
        )]
    );
    let EngineAction::Steps { wake, .. } = &action else {
        unreachable!()
    };
    assert_eq!(
        *wake,
        Some(lash_execution_contracts::process::EngineWake {
            until: lash_core::durable_port::DurableInstant(5_000),
            site: Some(element(2))
        })
    );

    // The machine caught the refusal and withdrew its wait on the effect.
    let (state, action) = settle(
        state,
        &run1,
        parked(vec![], vec![lash_kernel_doc::testkit::wait_id(1)]),
    );
    assert_eq!(
        action,
        EngineAction::Sleep {
            until: lash_core::durable_port::DurableInstant(5_000),
            site: Some(element(2)),
        }
    );
    let (state, action) = settle(state, &withdrawn_step, completed("null".to_owned()));
    assert!(matches!(action, EngineAction::Sleep { .. }));

    let (_, action) =
        advance(KernelVersion::NEWEST, state, EngineEvent::Woke).expect("a wake advances");
    let (_, input, _) = run_of(&action);
    assert_eq!(
        input
            .deliver
            .iter()
            .map(|d| (d.wait, d.outcome.clone()))
            .collect::<Vec<_>>(),
        vec![(lash_kernel_doc::testkit::wait_id(3), OutcomeWire::Elapsed)]
    );
}

/// Workflow admission's local names preserve the wrapper and every checker
/// error; hosts never need to parse a payload or display message for the code.
#[test]
fn workflow_admission_refusals_have_stable_local_codes() {
    use super::workflow::WorkflowAdmissionRefusal;
    let document = lash_kernel_doc::parse_document(
        "kernel 1\nnumbers float\neffect fetch(url: Text) -> Any contract \"1\"\neffect store(key: Text) -> Any contract \"1\"\n\nmain {\n  let a = perform fetch(\"u\") as Any\n  let b = perform store(\"k\") as Any\n}\n",
    ).expect("document");
    let functions = BTreeMap::new();
    let refusal =
        lash_kernel_check::admit(&document, &lash_kernel_check::Environment::new(&functions))
            .expect_err("both effects are missing");
    let document_refusal = WorkflowAdmissionRefusal::Document(refusal);
    assert_eq!(document_refusal.code(), "document");
    let WorkflowAdmissionRefusal::Document(refusal) = document_refusal else {
        panic!("the wrapper retains the checker refusal");
    };
    assert_eq!(refusal.errors.len(), 2);
    assert_eq!(
        refusal
            .errors
            .iter()
            .map(|error| error.reason.code())
            .collect::<Vec<_>>(),
        ["missing_effect", "missing_effect"]
    );
    assert_eq!(
        WorkflowAdmissionRefusal::NoSuchEntry {
            entry: lash_kernel_doc::Name::new("missing"),
        }
        .code(),
        "no_such_entry"
    );
    assert_eq!(
        WorkflowAdmissionRefusal::Boundary {
            message: "catalog cannot be stated as effects".into()
        }
        .code(),
        "boundary"
    );
}

/// P2: admitting an entry does not require the effects of main or siblings.
#[test]
fn workflow_admission_checks_only_the_selected_entry() {
    let document = lash_kernel_doc::parse_document(
        r#"kernel 1
numbers float
effect a.run(input: Any) -> Any contract "1"
effect b.run(input: Any) -> Any contract "1"
entry worker() -> Any
fn worker() { let got = perform b.run(null) as Any return got }
main { let got = perform a.run(null) as Any finish got }
"#,
    )
    .unwrap();
    let functions = BTreeMap::new();
    let mut environment = lash_kernel_check::Environment::new(&functions);
    environment.effects.insert(
        lash_kernel_doc::EffectName::new("b.run").unwrap(),
        document.manifest.effects[&lash_kernel_doc::EffectName::new("b.run").unwrap()].clone(),
    );
    super::WorkflowDocument::admit(
        document,
        lash_trace::WorkflowDocumentEntry::Entry {
            function: lash_kernel_doc::Name::new("worker"),
        },
        &environment,
    )
    .expect("only worker's performed effect must be offered");
}

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
    .observing(lash_kernel_vm::ObservationMode::Counts);
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
        refusal: refusal.clone(),
        message: refusal.to_string(),
        fatal: true,
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
        refusal: refusal.clone(),
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
    assert_eq!(found, refusal);
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
        function: lash_kernel_doc::FunctionId::from_bytes([2; 32])
    },
    "document",
    "function_not_held"
);
migration_refusal_law!(
    migration_document_function_retired_round_trips,
    Document,
    DocumentRefusal,
    FunctionRetired {
        function: lash_kernel_doc::FunctionId::from_bytes([2; 32]),
        name: lash_kernel_doc::FunctionName::try_from("retired.function".to_owned()).unwrap(),
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
        function: lash_kernel_doc::FunctionId::from_bytes([2; 32])
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
#[test]
fn a_held_function_value_upgrades_as_its_document_does_and_an_unheld_helper_refuses() {
    use lash_kernel_doc::{
        Document, Expr, Function, FunctionId, FunctionValue, Name, NumberPolicy, Param, Rhs,
        Signature, Stmt, Type,
    };
    let functions = lash_vm_releases::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .expect("the build's library");
    let (abs, name) = functions
        .iter()
        .find(|(_, registered)| {
            registered.definition().kernel == KernelVersion::One.number()
                && registered.definition().name.as_str() == "num.abs"
        })
        .map(|(id, registered)| (*id, registered.definition().name.clone()))
        .expect("the earlier version's num.abs");
    let value = |function: FunctionId| {
        let run = Name::new(lash_kernel_doc::FUNCTION_VALUE_ENTRY);
        let mut document = Document::new(NumberPolicy::Float, Vec::new());
        document.manifest.kernel = KernelVersion::One.number();
        document.manifest.functions.insert(function, name.clone());
        document.functions.insert(
            run.clone(),
            Function {
                params: vec![Name::new("x")],
                body: vec![
                    Stmt::Let {
                        name: Name::new("y"),
                        value: Rhs::Expr(Expr::Call {
                            function,
                            args: vec![Expr::Variable(Name::new("x"))],
                        }),
                    },
                    Stmt::Return {
                        value: Expr::Variable(Name::new("y")),
                    },
                ],
                start: None,
            },
        );
        document.entries.insert(
            run.clone(),
            lash_kernel_doc::Entry {
                signature: Signature {
                    params: vec![Param {
                        name: Name::new("x"),
                        ty: Type::Any,
                        optional: false,
                    }],
                    result: Type::Any,
                },
                convention: lash_kernel_doc::StartConvention::Direct,
            },
        );
        FunctionValue {
            captions: lash_kernel_doc::Captions::empty(document.identity()),
            document,
            entry: run,
        }
    };

    let held = value(abs);
    let upgraded = super::migrate_function_value(&held, &functions, KernelVersion::NEWEST)
        .expect("the value upgrades")
        .expect("an earlier version's value is carried");
    let planned = super::plan_migration(&held.document, &functions)
        .expect("the document upgrades")
        .expect("an earlier version's document is carried");
    assert_eq!(upgraded.document, planned.rewritten.document);
    assert_eq!(upgraded.entry, held.entry);
    assert_eq!(upgraded.signature(), held.signature());
    assert_ne!(
        upgraded.document.identity(),
        held.document.identity(),
        "the carried value is another document"
    );
    assert!(
        super::migrate_function_value(&upgraded, &functions, KernelVersion::NEWEST)
            .expect("a current value checks")
            .is_none(),
        "a value in the newest version is kept as it is"
    );

    // No helper hashes to this identity: it names one the build never held.
    let unheld = FunctionId::from_bytes([7; 32]);
    let refusal = super::migrate_function_value(&value(unheld), &functions, KernelVersion::NEWEST)
        .expect_err("an unheld helper refuses");
    assert!(
        matches!(
            refusal,
            super::KernelMigrationRefusal::Document {
                refusal: lash_kernel_migrate::DocumentRefusal::FunctionNotHeld { function },
                ..
            } if function == unheld
        ),
        "{refusal:?}"
    );
}

/// Two releases of package `p` in one registry, `p.outer` calling `p.fix`
/// in each, with R2's `p.fix` corrected, and a workflow over each.
fn releases() -> (
    lash_kernel_doc::FunctionRegistry,
    lash_kernel_migrate::Relink,
    [lash_kernel_doc::Document; 2],
) {
    let release = |fixed: bool| {
        let body = if fixed {
            "let y = [x]\n  return y"
        } else {
            "return x"
        };
        let fix = lash_kernel_doc::parse_definition(&format!(
            "function p.fix(x: Any) -> Any\nkernel 1\ncharge 1\nbody {{\n  {body}\n}}\n"
        ))
        .unwrap();
        let outer = lash_kernel_doc::parse_definition(&format!(
            "function p.outer(x: Any) -> Any\nkernel 1\ncharge 1\nuse p.fix = @{}\n\
             body {{\n  let y = invoke p.fix(x)\n  return y\n}}\n",
            fix.identity()
        ))
        .unwrap();
        [fix, outer]
    };
    let workflow = |[fix, outer]: &[lash_kernel_doc::FunctionDefinition; 2]| {
        lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers float\nuse p.fix = @{}\nuse p.outer = @{}\n\
             entry run(x: Any) -> Any\n\
             fn run(x) {{\nlet got = invoke p.outer(x)\nreturn got\n}}\nmain {{}}\n",
            fix.identity(),
            outer.identity()
        ))
        .unwrap()
    };
    let (r1, r2) = (release(false), release(true));
    let mut registry = lash_kernel_doc::FunctionRegistry::default();
    for definition in r1.iter().chain(&r2) {
        registry.register(definition.clone(), None).unwrap();
    }
    let relink = lash_kernel_migrate::relink(&r1, &r2);
    (registry, relink, [workflow(&r1), workflow(&r2)])
}

/// K-LIB-009 through the runtime: a published definition's document, or
/// a process that is not parked, written over release R1 of `p` is adopted
/// onto R2 as exactly the workflow written over R2, admitted on the
/// library that holds both; one that reaches nothing R1 holds is left as it
/// is.
#[test]
fn a_definition_or_unparked_process_is_adopted_onto_the_successor_release() {
    let (registry, relink, [over_r1, over_r2]) = releases();
    let adopted = super::plan_adoption(&over_r1, false, &registry, &relink)
        .expect("the workflow adopts")
        .expect("it reaches R1");
    assert_eq!(adopted.from, over_r1.identity());
    assert_eq!(adopted.rewritten.document, over_r2);
    assert!(
        super::plan_adoption(&over_r2, false, &registry, &relink)
            .expect("an R2 workflow plans")
            .is_none(),
        "a workflow already over R2 is not adopted"
    );
}

/// An adoption request for a parked run refuses with the typed
/// `StateNotCarried` (FIG-6233): no migrator carries its heap pages, so it
/// stays parked on R1, which its document still pins.
#[test]
fn a_parked_run_is_not_adopted_and_stays_on_the_release_it_pins() {
    let (registry, relink, [over_r1, _]) = releases();
    let refusal = super::plan_adoption(&over_r1, true, &registry, &relink)
        .expect_err("a parked run is not adopted");
    let super::HelperAdoptionRefusal::Parked {
        document,
        refusal: lash_kernel_migrate::ParkedRefusal::StateNotCarried { detail },
    } = &refusal
    else {
        panic!("{refusal:?}")
    };
    assert_eq!(*document, over_r1.identity());
    assert!(detail.contains("FIG-6233"), "{detail}");
    assert!(
        relink
            .keys()
            .all(|function| over_r1.manifest.functions.contains_key(function)),
        "it still pins every R1 function"
    );
}
