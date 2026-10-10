use lash_core::tool_run::{MaterialOwner, MaterialRole};
use lash_core::{
    EngineAction, EngineEvent, EngineState, Material, SettledOutput, StepName, StepRequest,
};
use lash_kernel_doc::{DocumentId, EffectIdentity, Site, TaskIdentity, Unit};
use lash_vm_client::wire::OutcomeWire;
use lash_vm_client::{OpaqueVmState, VmOwner};

use super::advance::{advance, decode, state_format};
use super::state::{Issued, KERNEL_RUN_STEP, KernelRunInput, KernelRunOutput, Phase, refused};
use lash_kernel_doc::KernelVersion;
use lash_kernel_state::{ParkedRun, Run};
use std::collections::{BTreeMap, BTreeSet};

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

/// A parked run sealed under this build's kernel version, whose bytes state
/// kernel version `stated`: what a worker exports when `stated` is the
/// seal's version.
fn sealed(stated: u32) -> OpaqueVmState {
    let document = DocumentId::from_bytes([7; 32]);
    let run = ParkedRun {
        run: Run {
            kernel: stated,
            document,
            functions: BTreeSet::new(),
            charged: 0,
            objects_allocated: 0,
            waits_issued: 0,
            ready: Vec::new(),
            withdrawn: BTreeSet::new(),
            unreported: Vec::new(),
        },
        session: BTreeMap::new(),
        tasks: Vec::new(),
        objects: BTreeMap::new(),
    };
    OpaqueVmState::seal(
        VmOwner::new("process:law"),
        crate::LASH_KERNEL_VERSION,
        document.to_string(),
        run.to_json().expect("a parked run serializes"),
    )
}

fn parked(issued: Vec<Issued>, withdrawn: Vec<u64>) -> SettledOutput {
    completed(
        serde_json::to_string(&KernelRunOutput::Parked {
            state: sealed(crate::LASH_KERNEL_VERSION),
            issued,
            withdrawn,
        })
        .expect("a run output serializes"),
    )
}

/// The effect a fan-out element performs: the one site, run by the task
/// the `element`-th execution of the spawn started.
fn element(element: u64) -> EffectIdentity {
    EffectIdentity {
        task: TaskIdentity::Spawned(lash_kernel_doc::SpawnIdentity {
            parent: std::sync::Arc::new(TaskIdentity::Main),
            site: Site::new(Unit::Main, vec![0]),
            occurrence: element,
        }),
        site: Site::new(
            Unit::Function(lash_kernel_doc::Name::new("worker")),
            vec![0],
        ),
        occurrence: 0,
        loops: Vec::new(),
    }
}

fn effect(wait: u64, identity: EffectIdentity) -> Issued {
    Issued::Effect {
        wait,
        identity,
        tool: lash_sansio::ToolId::new("tool:fetch"),
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

/// Target 3 and `K-TASK-012`: a body that fans out parks on every element's
/// effect at once, each its own step at the identity the machine computed
/// (the task component tells the elements apart), and each outcome is
/// handed to the machine as it settles, in whatever order: one that settles
/// while a run is in flight is handed to the next run, and none is handed
/// twice.
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
    assert_ne!(sites[0].task, sites[1].task);
    let (first, second) = (steps[0].step().clone(), steps[1].step().clone());

    // The second element settles first: the machine runs with it alone.
    let (state, action) = settle(state, &second, completed("\"b\"".to_owned()));
    let (run1, input, effects) = run_of(&action);
    assert!(effects.is_empty());
    assert_eq!(
        input.deliver.iter().map(|d| d.wait).collect::<Vec<_>>(),
        vec![11]
    );
    assert!(input.parked.is_some());

    // The first settles while that run is in flight: nothing new is asked.
    let (state, action) = settle(state, &first, completed("\"a\"".to_owned()));
    assert_eq!(action, EngineAction::Idle);

    // The run parks having asked for nothing: the queued outcome runs it
    // again, and it is the only one delivered.
    let (state, action) = settle(state, &run1, parked(vec![], vec![]));
    let (run2, input, _) = run_of(&action);
    assert_eq!(
        input.deliver.iter().map(|d| d.wait).collect::<Vec<_>>(),
        vec![10]
    );
    assert_eq!(
        input.deliver[0].outcome,
        OutcomeWire::Completed(lash_kernel_doc::Datum::Text("a".to_owned()))
    );

    let outcome = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!(["a", "b"])),
    );
    let (state, action) = settle(
        state,
        &run2,
        completed(
            serde_json::to_string(&KernelRunOutput::Ended {
                outcome: Box::new(outcome.clone()),
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
                    wait: 2,
                    identity: element(1),
                    kind: super::EFFECT_UNKNOWN.to_owned(),
                    message: "no such effect".to_owned(),
                },
                Issued::Sleep {
                    wait: 3,
                    identity: element(2),
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
            2,
            refused(super::EFFECT_UNKNOWN, "no such effect".to_owned())
        )]
    );
    let EngineAction::Steps { wake, .. } = &action else {
        unreachable!()
    };
    assert_eq!(*wake, Some(lash_core::durable_port::DurableInstant(5_000)));

    // The machine caught the refusal and withdrew its wait on the effect.
    let (state, action) = settle(state, &run1, parked(vec![], vec![1]));
    assert_eq!(
        action,
        EngineAction::Sleep {
            until: lash_core::durable_port::DurableInstant(5_000),
            site: None,
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
        vec![(3, OutcomeWire::Elapsed)]
    );
}

/// KMIGRATE's stale-seal check at every engine state that holds a run
/// (FIG-5793): a process's decode reads the kernel version the parked
/// run's bytes state in each state the transitions write with a run in it
/// (parked on a step and a timer, a run in flight, asleep on the timer,
/// ended) and accepts it under its seal, and refuses each one whose bytes
/// state another version than the seal.
#[test]
fn every_engine_state_holding_a_run_is_read_against_its_seal() {
    let mut written = Vec::new();
    let (state, run0) = started();
    let (state, action) = settle(
        state,
        &run0,
        parked(
            vec![
                effect(1, element(0)),
                Issued::Sleep {
                    wait: 2,
                    identity: element(1),
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
    let (run1, _, _) = run_of(&action);
    written.push(state.clone());
    let (state, action) = settle(state, &run1, parked(vec![], vec![]));
    assert!(matches!(action, EngineAction::Sleep { .. }));
    written.push(state.clone());
    let (state, action) =
        advance(KernelVersion::NEWEST, state, EngineEvent::Woke).expect("a wake advances");
    let (run2, _, _) = run_of(&action);
    written.push(state.clone());
    let outcome = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(serde_json::json!("done")),
    );
    let (state, _) = settle(
        state,
        &run2,
        completed(
            serde_json::to_string(&KernelRunOutput::Ended {
                outcome: Box::new(outcome),
            })
            .expect("a run output serializes"),
        ),
    );
    written.push(state);

    let phases = |phase: &Phase| match phase {
        Phase::Running { .. } => "running",
        Phase::Parked => "parked",
        Phase::Ended => "ended",
    };
    let mut seen = Vec::new();
    for state in written {
        let (mut decoded, _) = decode(KernelVersion::NEWEST, &state)
            .unwrap_or_else(|error| panic!("a written state decodes: {error:?}"));
        assert!(decoded.parked.is_some(), "the state holds its run");
        seen.push(phases(&decoded.phase));

        decoded.parked = Some(sealed(crate::LASH_KERNEL_VERSION + 1));
        let stale =
            super::advance::encode(&decoded, KernelVersion::NEWEST).expect("a state encodes");
        let refused = decode(KernelVersion::NEWEST, &stale)
            .expect_err("a run whose bytes state another version is refused");
        assert!(
            format!("{refused:?}").contains(&format!(
                "sealed under kernel version {}, and its bytes state kernel version {}",
                crate::LASH_KERNEL_VERSION,
                crate::LASH_KERNEL_VERSION + 1
            )),
            "{refused:?}"
        );
    }
    assert_eq!(
        seen,
        vec!["parked", "running", "parked", "running", "ended"],
        "every kind of state that holds a run is read"
    );
}
