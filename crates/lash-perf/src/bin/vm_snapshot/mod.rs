//! S1: the production continuation codec at quiet awaits, without a store or IPC.
//!
//! Setup executes guest code once. Each sample parks the pending sleep, encodes
//! the captured VM, decodes it on a pristine instance, and restores it to that
//! same pending sleep. No earlier instruction or host operation is re-executed.

use std::{collections::BTreeSet, path::Path, sync::Arc, time::Instant};

use anyhow::{Context, Result, bail, ensure};
use lash_protocol_rlm::{RlmHistoryProjection, rlm_history_projection};
use lash_vm::{
    AbilityOp, AbilityOutcome, ExecutionBounds, ExecutionMode, ExecutionOutcome, ProjectedBindings,
    ProjectedReadRequest, ProjectedReadResponse, ProjectedValue, ProjectionReadError,
    ProjectionReader, ProjectionType, ResourceRef, Value, VmExecutionStart, VmInstance, VmRequest,
    VmResume, VmRunConfig, VmStep,
};
use serde::Serialize;

#[derive(Serialize)]
struct Timing {
    p50_ms: f64,
    p99_ms: f64,
    samples_ns: Vec<u64>,
}

impl Timing {
    fn new(samples_ns: Vec<u64>) -> Self {
        let mut sorted = samples_ns.clone();
        sorted.sort_unstable();
        let percentile = |percent: usize| {
            sorted[(sorted.len() * percent).div_ceil(100) - 1] as f64 / 1_000_000.0
        };
        Self {
            p50_ms: percentile(50),
            p99_ms: percentile(99),
            samples_ns,
        }
    }
}

#[derive(Serialize)]
struct Measurement {
    name: String,
    source: String,
    elements: usize,
    mode: &'static str,
    bytes: usize,
    frames: usize,
    instructions_before_park: u64,
    capture: Timing,
    serialize: Timing,
    deserialize: Timing,
    restore: Timing,
    capture_and_serialize: Timing,
    deserialize_and_restore: Timing,
    completion_verified: bool,
    reads_without_history_provider: Option<bool>,
}

#[derive(Serialize)]
struct Report {
    architecture: &'static str,
    os: &'static str,
    debug_assertions: bool,
    format_version: u32,
    warmups: usize,
    samples_per_case: usize,
    clock: &'static str,
    measurements: Vec<Measurement>,
}

/// Benchmark provider over the actual RLM history projection. Production's
/// provider is private; these are its length/index reads, the only reads the
/// authored cell needs. The provider is deliberately recreated on restore.
struct History(RlmHistoryProjection);

impl History {
    fn answer(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        match request {
            ProjectedReadRequest::Len => Some(ProjectedReadResponse::Len(self.0.len())),
            ProjectedReadRequest::Field(field) if field.as_ref() == "length" => {
                Some(ProjectedReadResponse::Len(self.0.len()))
            }
            ProjectedReadRequest::Index(Value::Number(0.0)) => self
                .0
                .item(0)
                .and_then(|item| serde_json::to_value(item).ok())
                .map(lash_vm::from_json)
                .map(ProjectedReadResponse::Value),
            _ => None,
        }
    }
}

/// The in-process VM reads the provider directly; a run reads it over the
/// worker's wire.
impl ProjectionReader for History {
    fn read(
        &self,
        _resource: &ResourceRef,
        request: ProjectedReadRequest,
    ) -> Result<Option<ProjectedReadResponse>, ProjectionReadError> {
        Ok(self.answer(request))
    }

    fn read_range(
        &self,
        _resource: &ResourceRef,
        requests: Vec<ProjectedReadRequest>,
    ) -> Result<Vec<Option<ProjectedReadResponse>>, ProjectionReadError> {
        Ok(requests
            .into_iter()
            .map(|request| self.answer(request))
            .collect())
    }
}

/// The `history` binding: plain data naming the transcript.
fn history_binding() -> ProjectedValue {
    ProjectedValue::resource(
        lash_protocol_rlm::HISTORY_PROJECTION,
        "list",
        ResourceRef {
            projection: ProjectionType::new(lash_protocol_rlm::HISTORY_PROJECTION),
            id: "vm-snapshot".into(),
            revision: Some("0".into()),
        },
    )
}

fn history_config(elements: usize) -> Result<VmRunConfig> {
    let messages: Vec<_> = (0..elements)
        .map(|index| lash_core::Message {
            id: format!("history-{index}"),
            role: lash_core::MessageRole::User,
            parts: lash_sansio::shared_parts(vec![lash_core::Part::text(
                format!("history-{index}.text"),
                "h".repeat(1_024),
                None,
            )]),
            origin: None,
            reply_marker: None,
        })
        .collect();
    let chronological =
        lash_core::facade_support::ChronologicalProjection::from_turn_view(&[], &messages.into());
    let history = rlm_history_projection(&chronological)?;
    ensure!(history.len() == elements, "history fixture lost entries");
    let mut config = VmRunConfig::new(ExecutionMode::Foreground, ExecutionBounds::unbounded());
    config.projected = ProjectedBindings::new().with_reader(Arc::new(History(history)));
    config.projected.try_insert("history", history_binding())?;
    Ok(config)
}

struct Case {
    name: String,
    source: String,
    elements: usize,
    process: bool,
    history: bool,
    expected: f64,
}

fn cases() -> Vec<Case> {
    let mut cases = vec![Case {
        name: "small-cell".into(),
        source: "const orders = [{qty: 2, price: 3.5}, {qty: 1, price: 12}]; \
                 const total = orders.reduce((sum, order) => sum + order.qty * order.price, 0); \
                 await sleep(1); finish(total);"
            .into(),
        elements: 2,
        process: false,
        history: false,
        expected: 19.0,
    }];
    for records in [false, true] {
        for elements in [10_000, 100_000, 1_000_000] {
            let value = if records {
                "{id: i, amount: i % 100}"
            } else {
                "i"
            };
            let last = if records {
                "values[values.length - 1].id"
            } else {
                "values[values.length - 1]"
            };
            cases.push(Case {
                name: format!("{}-{elements}", if records { "records" } else { "numbers" }),
                source: format!(
                    "const values = []; for (let i = 0; i < {elements}; i++) {{ values.push({value}); }} \
                     await sleep(1); finish(values.length + {last});"
                ),
                elements,
                process: false,
                history: false,
                expected: (2 * elements - 1) as f64,
            });
        }
    }
    cases.push(Case {
        name: "deep-process".into(),
        source: "Native VM AST: 100000 counter increments, then 256 recursive callers holding depth and a captured counter, parked on sleep at depth zero.".into(),
        elements: 256,
        process: true,
        history: false,
        expected: 100_000.0 + 256.0 * 257.0 / 2.0,
    });
    for elements in [10, 10_000] {
        cases.push(Case {
            name: format!("history-{elements}"),
            source: "const held = {history}; const first = history[0]; \
                     await sleep(1); finish(held.history.length + first.content.length);"
                .into(),
            elements,
            process: false,
            history: true,
            expected: (elements + 1_024) as f64,
        });
    }
    cases
}

fn expect_sleep(instance: &mut VmInstance, mut step: VmStep) -> Result<()> {
    loop {
        match step {
            VmStep::Suspended(suspended) => match suspended.request {
                VmRequest::Effect(AbilityOp::Sleep(_)) => return Ok(()),
                VmRequest::CancelCheckpoint(_) => {
                    step = instance.resume(VmResume::CancelCheckpoint { cancelled: false })?;
                }
                other => bail!("expected quiet sleep, got {other:?}"),
            },
            other => bail!("expected suspension, got {other:?}"),
        }
    }
}

fn verify_completion(instance: &mut VmInstance, case: &Case) -> Result<bool> {
    let mut step = instance.resume(VmResume::Effect(Ok(AbilityOutcome::Value(
        Value::Undefined,
    ))))?;
    loop {
        match step {
            VmStep::Complete(complete) => {
                return Ok(
                    matches!(complete.outcome, ExecutionOutcome::Finished(Value::Number(n)) if n == case.expected),
                );
            }
            VmStep::GuestError(error) if case.history => {
                ensure!(
                    matches!(
                        error.failure.error,
                        lash_vm::RuntimeError::ProjectionRefused {
                            refusal: lash_vm::ProjectionRefusal::NoProvider { .. },
                            ..
                        }
                    ),
                    "history failed for a cause other than its missing provider: {:?}",
                    error.failure.error
                );
                return Ok(false);
            }
            VmStep::Suspended(suspended) => {
                let answer = match suspended.request {
                    VmRequest::Effect(AbilityOp::Finish(value)) => {
                        VmResume::Effect(Ok(AbilityOutcome::Value(value)))
                    }
                    VmRequest::CancelCheckpoint(_) => {
                        VmResume::CancelCheckpoint { cancelled: false }
                    }
                    VmRequest::Boundary => VmResume::Continue,
                    other => bail!("unexpected completion request: {other:?}"),
                };
                step = instance.resume(answer)?;
            }
            other => bail!("unexpected completion: {other:?}"),
        }
    }
}

fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn deep_program() -> lash_vm::Program {
    use lash_vm::{AssignTarget, CoercingBinaryOp as Op, Expr, FunctionExpr, Program};
    let var = |name: &str| Expr::Variable(name.into());
    let assign = |name: &str, value| Expr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(value),
    };
    let binary = |left, op, right| Expr::CoercingBinary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    };
    let call = |depth| Expr::Call {
        function: Box::new(var("descend")),
        args: vec![depth],
    };
    let body = Expr::If {
        condition: Box::new(binary(var("depth"), Op::StrictEqual, Expr::Number(0.0))),
        then_block: Box::new(Expr::Block(vec![
            Expr::SleepFor(Box::new(Expr::Number(1.0))),
            var("ticks"),
        ])),
        else_block: Box::new(binary(
            var("depth"),
            Op::Add,
            call(binary(var("depth"), Op::Subtract, Expr::Number(1.0))),
        )),
    };
    Program::block(vec![
        assign("ticks", Expr::Number(0.0)),
        Expr::While {
            condition: Box::new(binary(var("ticks"), Op::Less, Expr::Number(100_000.0))),
            body: Box::new(assign(
                "ticks",
                binary(var("ticks"), Op::Add, Expr::Number(1.0)),
            )),
        },
        assign(
            "descend",
            Expr::Function(Box::new(FunctionExpr {
                name: Some("descend".into()),
                js_name: Some("descend".into()),
                receiver: None,
                params: vec!["depth".into()],
                captures: vec!["ticks".into()],
                body: Box::new(body),
            })),
        ),
        Expr::Finish(Box::new(call(Expr::Number(256.0)))),
    ])
}

fn measure(mut case: Case, samples: usize) -> Result<Measurement> {
    let globals = if case.history {
        BTreeSet::from(["history".to_string()])
    } else {
        BTreeSet::new()
    };
    let parsed = if case.process {
        let program = deep_program();
        case.source = serde_json::to_string(&program)?;
        program
    } else {
        lash_typescript::parse_with_globals(&case.source, &globals)
            .map_err(|error| anyhow::anyhow!("parse {}: {error}", case.name))?
    };
    let spans = parsed.spans.clone();
    let artifact = lash_vm::ModuleArtifact::from_program(parsed)?;
    let program = Arc::new(lash_vm::compile(
        &artifact,
        lash_vm::Entry::Main,
        Some(&spans),
    )?);
    let mode = if case.process {
        ExecutionMode::Process
    } else {
        ExecutionMode::Foreground
    };
    let config = if case.history {
        history_config(case.elements)?
    } else {
        VmRunConfig::new(mode, ExecutionBounds::unbounded())
    };
    let mut instance = VmInstance::pristine();
    let step = instance.start(program.clone(), VmExecutionStart::Session, config.clone())?;
    expect_sleep(&mut instance, step)?;
    // A separately assembled host view, not a shared descriptor from the first
    // instance. Its construction is not a VM restore cost.
    let restore_config = if case.history {
        history_config(case.elements)?
    } else {
        config
    };
    let mut times = [const { Vec::new() }; 4];
    let mut bytes_count = 0;
    let mut frames = 0;
    let mut instructions = 0;
    let mut without_provider = None;
    for index in 0..samples + 1 {
        let start = Instant::now();
        let parked = instance.resume(VmResume::Park)?;
        let capture_ns = elapsed(start);
        let VmStep::Parked(parked) = parked else {
            bail!("{} did not park: {parked:?}", case.name);
        };
        frames = parked.continuation.frame_depth();
        instructions = parked.continuation.instructions_executed;
        let start = Instant::now();
        let bytes = parked.continuation.to_bytes()?;
        let serialize_ns = elapsed(start);
        if index != 0 {
            ensure!(
                bytes_count == bytes.len(),
                "snapshot size changed without guest progress"
            );
        }
        bytes_count = bytes.len();
        drop(parked);
        drop(instance);
        instance = VmInstance::pristine();
        let start = Instant::now();
        let continuation = instance.open_continuation(&bytes)?;
        let deserialize_ns = elapsed(start);
        let start = Instant::now();
        let step = instance.start(
            program.clone(),
            VmExecutionStart::Continuation(Box::new(continuation)),
            restore_config.clone(),
        )?;
        let restore_ns = elapsed(start);
        expect_sleep(&mut instance, step)?;
        if index != 0 {
            for (timing, sample) in
                times
                    .iter_mut()
                    .zip([capture_ns, serialize_ns, deserialize_ns, restore_ns])
            {
                timing.push(sample);
            }
        }
        if case.history && index == samples {
            let mut bare = VmInstance::pristine();
            let continuation = bare.open_continuation(&bytes)?;
            let step = bare.start(
                program.clone(),
                VmExecutionStart::Continuation(Box::new(continuation)),
                VmRunConfig::new(mode, ExecutionBounds::unbounded()),
            )?;
            expect_sleep(&mut bare, step)?;
            without_provider = Some(verify_completion(&mut bare, &case)?);
            ensure!(
                without_provider == Some(false),
                "history unexpectedly restored without its host view"
            );
        }
    }
    ensure!(
        verify_completion(&mut instance, &case)?,
        "{} changed its result across restore",
        case.name
    );
    let [capture, serialize, deserialize, restore] = times;
    let sum = |left: &[u64], right: &[u64]| {
        Timing::new(left.iter().zip(right).map(|(a, b)| a + b).collect())
    };
    let capture_and_serialize = sum(&capture, &serialize);
    let deserialize_and_restore = sum(&deserialize, &restore);
    Ok(Measurement {
        name: case.name,
        source: case.source,
        elements: case.elements,
        mode: if case.process {
            "process"
        } else {
            "foreground"
        },
        bytes: bytes_count,
        frames,
        instructions_before_park: instructions,
        capture: Timing::new(capture),
        serialize: Timing::new(serialize),
        deserialize: Timing::new(deserialize),
        restore: Timing::new(restore),
        capture_and_serialize,
        deserialize_and_restore,
        completion_verified: true,
        reads_without_history_provider: without_provider,
    })
}

/// Run S1's ten workloads and retain every sample, not just the percentiles.
pub fn run(out: &Path, samples: usize, selected: &[String]) -> Result<()> {
    ensure!(samples >= 100, "S1 needs at least 100 samples per case");
    let cases = cases();
    for name in selected {
        ensure!(
            cases.iter().any(|case| &case.name == name),
            "unknown S1 case: {name}"
        );
    }
    let mut report = Report {
        architecture: std::env::consts::ARCH,
        os: std::env::consts::OS,
        debug_assertions: cfg!(debug_assertions),
        format_version: lash_vm::VM_CONTINUATION_FORMAT_VERSION,
        warmups: 1,
        samples_per_case: samples,
        clock: "Instant wall time; nearest-rank percentiles",
        measurements: Vec::new(),
    };
    for case in cases
        .into_iter()
        .filter(|case| selected.is_empty() || selected.contains(&case.name))
    {
        println!("measuring {}", case.name);
        let measurement = measure(case, samples)?;
        println!(
            "{}: {} bytes, capture+encode p50/p99 {:.3}/{:.3} ms, decode+restore {:.3}/{:.3} ms",
            measurement.name,
            measurement.bytes,
            measurement.capture_and_serialize.p50_ms,
            measurement.capture_and_serialize.p99_ms,
            measurement.deserialize_and_restore.p50_ms,
            measurement.deserialize_and_restore.p99_ms
        );
        report.measurements.push(measurement);
        std::fs::write(out, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("write {}", out.display()))?;
    }
    Ok(())
}
