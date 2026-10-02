//! A long process loop streams its execution observations (FIG-4458).
//!
//! A process body reports every node it runs, and a loop reports each of its
//! iterations. The worker hands a step's observations to its parent in
//! chunks that each fit one frame, so a body looping many thousands of times
//! within its fuel, heap and depth budgets completes. A loop of a few
//! thousand iterations once outgrew one frame's decode bounds: the broker
//! refused the payload as a retryable protocol violation, and every retry met
//! the same bound, so the body never finished.

use super::recovery::{segment_journals_end_where_their_bodies_ran, start, worker};
use super::worker_verdicts::{process_host, workers_with_cpu};
use super::*;

/// Iterations of the body's loop. Each reports about two observations, and
/// one frame holds about 3,600 of them, so a single payload of this loop's
/// observations outgrew the decode bounds about three times over.
const ITERATIONS: f64 = 5_000.0;

/// How long the body may take. The parent publishes every observation to its
/// live process graph and folds them in batches (FIG-4499): a fold apiece
/// cost tens of seconds an execution, on every replay of the body.
const LONG_LOOP_BOUND: Duration = Duration::from_secs(300);

/// ```text
/// process main() {
///   i = 0
///   while (i < ITERATIONS) { i = i + 1 }
///   first = tools.count_call({})
///   finish first
/// }
/// ```
async fn long_loop_request(engine: &Engine) -> lash_core::ProcessStartRequest {
    use lashlang::CoercingBinaryOp::{Add, Less};
    let program = b::module(
        vec![b::process_with_signals(
            PROCESS,
            Vec::new(),
            Vec::new(),
            b::block(vec![
                b::assign("i", b::num(0.0)),
                b::while_loop(
                    b::binary(b::var("i"), Less, b::num(ITERATIONS)),
                    b::block(vec![b::assign(
                        "i",
                        b::binary(b::var("i"), Add, b::num(1.0)),
                    )]),
                ),
                b::assign(
                    "first",
                    b::module_call(&["tools"], TOOL, vec![b::record(Vec::new())]),
                ),
                b::finish(b::var("first")),
            ]),
        )],
        Vec::new(),
    );
    let input = publish_program(engine, program, lashlang::LashlangAbilities::default()).await;
    lash_core::ProcessStartRequest::new(
        input.into_process_input().expect("the process input"),
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_ref(
        lash_core::publish_process_execution_env(
            engine.lash_backend().process_env_store().as_ref(),
            &lash_core::testing::host_pin_claim_for_testing(),
            &process_env_spec(),
        )
        .await
        .expect("publish the captured environment"),
    )
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types())
}

/// The body loops `ITERATIONS` times inside its budgets, then calls the tool
/// once and finishes with its result.
async fn a_long_loop_completes(engine: Engine) {
    let executions = Arc::new(AtomicUsize::new(0));
    let host = process_host(
        engine.lash_backend(),
        workers_with_cpu(lash::rlm::WorkerDeadlines::standard().cumulative_cpu),
        &executions,
    );
    engine.install_process_worker(worker(&host));
    let request = long_loop_request(&engine).await;
    let id = start(&engine, &host, request).await;
    let output = tokio::time::timeout(LONG_LOOP_BOUND, host.processes().await_output(&id))
        .await
        .expect("the long loop reached its terminal")
        .expect("the terminal resolved");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without output: {output:?}")
    };
    let lash_core::ToolCallOutcome::Success(value) = &output.outcome else {
        panic!("the process failed: {output:?}")
    };
    assert_eq!(
        value.to_json_value(),
        json!({"result": "counted"}),
        "the terminal is the body's result"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the tool call after the loop ran once"
    );
    let runs = engine.completed_process_invocations(&id).await;
    segment_journals_end_where_their_bodies_ran(&engine, &runs).await;
    engine.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_body_looping_five_thousand_times_completes() {
    a_long_loop_completes(Engine::double(0x4458, None).await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the `crash-windows` Restate suite runs it"]
async fn live_restate_a_process_body_looping_five_thousand_times_completes() {
    a_long_loop_completes(Engine::live("observation-stream", None).await).await;
}
