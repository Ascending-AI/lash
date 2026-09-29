//! Measurement prototype for FIG-4157. Run an optimized binary on the host.
#![allow(clippy::disallowed_methods, clippy::expect_used)]

#[path = "monty_comparison.rs"]
#[allow(dead_code)]
mod baseline;
#[path = "tsvm_costs/corpus.rs"]
mod corpus;
#[path = "tsvm_costs/wire.rs"]
mod wire;

use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lashlang::{CompiledProgram, ExecutionScratch, Snapshot, State, VmContinuation};
use wire::{Message, Worker};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Samples {
    raw: std::fs::File,
    summary: std::fs::File,
}

impl Samples {
    fn new(directory: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let mut raw = std::fs::File::create(directory.join("samples.csv"))?;
        writeln!(raw, "metric,sample,value,unit")?;
        let mut summary = std::fs::File::create(directory.join("summary.csv"))?;
        writeln!(summary, "metric,count,p50,p99,max,unit")?;
        Ok(Self { raw, summary })
    }

    fn record(&mut self, name: &str, values: &[u64], unit: &str) -> io::Result<()> {
        assert!(!values.is_empty());
        for (index, value) in values.iter().enumerate() {
            writeln!(self.raw, "{name},{index},{value},{unit}")?;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let p50 = sorted[(sorted.len() * 50).div_ceil(100) - 1];
        let p99 = sorted[(sorted.len() * 99).div_ceil(100) - 1];
        let max = sorted[sorted.len() - 1];
        writeln!(
            self.summary,
            "{name},{},{p50},{p99},{max},{unit}",
            sorted.len()
        )?;
        println!(
            "{name}: n={} p50={p50} p99={p99} max={max} {unit}",
            sorted.len()
        );
        Ok(())
    }
}

fn nanos(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).expect("duration fits u64")
}

fn measure(count: usize, mut operation: impl FnMut()) -> Vec<u64> {
    (0..count)
        .map(|_| {
            let start = Instant::now();
            operation();
            nanos(start)
        })
        .collect()
}

fn worker_main() -> Result<()> {
    let mut state = State::new();
    black_box(&state);
    let host = baseline::Host::default();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    wire::write_frame(
        &mut output,
        &Message {
            sequence: 0,
            operation: "ready".into(),
            payload: String::new(),
        },
    )?;
    loop {
        let message = match wire::read_frame(&mut input) {
            Ok(message) => message,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        match message.operation.as_str() {
            "echo" => wire::write_frame(&mut output, &message)?,
            "simple" => {
                baseline::simple_session(&host);
                wire::write_frame(&mut output, &message)?;
            }
            "reset" => {
                state = State::new();
                black_box(&state);
                wire::write_frame(&mut output, &message)?;
            }
            _ => return Err(io::Error::other("unknown benchmark operation").into()),
        }
    }
}

// Prototype one-owner state using the current concrete types. Scratch and
// empty caches have no Clone API; both strategies construct those fresh.
struct Instance {
    state: State,
    scratch: ExecutionScratch,
    continuation: Option<VmContinuation>,
    programs: Vec<CompiledProgram>,
}

impl Instance {
    fn fresh() -> Self {
        Self {
            state: State::new(),
            scratch: ExecutionScratch::new(),
            continuation: None,
            programs: Vec::new(),
        }
    }

    fn from_template(template: &Self) -> Self {
        Self {
            state: template.state.clone(),
            scratch: ExecutionScratch::new(),
            continuation: template.continuation.clone(),
            programs: template.programs.clone(),
        }
    }
}

fn reset_measurements(case: &corpus::Case, count: usize, samples: &mut Samples) -> Result<()> {
    let bytes = case.snapshot.to_canonical_bytes()?;
    let template = Instance::fresh();
    let mut fresh = Vec::with_capacity(count);
    let mut cloned = Vec::with_capacity(count);
    for index in 0..count {
        // Interleave A/B to reduce order drift; fixture construction is untimed.
        for strategy in [index % 2, 1 - index % 2] {
            let dirty = Instance {
                state: State::from_snapshot(Snapshot::from_canonical_bytes(&bytes)?),
                scratch: ExecutionScratch::new(),
                continuation: Some(case.continuation.clone()),
                programs: vec![case.program.clone()],
            };
            black_box((
                &dirty.state,
                &dirty.scratch,
                &dirty.continuation,
                &dirty.programs,
            ));
            let start = Instant::now();
            drop(dirty);
            let clean = if strategy == 0 {
                Instance::fresh()
            } else {
                Instance::from_template(&template)
            };
            black_box(&clean);
            let elapsed = nanos(start);
            assert!(clean.state.binding_names().next().is_none());
            assert!(clean.continuation.is_none() && clean.programs.is_empty());
            if strategy == 0 {
                fresh.push(elapsed);
            } else {
                cloned.push(elapsed);
            }
        }
    }
    samples.record(&format!("reset-fresh-{}", case.id), &fresh, "ns")?;
    samples.record(&format!("reset-template-{}", case.id), &cloned, "ns")?;
    Ok(())
}

fn state_measurements(cases: &[corpus::Case], count: usize, samples: &mut Samples) -> Result<()> {
    for case in cases {
        let continuation_bytes = serde_json::to_vec(&case.continuation)?;
        let snapshot_bytes = case.snapshot.to_canonical_bytes()?;
        let tree = serde_json::to_value(&case.continuation)?;
        fn nodes(value: &serde_json::Value) -> (u64, u64) {
            let children: Vec<_> = match value {
                serde_json::Value::Array(values) => values.iter().collect(),
                serde_json::Value::Object(values) => values.values().collect(),
                _ => Vec::new(),
            };
            children.into_iter().map(nodes).fold(
                (1, 1),
                |(count, depth), (child_count, child_depth)| {
                    (count + child_count, depth.max(child_depth + 1))
                },
            )
        }
        let (node_count, depth) = nodes(&tree);
        for (metric, value) in [
            ("source-bytes", case.source.len() as u64),
            ("continuation-bytes", continuation_bytes.len() as u64),
            ("snapshot-bytes", snapshot_bytes.len() as u64),
            ("continuation-json-nodes", node_count),
            ("continuation-json-depth", depth),
            (
                "continuation-logical-heap",
                case.continuation.heap.live_logical_bytes(),
            ),
        ] {
            samples.record(&format!("{metric}-{}", case.id), &[value], "count")?;
        }
        let timings = measure(count, || {
            black_box(serde_json::to_vec(black_box(&case.continuation)).expect("encode"));
        });
        samples.record(&format!("continuation-encode-{}", case.id), &timings, "ns")?;
        let timings = measure(count, || {
            black_box(
                serde_json::from_slice::<VmContinuation>(black_box(&continuation_bytes))
                    .expect("decode"),
            );
        });
        samples.record(
            &format!("continuation-decode-drop-{}", case.id),
            &timings,
            "ns",
        )?;
        let timings = measure(count, || {
            black_box(
                black_box(&case.snapshot)
                    .to_canonical_bytes()
                    .expect("encode"),
            );
        });
        samples.record(&format!("snapshot-encode-{}", case.id), &timings, "ns")?;
        let timings = measure(count, || {
            black_box(Snapshot::from_canonical_bytes(black_box(&snapshot_bytes)).expect("decode"));
        });
        samples.record(&format!("snapshot-decode-drop-{}", case.id), &timings, "ns")?;
    }
    Ok(())
}

fn benchmark(root: &Path, directory: &Path, warm: usize, cold: usize, idle_ms: u64) -> Result<()> {
    let mut samples = Samples::new(directory)?;
    let host = baseline::Host::default();
    let cases = corpus::load(root);
    wire::verify()?;
    baseline::simple_session(&host);
    baseline::agent_session(&host);
    samples.record(
        "timer-control",
        &measure(warm, || {
            black_box(());
        }),
        "ns",
    )?;
    let mut idle = Vec::with_capacity(cold);
    for _ in 0..cold {
        std::thread::sleep(Duration::from_millis(idle_ms));
        let start = Instant::now();
        baseline::simple_session(&host);
        idle.push(nanos(start));
    }
    samples.record("baseline-simple-after-idle", &idle, "ns")?;
    samples.record(
        "baseline-simple-warm",
        &measure(warm, || baseline::simple_session(&host)),
        "ns",
    )?;
    samples.record(
        "baseline-ten-feeds-warm",
        &measure(warm, || baseline::agent_session(&host)),
        "ns",
    )?;
    let mut spawn = Vec::with_capacity(cold);
    let mut rss = Vec::with_capacity(cold);
    for _ in 0..cold {
        let start = Instant::now();
        let worker = Worker::spawn()?;
        spawn.push(nanos(start));
        rss.push(worker.rss_kib()?);
    }
    samples.record("exec-to-ready-process-cold", &spawn, "ns")?;
    samples.record("worker-idle-rss", &rss, "KiB")?;
    let mut pool = vec![Worker::spawn()?, Worker::spawn()?];
    samples.record(
        "prewarmed-checkout",
        &measure(warm, || {
            let worker = pool.pop().expect("pool worker");
            black_box(&worker);
            pool.push(worker);
        }),
        "ns",
    )?;
    let worker = pool.last_mut().expect("pool worker");
    for size in [32, 8192, 1024 * 1024] {
        let mut message = Message {
            sequence: 0,
            operation: "echo".into(),
            payload: "x".repeat(size),
        };
        for _ in 0..100 {
            assert_eq!(worker.exchange(&message)?, message);
        }
        let mut timings = Vec::with_capacity(warm);
        for sequence in 0..warm {
            message.sequence = sequence as u64;
            let start = Instant::now();
            let response = worker.exchange(&message)?;
            timings.push(nanos(start));
            assert_eq!(response, message);
        }
        samples.record(&format!("pipe-json-roundtrip-{size}"), &timings, "ns")?;
        samples.record(
            &format!("pipe-json-wire-bytes-{size}"),
            &[serde_json::to_vec(&message)?.len() as u64 + 4],
            "bytes",
        )?;
    }
    let mut message = Message {
        sequence: 0,
        operation: "simple".into(),
        payload: String::new(),
    };
    let mut timings = Vec::with_capacity(warm);
    for sequence in 0..warm {
        message.sequence = sequence as u64;
        let start = Instant::now();
        assert_eq!(worker.exchange(&message)?, message);
        timings.push(nanos(start));
    }
    samples.record("prewarmed-simple-end-to-end", &timings, "ns")?;
    samples.record("worker-rss-after-work", &[worker.rss_kib()?], "KiB")?;
    drop(pool);
    for width in [1, 2, 4] {
        let mut workers = (0..width)
            .map(|_| Worker::spawn())
            .collect::<io::Result<Vec<_>>>()?;
        let mut timings = Vec::with_capacity(warm);
        for sequence in 0..warm {
            message.sequence = sequence as u64;
            let start = Instant::now();
            for worker in &mut workers {
                worker.send(&message)?;
            }
            for worker in &mut workers {
                assert_eq!(worker.receive()?, message);
            }
            timings.push(nanos(start));
        }
        samples.record(&format!("pool-simple-batch-{width}"), &timings, "ns")?;
        let rss = workers
            .iter()
            .map(Worker::rss_kib)
            .collect::<io::Result<Vec<_>>>()?;
        samples.record(&format!("pool-used-worker-rss-{width}"), &rss, "KiB")?;
    }
    state_measurements(&cases, warm, &mut samples)?;
    for case in cases.iter().filter(|case| case.id.starts_with("stress-")) {
        let state = State::from_snapshot(case.snapshot.clone());
        samples.record(
            &format!("parse-compile-fresh-{}", case.id),
            &measure(cold, || {
                black_box(baseline::compile_cell(&case.source, &state));
            }),
            "ns",
        )?;
    }
    for case in cases
        .iter()
        .filter(|case| case.id == "session-exotic-globals-1" || case.id == "stress-1048576")
    {
        reset_measurements(case, warm, &mut samples)?;
    }
    Ok(())
}

fn rss_probe(directory: &Path, count: usize) -> Result<()> {
    let mut worker = Worker::spawn()?;
    let message = Message {
        sequence: 42,
        operation: "simple".into(),
        payload: String::new(),
    };
    for _ in 0..100 {
        assert_eq!(worker.exchange(&message)?, message);
    }
    let rss = (0..count)
        .map(|_| worker.rss_kib())
        .collect::<io::Result<Vec<_>>>()?;
    let mut samples = Samples {
        raw: std::fs::OpenOptions::new()
            .append(true)
            .open(directory.join("samples.csv"))?,
        summary: std::fs::OpenOptions::new()
            .append(true)
            .open(directory.join("summary.csv"))?,
    };
    samples.record("worker-warm-idle-rss", &rss, "KiB")?;
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments.iter().any(|argument| argument == "--worker") {
        return worker_main();
    }
    let option = |name: &str, default: &str| {
        arguments
            .windows(2)
            .find(|pair| pair[0] == name)
            .map_or_else(|| default.to_string(), |pair| pair[1].clone())
    };
    let root = PathBuf::from(option("--repo", "."));
    if arguments.iter().any(|argument| argument == "--verify") {
        wire::verify()?;
        let cases = corpus::load(&root);
        let mut worker = Worker::spawn()?;
        let message = Message {
            sequence: 42,
            operation: "simple".into(),
            payload: String::new(),
        };
        assert_eq!(worker.exchange(&message)?, message);
        println!(
            "verified framing, worker, {} corpus snapshot/continuation round trips and resumes",
            cases.len()
        );
        return Ok(());
    }
    if cfg!(debug_assertions) {
        return Err(io::Error::other("measurements require an optimized build").into());
    }
    let warm = option("--warm", "10000").parse()?;
    let cold = option("--cold", "200").parse()?;
    assert!(warm >= 10000 && cold >= 200, "ticket sample floors");
    let output = PathBuf::from(option("--output", "tsvm-f0-data"));
    if arguments.iter().any(|argument| argument == "--rss") {
        return rss_probe(&output, warm);
    }
    benchmark(
        &root,
        &output,
        warm,
        cold,
        option("--idle-ms", "1000").parse()?,
    )
}
