//! The VM instance laws (FIG-4158, ADR 0123) over every corpus program.
//!
//! A worker runs model code in one [`VmInstance`] it resets between sessions
//! and drives through the owned step/resume interface. Three laws:
//!
//! * [`vm_reset_leaves_no_guest_observable_state`]: whatever one session
//!   planted, a reset leaves nothing a later session can observe.
//! * [`reset_equals_fresh_for_every_corpus_program`]: a reset instance runs
//!   every program exactly as a pristine one does.
//! * [`step_resume_matches_straight_through_for_every_corpus_program`]: a run
//!   driven step by step, and a process run parked at every boundary and
//!   reopened from its bytes on a fresh instance, match the same program run
//!   straight through.
//! * [`effect_park_matches_straight_through_for_every_corpus_program`]
//!   (FIG-4159, FIG-4275): a run of either mode parked on every operation it
//!   awaits (resource operations and their batches, process awaits, sleeps
//!   and process awaits), reopened from its bytes on a fresh instance and
//!   answered there with the outcome its host held, matches the same program
//!   run straight through:
//!   the same requests, the same cancel checkpoints, the same end and state.

use std::future::Future;
use std::num::NonZeroU64;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use lash_vm::testing::harness::EchoHost;
use lash_vm::{
    AbilityOp, AbilityOutcome, CompiledProgram, ExecutionBound, ExecutionBounds, ExecutionHost,
    ExecutionHostError, ExecutionMode, LashVmHostEnvironment, State, Value, VmExecutionStart,
    VmInstance, VmRequest, VmResume, VmRunConfig, VmStep,
};

use super::corpora::{self, CorpusProgram};

/// Every run here is bounded, so a corpus program that never ends fails
/// every path the same way instead of hanging the law.
const INSTRUCTION_BUDGET: NonZeroU64 = NonZeroU64::new(2_000_000).expect("nonzero");

fn bounds() -> ExecutionBounds {
    ExecutionBounds::new(
        ExecutionBound::Bounded(INSTRUCTION_BUDGET),
        ExecutionBound::Bounded(lash_vm::DEFAULT_HOST_MEMORY_LIMIT_BYTES),
    )
}

/// Polls a future the harness host answers immediately.
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a run over the harness host never waits"),
    }
}

/// The harness's answer to one effect.
fn answer(op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
    ready(EchoHost.perform(op))
}

/// Everything a run shows its host and leaves behind, in a form compared
/// byte for byte.
#[derive(Debug, PartialEq, Eq)]
struct Run {
    /// Every request the run made, in order.
    transcript: Vec<String>,
    /// How the run ended.
    end: String,
    /// The session state it left, canonically encoded (foreground runs).
    state: Option<String>,
}

fn encoded(state: &State) -> String {
    match state.snapshot().to_canonical_bytes() {
        Ok(bytes) => format!("{bytes:02x?}"),
        Err(error) => format!("unencodable: {error}"),
    }
}

/// The straight-through reference: the program run to its end by the
/// one-shot `execute`, against a host that records what it is asked.
struct StraightHost {
    mode: ExecutionMode,
    bounds: ExecutionBounds,
    transcript: Mutex<Vec<String>>,
}

impl ExecutionHost for StraightHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        self.record(format!("effect {op:?}"));
        answer(op)
    }

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        self.record(format!("checkpoint {checkpoint}"));
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.mode
    }

    fn execution_bounds(&self) -> ExecutionBounds {
        self.bounds
    }
}

impl StraightHost {
    fn record(&self, entry: String) {
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
    }
}

fn straight_through(program: &CompiledProgram, globals: &State, mode: ExecutionMode) -> Run {
    straight_through_within(program, globals, mode, bounds())
}

fn straight_through_within(
    program: &CompiledProgram,
    globals: &State,
    mode: ExecutionMode,
    bounds: ExecutionBounds,
) -> Run {
    let host = StraightHost {
        mode,
        bounds,
        transcript: Mutex::new(Vec::new()),
    };
    let mut state = globals.clone();
    let result = ready(lash_vm::execute(program, &mut state, &host));
    let end = match result {
        Ok(outcome) => format!("complete {outcome:?}"),
        Err(error) => format!("guest error {error:?}"),
    };
    Run {
        transcript: host
            .transcript
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        end,
        state: (mode == ExecutionMode::Foreground).then(|| encoded(&state)),
    }
}

/// How a stepped process run answers each boundary it is offered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundaries {
    RunOn,
    /// Park, carry the continuation's bytes to a pristine instance, and
    /// resume there.
    ParkAndReopen,
    /// Run on at every boundary, and park on every operation the run awaits
    /// that can be parked: the host settles the operation and holds its
    /// outcome, the continuation's bytes go to a pristine instance, and the
    /// operation the resumed run issues again is answered with the held
    /// outcome, recorded once.
    ParkEveryEffectAndReopen,
}

/// The program driven through the owned interface on `instance`.
fn stepped(
    instance: &mut VmInstance,
    program: &std::sync::Arc<CompiledProgram>,
    mode: ExecutionMode,
    boundaries: Boundaries,
) -> Run {
    stepped_within(instance, program, mode, boundaries, bounds())
}

/// A corpus program compiled, with its session globals bound. A program's
/// globals are bound to `null`: what matters is that every path sees the
/// same session.
struct Compiled {
    id: String,
    program: std::sync::Arc<CompiledProgram>,
    globals: State,
}

fn compile(program: &CorpusProgram) -> Compiled {
    let environment = program.environment();
    let linked = lash_typescript::link(&program.source, &environment)
        .unwrap_or_else(|error| panic!("{}: does not admit: {error}", program.id));
    let compiled = lash_vm::testing::harness::compile_linked_main(&linked);
    let mut globals = State::new();
    for name in &program.globals {
        globals
            .insert_global(name.clone(), Value::Null)
            .unwrap_or_else(|error| panic!("{}: bind `{name}`: {error}", program.id));
    }
    Compiled {
        id: program.id.clone(),
        program: std::sync::Arc::new(compiled),
        globals,
    }
}

fn corpus() -> Vec<Compiled> {
    let compiled = corpora::all().iter().map(compile).collect::<Vec<_>>();
    assert!(
        compiled.len() > 100,
        "the corpus must be the real corpus, found {} programs",
        compiled.len()
    );
    compiled
}

fn with_globals(instance: &mut VmInstance, globals: &State) {
    instance.replace_state(globals.clone());
}

#[test]
fn reset_equals_fresh_for_every_corpus_program() {
    let corpus = corpus();
    let pristine = format!("{:?}", VmInstance::pristine());
    // One instance runs the whole corpus, reset between programs: every
    // program after the first runs on an instance the previous one used.
    let mut reused = VmInstance::pristine();
    let mut failures = Vec::new();
    for program in &corpus {
        reused.reset();
        assert_eq!(
            format!("{reused:?}"),
            pristine,
            "{}: a reset instance is not pristine",
            program.id
        );
        with_globals(&mut reused, &program.globals);
        let after_reset = stepped(
            &mut reused,
            &program.program,
            ExecutionMode::Foreground,
            Boundaries::RunOn,
        );
        let mut fresh = VmInstance::pristine();
        with_globals(&mut fresh, &program.globals);
        let on_fresh = stepped(
            &mut fresh,
            &program.program,
            ExecutionMode::Foreground,
            Boundaries::RunOn,
        );
        if after_reset != on_fresh {
            failures.push(format!(
                "{}:\n  reset: {after_reset:?}\n  fresh: {on_fresh:?}",
                program.id
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} programs ran differently after a reset:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
}

#[test]
fn step_resume_matches_straight_through_for_every_corpus_program() {
    let corpus = corpus();
    let mut failures = Vec::new();
    let mut parked_runs = 0_usize;
    for program in &corpus {
        // Foreground: every effect and checkpoint answered through the owned
        // interface, against the one-shot run.
        let straight = straight_through(
            &program.program,
            &program.globals,
            ExecutionMode::Foreground,
        );
        let mut instance = VmInstance::pristine();
        with_globals(&mut instance, &program.globals);
        let stepped_run = stepped(
            &mut instance,
            &program.program,
            ExecutionMode::Foreground,
            Boundaries::RunOn,
        );
        if stepped_run != straight {
            failures.push(format!(
                "{} (foreground):\n  stepped:  {stepped_run:?}\n  straight: {straight:?}",
                program.id
            ));
        }

        // Process mode: parked at every boundary, each continuation carried
        // as bytes to a pristine instance, against the one-shot process run.
        let straight = straight_through(&program.program, &program.globals, ExecutionMode::Process);
        let mut instance = VmInstance::pristine();
        with_globals(&mut instance, &program.globals);
        let reopened = stepped(
            &mut instance,
            &program.program,
            ExecutionMode::Process,
            Boundaries::ParkAndReopen,
        );
        if reopened
            .transcript
            .iter()
            .any(|entry| entry.starts_with("effect"))
        {
            parked_runs += 1;
        }
        if reopened != straight {
            failures.push(format!(
                "{} (process, parked at every boundary):\n  reopened: {reopened:?}\n  straight: {straight:?}",
                program.id
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} runs of {} programs diverged from straight through:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
    assert!(
        parked_runs > 0,
        "no corpus program reached an effect, so nothing was parked"
    );
}

#[test]
fn effect_park_matches_straight_through_for_every_corpus_program() {
    let corpus = corpus();
    let mut failures = Vec::new();
    let mut parked_programs = 0_usize;
    let mut batch_parked_programs = 0_usize;
    for program in &corpus {
        for mode in [ExecutionMode::Foreground, ExecutionMode::Process] {
            let straight = straight_through(&program.program, &program.globals, mode);
            let mut instance = VmInstance::pristine();
            with_globals(&mut instance, &program.globals);
            let parked = stepped(
                &mut instance,
                &program.program,
                mode,
                Boundaries::ParkEveryEffectAndReopen,
            );
            if mode == ExecutionMode::Foreground
                && parked
                    .transcript
                    .iter()
                    .any(|entry| entry.starts_with("effect ResourceOperation("))
            {
                parked_programs += 1;
            }
            if mode == ExecutionMode::Foreground
                && parked
                    .transcript
                    .iter()
                    .any(|entry| entry.starts_with("effect ResourceOperationBatch("))
            {
                batch_parked_programs += 1;
            }
            if parked != straight {
                failures.push(format!(
                    "{} ({mode:?}, parked on every effect):\n  parked:   {parked:?}\n  straight: {straight:?}",
                    program.id
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} runs of {} programs diverged from straight through:\n{}",
        failures.len(),
        corpus.len(),
        failures.join("\n")
    );
    // The corpus's scalar resource calls: every program that issues one
    // parked on it.
    assert!(
        parked_programs >= 7,
        "too few corpus programs parked on a resource operation: {parked_programs}"
    );
    // The corpus's aggregate awaits: every program that issues a
    // resource-operation batch parked on it (FIG-4275).
    assert!(
        batch_parked_programs >= 4,
        "too few corpus programs parked on a resource-operation batch: {batch_parked_programs}"
    );
}

/// A run parked on an operation between long stretches of computation meets
/// every cancel checkpoint where an unparked run does, in both modes: the
/// park uncharges the operation's dispatch, and a whole-run loop resumes in
/// the yield phase it parked in.
#[test]
fn effect_park_keeps_cancel_checkpoints_where_an_unparked_run_meets_them() {
    let environment = probe_environment(&[]);
    let program = link(
        r#"
let total = 0;
for (let i = 0; i < 150000; i++) { total = total + i; }
const first = await tools.echo({ value: total });
for (let i = 0; i < 250000; i++) { total = total + i * 2; }
const second = await tools.echo({ value: total + 1 });
const third = await tools.echo({ value: second });
for (let i = 0; i < 400000; i++) { total = total - i; }
finish([first, second, third, total]);
"#,
        &environment,
    );
    let long = ExecutionBounds::new(
        ExecutionBound::Bounded(NonZeroU64::new(50_000_000).expect("nonzero")),
        ExecutionBound::Bounded(lash_vm::DEFAULT_HOST_MEMORY_LIMIT_BYTES),
    );
    for mode in [ExecutionMode::Foreground, ExecutionMode::Process] {
        let straight = straight_through_within(&program, &State::new(), mode, long);
        assert!(
            straight
                .transcript
                .iter()
                .filter(|entry| entry.starts_with("checkpoint"))
                .count()
                >= 3
                && straight.end.starts_with("complete"),
            "{mode:?}: the program crosses several cancel checkpoints: {:?}",
            straight.transcript
        );
        let mut instance = VmInstance::pristine();
        let parked = stepped_within(
            &mut instance,
            &program,
            mode,
            Boundaries::ParkEveryEffectAndReopen,
            long,
        );
        assert_eq!(
            parked, straight,
            "{mode:?}: a parked run matches straight through"
        );
    }
}

/// The text session A plants everywhere guest state can live.
const SENTINEL: &str = "SENTINEL-A-4158";

fn probe_environment(globals: &[&str]) -> LashVmHostEnvironment {
    lash_vm::testing::harness::test_environment().with_globals(globals.iter().copied())
}

fn link(source: &str, environment: &LashVmHostEnvironment) -> std::sync::Arc<CompiledProgram> {
    let linked = lash_typescript::link(source, environment)
        .unwrap_or_else(|error| panic!("`{source}` links: {error}"));
    std::sync::Arc::new(lash_vm::testing::harness::compile_linked_main(&linked))
}

/// Drives a run to its end, answering every effect from the harness host.
fn run_to_end(instance: &mut VmInstance, program: &std::sync::Arc<CompiledProgram>) -> Run {
    stepped(
        instance,
        program,
        ExecutionMode::Foreground,
        Boundaries::RunOn,
    )
}

#[test]
fn vm_reset_leaves_no_guest_observable_state() {
    let planted_names = [
        "planted",
        "plantedClosure",
        "plantedPattern",
        "plantedMatched",
        "plantedError",
        "plantedRecord",
        "plantedEcho",
    ];
    let environment = probe_environment(&[]);
    let mut instance = VmInstance::pristine();

    // A parked continuation: a process run parked mid-body, holding the
    // sentinel in its frame. A parked run hands its state to the
    // continuation, so it goes first and session A's cells run after it.
    let parked_source = format!(
        r#"
const held = await tools.echo({{ value: "{SENTINEL}-parked" }});
const again = await tools.echo({{ value: held }});
finish(again);
"#
    );
    let parked_program = link(&parked_source, &environment);
    let mut step = instance
        .start(
            parked_program.clone(),
            VmExecutionStart::Session,
            VmRunConfig::new(ExecutionMode::Process, bounds()),
        )
        .expect("start the process run");
    let parked = loop {
        step = match step {
            VmStep::Suspended(suspended) => {
                let resume = match suspended.request {
                    VmRequest::Effect(op) => VmResume::Effect(answer(op)),
                    VmRequest::CancelCheckpoint(_) => {
                        VmResume::CancelCheckpoint { cancelled: false }
                    }
                    VmRequest::Boundary => VmResume::Park,
                    VmRequest::ParkDeclined(_) => VmResume::Continue,
                };
                instance.resume(resume).expect("answer the process run")
            }
            VmStep::Parked(parked) => break parked,
            other => panic!("the process run must park at its first boundary: {other:?}"),
        };
    };
    let parked_bytes = parked.continuation.to_bytes().expect("encode the park");
    assert!(
        String::from_utf8_lossy(&parked_bytes).contains(SENTINEL),
        "the parked continuation must hold the sentinel"
    );

    // Session A: globals, a closure over them, a regex with advanced
    // `lastIndex`, an error object, a record, and an effect's result.
    let plant = format!(
        r#"
const planted = "{SENTINEL}";
const plantedClosure = () => planted + "-closure";
const plantedPattern = /SENTINEL-A-4158/g;
const plantedMatched = plantedPattern.test("xx {SENTINEL}");
const plantedError = new Error(planted);
const plantedRecord = {{ secret: planted, nested: [planted, plantedClosure()] }};
const plantedEcho = await tools.echo({{ value: planted }});
finish(plantedClosure());
"#
    );
    let planted = run_to_end(&mut instance, &link(&plant, &environment));
    assert!(
        planted.end.contains(SENTINEL),
        "session A must have planted its sentinel: {planted:?}"
    );
    assert!(
        encoded(instance.state()).len() > encoded(&State::new()).len(),
        "session A must have left state behind"
    );

    // The module cache: session A's cell, compiled and cached.
    let plant_environment = probe_environment(&[]);
    let program = lash_typescript::parse_cell(&plant, &plant_environment).expect("parse A");
    instance
        .linked_programs_mut()
        .get_or_compile_ast(&plant, program, &plant_environment)
        .expect("cache A's cell");

    // A pending handle: a run left in flight over session A's globals,
    // suspended on its effect, with the sentinel on its stack.
    let in_flight = format!(
        r#"
const pendingSecret = "{SENTINEL}-in-flight";
const pendingEcho = await tools.echo({{ value: pendingSecret }});
finish(pendingEcho);
"#
    );
    let step = instance
        .start(
            link(&in_flight, &environment),
            VmExecutionStart::Session,
            VmRunConfig::new(ExecutionMode::Foreground, bounds()),
        )
        .expect("start the in-flight run");
    let VmStep::Suspended(suspended) = step else {
        panic!("the in-flight run must suspend on its effect: {step:?}");
    };
    assert!(
        format!("{:?}", suspended.request).contains(SENTINEL),
        "the pending effect must carry the sentinel"
    );
    assert!(instance.is_running());

    instance.reset();

    // Session B probes every place A planted.
    assert!(!instance.is_running(), "a reset leaves no run in flight");
    assert!(
        matches!(
            instance.resume(VmResume::Continue),
            Err(lash_vm::VmStepError::NotRunning)
        ),
        "a reset instance has no pending request to answer"
    );
    assert!(
        instance
            .linked_programs_mut()
            .cached_linked_program(&plant, &plant_environment)
            .is_none(),
        "a reset instance keeps no compiled cell"
    );
    assert_eq!(
        format!("{instance:?}"),
        format!("{:?}", VmInstance::pristine()),
        "a reset instance is a pristine one"
    );
    assert_eq!(encoded(instance.state()), encoded(&State::new()));
    assert!(
        instance.state().binding_names().next().is_none(),
        "a reset instance binds nothing"
    );

    let probes = [
        // Globals, closures, the error and the record, by name: the names
        // link, and the values must not be there to read.
        "finish(typeof planted);".to_string(),
        "finish(typeof plantedClosure);".to_string(),
        "finish(typeof plantedError);".to_string(),
        "finish(typeof plantedRecord);".to_string(),
        "finish(typeof plantedEcho);".to_string(),
        "finish(typeof plantedPattern);".to_string(),
        // The regex state: a fresh pattern of the same source starts at 0.
        "const probe = /SENTINEL-A-4158/g;\nfinish([probe.lastIndex, probe.test(\"SENTINEL-A-4158\"), probe.lastIndex]);"
            .to_string(),
        // Error objects: a new one carries nothing of A's.
        "finish(String(new Error(\"probe\")));".to_string(),
        // The effect path: B's own effect answers only B.
        "finish(await tools.echo({ value: \"probe\" }));".to_string(),
        // The module cache: A's own source, recompiled and rerun under B.
        plant.clone(),
    ];
    let probe_environment = probe_environment(&planted_names);
    for probe in &probes {
        let program = link(probe, &probe_environment);
        let after_reset = run_to_end(&mut instance, &program);
        let mut fresh = VmInstance::pristine();
        let on_fresh = run_to_end(&mut fresh, &program);
        assert_eq!(
            format!("{after_reset:?}").into_bytes(),
            format!("{on_fresh:?}").into_bytes(),
            "`{probe}` observed state session A left behind"
        );
        if probe != &plant {
            assert!(
                !format!("{after_reset:?}").contains(SENTINEL),
                "`{probe}` read session A's sentinel: {after_reset:?}"
            );
        }
        instance.reset();
    }

    // The parked continuation belongs to its owner, not the instance: it
    // reopens on the reset instance only as what it is, and B's instance
    // held none of it.
    let reopened = instance
        .open_continuation(&parked_bytes)
        .expect("the parked bytes reopen");
    drop(reopened);
    assert_eq!(
        format!("{instance:?}"),
        format!("{:?}", VmInstance::pristine()),
        "opening a continuation installs nothing"
    );
}

fn stepped_within(
    instance: &mut VmInstance,
    program: &std::sync::Arc<CompiledProgram>,
    mode: ExecutionMode,
    boundaries: Boundaries,
    bounds: ExecutionBounds,
) -> Run {
    let config = VmRunConfig::new(mode, bounds);
    let mut transcript = Vec::new();
    let mut parks = 0_usize;
    let mut owner = None::<VmInstance>;
    // The operation a parked run awaits and the outcome its host holds.
    let mut held = None::<(String, Result<lash_vm::AbilityOutcome, ExecutionHostError>)>;
    let mut effect_parks = 0_usize;
    let mut declined_parks = Vec::new();
    let mut step = instance
        .start(program.clone(), VmExecutionStart::Session, config.clone())
        .unwrap_or_else(|error| panic!("the run starts: {error}"));
    let end = loop {
        let current = owner.as_mut().unwrap_or(&mut *instance);
        step = match step {
            VmStep::Suspended(suspended) => {
                let parkable = suspended.request.parkable();
                let resume = match suspended.request {
                    VmRequest::Effect(op) if held.is_some() => {
                        let (issued, outcome) = held.take().expect("checked above");
                        assert_eq!(
                            format!("{op:?}"),
                            issued,
                            "a resumed run issues the operation it parked on again"
                        );
                        VmResume::Effect(outcome)
                    }
                    VmRequest::Effect(op)
                        if parkable && boundaries == Boundaries::ParkEveryEffectAndReopen =>
                    {
                        let issued = format!("{op:?}");
                        transcript.push(format!("effect {issued}"));
                        held = Some((issued, answer(op)));
                        VmResume::Park
                    }
                    VmRequest::Effect(op) => {
                        transcript.push(format!("effect {op:?}"));
                        VmResume::Effect(answer(op))
                    }
                    VmRequest::CancelCheckpoint(checkpoint) => {
                        transcript.push(format!("checkpoint {checkpoint}"));
                        VmResume::CancelCheckpoint { cancelled: false }
                    }
                    VmRequest::Boundary => match boundaries {
                        Boundaries::RunOn | Boundaries::ParkEveryEffectAndReopen => {
                            VmResume::Continue
                        }
                        Boundaries::ParkAndReopen => VmResume::Park,
                    },
                    VmRequest::ParkDeclined(error) => {
                        if held.is_some() {
                            declined_parks.push(error.to_string());
                        }
                        VmResume::Continue
                    }
                };
                current
                    .resume(resume)
                    .unwrap_or_else(|error| panic!("the resume answers its request: {error}"))
            }
            VmStep::Parked(parked) => {
                parks += 1;
                if parked.reason == lash_vm::VmParkReason::AwaitingEffect {
                    effect_parks += 1;
                    assert!(held.is_some(), "a run parks on an effect only when asked");
                }
                let bytes = parked
                    .continuation
                    .to_bytes()
                    .expect("a parked continuation encodes");
                let mut successor = VmInstance::pristine();
                let continuation = successor
                    .open_continuation(&bytes)
                    .expect("a parked continuation reopens on a fresh instance");
                let next = successor
                    .start(
                        program.clone(),
                        VmExecutionStart::Continuation(Box::new(continuation)),
                        config.clone(),
                    )
                    .unwrap_or_else(|error| panic!("the continuation resumes: {error}"));
                owner = Some(successor);
                next
            }
            VmStep::Complete(complete) => break format!("complete {:?}", complete.outcome),
            VmStep::GuestError(error) => break format!("guest error {:?}", error.failure.error),
        };
    };
    assert!(
        held.is_none(),
        "every operation a run parked on was issued again"
    );
    if boundaries == Boundaries::ParkEveryEffectAndReopen {
        let parkable = transcript
            .iter()
            .filter(|entry| {
                entry.starts_with("effect ResourceOperation(")
                    || entry.starts_with("effect ResourceOperationBatch(")
                    || entry.starts_with("effect Await(")
                    || entry.starts_with("effect Sleep")
            })
            .count();
        assert_eq!(
            effect_parks + declined_parks.len(),
            parkable,
            "the run parked on every parkable operation it issued, or declined it: {declined_parks:?}; {mode:?} transcript {transcript:?}, end {end}"
        );
        for reason in &declined_parks {
            eprintln!("declined park: {reason}");
        }
    }
    if boundaries == Boundaries::ParkAndReopen {
        let effects = transcript
            .iter()
            .filter(|entry| entry.starts_with("effect"))
            .count();
        assert!(parks <= effects, "a run parks at most once per effect");
    }
    let finished = owner.as_ref().unwrap_or(&*instance);
    assert!(
        !finished.is_running(),
        "an ended run leaves nothing in flight"
    );
    Run {
        transcript,
        end,
        state: (mode == ExecutionMode::Foreground).then(|| encoded(finished.state())),
    }
}
