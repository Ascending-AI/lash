//! Spike (FIG-6364), never lands: is a workflow run's path rebuilt exactly
//! by re-running it from what its process records, and how large would a
//! recorded trace or count delta be?
//!
//! A run is driven as a kernel process drives it (`lash-vm-runtime`
//! `process/run.rs`): each segment is a fresh machine that imports the
//! whole state the previous one exported at its park, is handed the
//! outcomes that settled, and runs until it parks or ends. The original
//! run counts every document statement start directly. Each segment is
//! then replayed from its recorded start (the whole state its `kernel_run`
//! input carries, plus its deliveries) with a statement trace on, and its
//! end (the whole state it exports, byte for byte, or its end) and its
//! trace-derived counts are compared with the original's. A second replay
//! runs the whole life on one machine from the document and the ordered
//! outcomes alone. Host reads (clock, random) are answered either from the
//! tape the original drew, or drawn afresh, to show where an unrecorded
//! read makes a replay diverge.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::graph_tokens::{ChildRef, TokenError, decode_body, encode_body};
use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, ErrorDatum, Float, FrozenDatum, Handle, Integer, Name,
    Param, Signature, Site, Timestamp, Type,
};
use lash_kernel_vm::observe::StatementAt;
use lash_kernel_vm::{
    AdmittedRequest, Bindings, Bounds, Host, KernelMachine, KernelWhole, Machine, Outcome,
    PreparedLibrary, Program, Request, Start, Step, Target, WaitId,
};
use sha2::{Digest, Sha256};

const BOUNDS: Bounds = Bounds {
    charge: 1_000_000_000,
    memory: 1 << 30,
    call_depth: 200,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

const RELEASE: ChildRef = ChildRef([0xee; 32]);
const OWNER: ChildRef = ChildRef([0x0e; 32]);

/// One workload: a lowered document, what it is started with, and where
/// it comes from.
pub(crate) struct Workload {
    pub(crate) name: String,
    pub(crate) origin: &'static str,
    pub(crate) program: Program,
    pub(crate) target: Target,
    pub(crate) args: Vec<Datum>,
}

/// The process host a segment runs with. Like `StepHost`, its clock reads
/// one instant per segment; its random bits are drawn afresh and kept, or
/// answered from a recorded tape.
struct TapeHost {
    now_ms: i64,
    state: u64,
    tape: Option<VecDeque<u64>>,
    drawn: Vec<u64>,
    clock_reads: u32,
}

impl TapeHost {
    fn fresh(now_ms: i64, seed: u64) -> Self {
        Self {
            now_ms,
            state: seed | 1,
            tape: None,
            drawn: Vec::new(),
            clock_reads: 0,
        }
    }

    fn replaying(now_ms: i64, draws: &[u64]) -> Self {
        Self {
            now_ms,
            state: 1,
            tape: Some(draws.iter().copied().collect()),
            drawn: Vec::new(),
            clock_reads: 0,
        }
    }
}

impl Host for TapeHost {
    fn clock(&mut self) -> Timestamp {
        self.clock_reads += 1;
        Timestamp {
            nanoseconds: Integer::from(self.now_ms.saturating_mul(1_000_000)),
        }
    }

    fn random(&mut self) -> u64 {
        let bits = match &mut self.tape {
            Some(tape) => tape.pop_front().unwrap_or(0),
            None => {
                // xorshift64*
                self.state ^= self.state >> 12;
                self.state ^= self.state << 25;
                self.state ^= self.state >> 27;
                self.state.wrapping_mul(0x2545_f491_4f6c_dd1d)
            }
        };
        self.drawn.push(bits);
        bits
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_string(),
            message: "a process holds no projection handle".to_string(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, _value: &Datum) {}

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// FrozenValue bodies by content digest, as the checkpoint graph holds them.
#[derive(Default)]
struct Frozen(BTreeMap<[u8; 32], Vec<u8>>);

impl Frozen {
    fn freeze(&mut self, datum: &FrozenDatum) -> Result<ChildRef, TokenError> {
        let body = encode_body(datum);
        let digest: [u8; 32] = Sha256::digest(&body).into();
        self.0.insert(digest, body);
        Ok(ChildRef(digest))
    }

    fn thaw(&self, child: ChildRef) -> Result<Datum, TokenError> {
        let body = self
            .0
            .get(&child.0)
            .ok_or_else(|| TokenError::Invalid("no such frozen value".to_owned()))?;
        decode_body::<FrozenDatum>(body).map(|frozen| frozen.0)
    }
}

/// How a segment ended: the whole state it exported at its park, or its
/// end.
#[derive(Clone, PartialEq)]
enum Ending {
    Parked(Vec<u8>),
    Ended(String),
}

impl Ending {
    fn describe(&self) -> String {
        match self {
            Self::Parked(bytes) => format!(
                "parked {}B sha256:{}",
                bytes.len(),
                &hex(&Sha256::digest(bytes))[..16]
            ),
            Self::Ended(end) => format!("ended {}", truncate(end, 120)),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn truncate(text: &str, at: usize) -> String {
    if text.len() <= at {
        text.to_string()
    } else {
        format!("{}…", &text[..at])
    }
}

/// One committed `kernel_run`, as its process records it: where it started
/// (the whole its input carries, or the start), the outcomes it was handed,
/// and how it ended. `now_ms` and `draws` are the host answers the step's
/// output would have to carry for a replay to read them.
struct Segment {
    start: Option<Vec<u8>>,
    deliveries: Vec<(WaitId, Outcome)>,
    now_ms: i64,
    draws: Vec<u64>,
    clock_reads: u32,
    end: Ending,
    /// The throwaway direct counter's counts: document statement starts.
    counts: BTreeMap<StatementAt, u64>,
    /// The original's own statement trace, for the volume figures.
    trace: Vec<(u32, StatementAt)>,
    library_starts: u64,
    ns: u128,
}

fn admitted(request: &Request) -> (EffectIdentity, AdmittedRequest) {
    match request {
        Request::Effect(effect) => (
            effect.identity.clone(),
            AdmittedRequest::Effect {
                effect: effect.effect.clone(),
                args: effect.args.clone(),
            },
        ),
        Request::Sleep(sleep) => (
            sleep.identity.clone(),
            AdmittedRequest::Sleep {
                duration: sleep.duration.into(),
            },
        ),
    }
}

fn wait_of(request: &Request) -> WaitId {
    match request {
        Request::Effect(effect) => effect.wait,
        Request::Sleep(sleep) => sleep.wait,
    }
}

fn text(value: &str) -> Datum {
    Datum::Text(value.to_string())
}

fn float(value: f64) -> Datum {
    Datum::Float(Float::new(value))
}

fn record<const N: usize>(fields: [(&str, Datum); N]) -> Datum {
    Datum::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

fn argument_text(argument: &Datum, field: &str) -> String {
    if let Datum::Record(fields) = argument
        && let Some((_, Datum::Text(text))) = fields.iter().find(|(name, _)| name == field)
    {
        return text.clone();
    }
    String::new()
}

/// The scripted host's answer to one effect.
fn answer(request: &Request) -> Outcome {
    let Request::Effect(effect) = request else {
        return Outcome::Elapsed;
    };
    let argument = effect.args.first().cloned().unwrap_or(Datum::Null);
    match effect.effect.as_str() {
        "display.show_message" | "display.set_status" | "mail.label" | "mail.archive" => {
            Outcome::Completed(Datum::Null)
        }
        "gmail.list_recent" | "mail.list" => Outcome::Completed(Datum::List(
            (0..12)
                .map(|index| {
                    record([
                        ("id", text(&format!("m{index}"))),
                        ("sender", text(&format!("sender{}@example.com", index % 4))),
                        (
                            "subject",
                            text(&format!("Subject {index}: quarterly report")),
                        ),
                        ("snippet", text("Please review the attached numbers today")),
                        ("unread", Datum::Bool(index % 3 != 0)),
                    ])
                })
                .collect(),
        )),
        "mail.classify" => {
            let id = argument_text(&argument, "id");
            let category = match id.len() % 3 {
                0 => "urgent",
                1 => "newsletter",
                _ => "normal",
            };
            Outcome::Completed(record([
                ("category", text(category)),
                ("labels", Datum::List(vec![text("inbox"), text(category)])),
                ("score", float(0.25 * (id.len() % 4) as f64)),
            ]))
        }
        "host.approval" => Outcome::Completed(text("approved")),
        "records.fetch" => Outcome::Completed(Datum::List(
            (0..400)
                .map(|index| {
                    record([
                        ("id", float(index as f64)),
                        ("region", text(["eu", "us", "apac"][index % 3])),
                        ("amount", float((index * 37 % 1000) as f64)),
                        ("tags", Datum::List(vec![text("a"), text("b")])),
                    ])
                })
                .collect(),
        )),
        "records.store" => Outcome::Completed(Datum::Bool(true)),
        name => super::gate_six::answer(name, argument),
    }
}

fn start_of(workload: &Workload) -> Start {
    Start {
        target: workload.target.clone(),
        args: workload.args.clone(),
        bindings: Bindings::default(),
    }
}

fn import(
    workload: &Workload,
    bytes: &[u8],
    frozen: &Frozen,
    admissions: &BTreeMap<EffectIdentity, AdmittedRequest>,
) -> KernelMachine {
    let whole: KernelWhole = decode_body(bytes).unwrap_or_else(|error| panic!("{error}"));
    KernelMachine::import_whole(
        workload.program.clone(),
        BOUNDS,
        whole,
        |child| frozen.thaw(child),
        |identity| admissions.get(identity).cloned(),
    )
    .unwrap_or_else(|error| panic!("{}: import: {error}", workload.name))
}

/// Runs a machine until it parks or ends.
fn to_park(machine: &mut KernelMachine, host: &mut TapeHost, name: &str) -> Step {
    to_park_sliced(machine, host, name, u64::MAX)
}

/// Runs a machine until it parks or ends, returning to the caller every
/// `slice` units of charge, as a worker's slices do.
fn to_park_sliced(
    machine: &mut KernelMachine,
    host: &mut TapeHost,
    name: &str,
    slice: u64,
) -> Step {
    loop {
        match machine
            .run(host, slice)
            .unwrap_or_else(|error| panic!("{name}: {error}"))
        {
            Step::Slice => {}
            step => return step,
        }
    }
}

fn export(machine: &mut KernelMachine, frozen: &mut Frozen, name: &str) -> Vec<u8> {
    let whole = machine
        .export_whole(RELEASE, OWNER, |datum| frozen.freeze(datum))
        .unwrap_or_else(|error| panic!("{name}: export: {error:?}"));
    encode_body(&whole)
}

/// The run as its process would commit it: one segment per `kernel_run`.
fn original(
    workload: &Workload,
    seed: u64,
) -> (
    Vec<Segment>,
    Frozen,
    BTreeMap<EffectIdentity, AdmittedRequest>,
) {
    let mut frozen = Frozen::default();
    let mut admissions = BTreeMap::new();
    let mut segments = Vec::new();
    let mut start: Option<Vec<u8>> = None;
    let mut deliveries: Vec<(WaitId, Outcome)> = Vec::new();
    for k in 0_i64.. {
        let began = Instant::now();
        let mut machine = match &start {
            None => KernelMachine::start(workload.program.clone(), BOUNDS, start_of(workload))
                .unwrap_or_else(|error| panic!("{}: {error}", workload.name)),
            Some(bytes) => import(workload, bytes, &frozen, &admissions),
        };
        for (wait, outcome) in &deliveries {
            machine
                .deliver(*wait, outcome.clone())
                .unwrap_or_else(|error| panic!("{}: deliver: {error}", workload.name));
        }
        machine.count_statements();
        machine.trace_statements();
        let now_ms = 1_700_000_000_000 + k * 7_000;
        let mut host = TapeHost::fresh(now_ms, seed ^ (k as u64).wrapping_mul(0x9e37_79b9));
        let step = to_park(&mut machine, &mut host, &workload.name);
        let ns = began.elapsed().as_nanos();
        let counts = machine.statement_counts();
        let (trace, library_starts) = machine.statement_trace();
        let mut segment = Segment {
            start: start.take(),
            deliveries: std::mem::take(&mut deliveries),
            now_ms,
            draws: host.drawn,
            clock_reads: host.clock_reads,
            end: Ending::Ended(String::new()),
            counts,
            trace,
            library_starts,
            ns,
        };
        match step {
            Step::Parked(park) => {
                for request in &park.requests {
                    let (identity, request) = admitted(request);
                    admissions.insert(identity, request);
                }
                let bytes = export(&mut machine, &mut frozen, &workload.name);
                segment.end = Ending::Parked(bytes.clone());
                deliveries = park
                    .requests
                    .iter()
                    .filter(|request| !park.withdrawn.contains(&wait_of(request)))
                    .map(|request| (wait_of(request), answer(request)))
                    .collect();
                start = Some(bytes);
                segments.push(segment);
            }
            Step::Ended(end) => {
                segment.end = Ending::Ended(format!("{end:?}"));
                segments.push(segment);
                break;
            }
            Step::Slice => unreachable!(),
        }
    }
    (segments, frozen, admissions)
}

fn histogram(trace: &[(u32, StatementAt)]) -> BTreeMap<StatementAt, u64> {
    let mut counts = BTreeMap::new();
    for (_, at) in trace {
        *counts.entry(at.clone()).or_insert(0) += 1;
    }
    counts
}

/// How the host reads of a replay are answered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// From the tape the original drew: one instant and every random draw.
    Tape,
    /// From the tape, in slices of one unit of charge.
    TapeSliced,
    /// Drawn afresh, as an unrecorded read is after a crash (`K-HOST-001`).
    Fresh,
}

struct Replayed {
    end: Ending,
    counts: BTreeMap<StatementAt, u64>,
    entries: usize,
    ns: u128,
}

/// Replays one segment from its recorded start with a statement trace on.
fn replay_segment(
    workload: &Workload,
    segment: &Segment,
    frozen: &mut Frozen,
    admissions: &BTreeMap<EffectIdentity, AdmittedRequest>,
    reads: Reads,
) -> Replayed {
    let began = Instant::now();
    let mut machine = match &segment.start {
        None => KernelMachine::start(workload.program.clone(), BOUNDS, start_of(workload))
            .unwrap_or_else(|error| panic!("{}: {error}", workload.name)),
        Some(bytes) => import(workload, bytes, frozen, admissions),
    };
    for (wait, outcome) in &segment.deliveries {
        machine
            .deliver(*wait, outcome.clone())
            .unwrap_or_else(|error| panic!("{}: deliver: {error}", workload.name));
    }
    machine.trace_statements();
    let mut host = match reads {
        Reads::Tape | Reads::TapeSliced => TapeHost::replaying(segment.now_ms, &segment.draws),
        Reads::Fresh => TapeHost::fresh(segment.now_ms + 86_401_000, 0x0005_eed0_fa11),
    };
    let slice = if reads == Reads::TapeSliced {
        1
    } else {
        u64::MAX
    };
    let step = to_park_sliced(&mut machine, &mut host, &workload.name, slice);
    let ns = began.elapsed().as_nanos();
    let (trace, _) = machine.statement_trace();
    let end = match step {
        Step::Parked(_) => Ending::Parked(export(&mut machine, frozen, &workload.name)),
        Step::Ended(end) => Ending::Ended(format!("{end:?}")),
        Step::Slice => unreachable!(),
    };
    Replayed {
        end,
        counts: histogram(&trace),
        entries: trace.len(),
        ns,
    }
}

/// Replays the whole life on one machine from the document and the
/// recorded outcomes in order, never importing a recorded state. Gives
/// how many parks matched the recorded whole byte for byte, whether the
/// end matched, and the life's trace-derived counts.
fn replay_life(
    workload: &Workload,
    segments: &[Segment],
    frozen: &mut Frozen,
) -> (usize, bool, BTreeMap<StatementAt, u64>, Option<String>) {
    let mut machine = KernelMachine::start(workload.program.clone(), BOUNDS, start_of(workload))
        .unwrap_or_else(|error| panic!("{}: {error}", workload.name));
    machine.trace_statements();
    let mut matched = 0;
    for (k, segment) in segments.iter().enumerate() {
        for (wait, outcome) in &segment.deliveries {
            if let Err(error) = machine.deliver(*wait, outcome.clone()) {
                let (trace, _) = machine.statement_trace();
                return (
                    matched,
                    false,
                    histogram(&trace),
                    Some(format!("segment {k}: deliver: {error}")),
                );
            }
        }
        let mut host = TapeHost::replaying(segment.now_ms, &segment.draws);
        let step = to_park(&mut machine, &mut host, &workload.name);
        match (step, &segment.end) {
            (Step::Parked(_), Ending::Parked(recorded)) => {
                // Exporting a running machine changes nothing it does
                // (`K-MACH-007`).
                let bytes = export(&mut machine, frozen, &workload.name);
                if &bytes == recorded {
                    matched += 1;
                } else {
                    let (trace, _) = machine.statement_trace();
                    return (
                        matched,
                        false,
                        histogram(&trace),
                        Some(format!("segment {k}: the park's whole differs")),
                    );
                }
            }
            (Step::Ended(end), Ending::Ended(recorded)) => {
                let (trace, _) = machine.statement_trace();
                return (
                    matched,
                    &format!("{end:?}") == recorded,
                    histogram(&trace),
                    None,
                );
            }
            (step, _) => {
                let (trace, _) = machine.statement_trace();
                return (
                    matched,
                    false,
                    histogram(&trace),
                    Some(format!(
                        "segment {k}: {} where the original {}",
                        match step {
                            Step::Parked(_) => "parked",
                            Step::Ended(_) => "ended",
                            Step::Slice => "sliced",
                        },
                        segment.end.describe()
                    )),
                );
            }
        }
    }
    let (trace, _) = machine.statement_trace();
    (
        matched,
        false,
        histogram(&trace),
        Some("no end".to_string()),
    )
}

fn sum_counts<'a>(
    counts: impl Iterator<Item = &'a BTreeMap<StatementAt, u64>>,
) -> BTreeMap<StatementAt, u64> {
    let mut total = BTreeMap::new();
    for counts in counts {
        for (at, count) in counts {
            *total.entry(at.clone()).or_insert(0) += count;
        }
    }
    total
}

/// What a block-entry trace of one segment would hold, read off the
/// statement-start trace: block entries (a start at index 0), and the
/// exits a decoder needs to derive statement counts from them: every block
/// left before its last statement started, abruptly or still open when the
/// segment stopped. Empty blocks start no statement and are not seen.
#[derive(Default, Clone, Copy)]
struct Volume {
    statements: u64,
    block_entries: u64,
    abrupt_exits: u64,
    open_at_end: u64,
    task_switches: u64,
    touched: u64,
    library_statements: u64,
}

fn volume(segment: &Segment, blocks: &BTreeMap<Site, u32>) -> Volume {
    let mut volume = Volume {
        statements: segment.trace.len() as u64,
        touched: segment.counts.len() as u64,
        library_statements: segment.library_starts,
        ..Volume::default()
    };
    // Each task's open blocks, innermost last, with the index last started.
    let mut stacks: BTreeMap<u32, Vec<(Site, u32)>> = BTreeMap::new();
    let mut last_task = None;
    let left = |site: &Site, index: u32| -> bool {
        // A block left with statements after the last one started.
        blocks.get(site).is_some_and(|len| index + 1 < *len)
    };
    for (task, at) in &segment.trace {
        if last_task.is_some_and(|last| last != *task) {
            volume.task_switches += 1;
        }
        last_task = Some(*task);
        let stack = stacks.entry(*task).or_default();
        if at.index == 0 {
            volume.block_entries += 1;
            // A loop body entered again: its previous iteration ended.
            if stack.last().is_some_and(|(site, _)| *site == at.block) {
                let (site, index) = stack.pop().unwrap_or_else(|| unreachable!());
                if left(&site, index) {
                    volume.abrupt_exits += 1;
                }
            }
            stack.push((at.block.clone(), 0));
        } else {
            // Blocks above the one this statement is in have ended.
            while stack.last().is_some_and(|(site, _)| *site != at.block) {
                let (site, index) = stack.pop().unwrap_or_else(|| unreachable!());
                if left(&site, index) {
                    volume.abrupt_exits += 1;
                }
            }
            match stack.last_mut() {
                Some((_, index)) => *index = at.index,
                // Resumed after a park: the block was entered in an
                // earlier segment.
                None => stack.push((at.block.clone(), at.index)),
            }
        }
    }
    for stack in stacks.values() {
        volume.open_at_end += stack
            .iter()
            .filter(|(site, index)| left(site, *index))
            .count() as u64;
    }
    volume
}

fn quantiles(values: &mut [u64]) -> String {
    if values.is_empty() {
        return "-".to_string();
    }
    values.sort_unstable();
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    format!(
        "min {} p50 {} p90 {} p99 {} max {}",
        values[0],
        at(0.5),
        at(0.9),
        at(0.99),
        values[values.len() - 1]
    )
}

/// Runs every workload, checks its replays, and prints one row per
/// workload and one per segment as TSV under `dir`.
#[expect(
    clippy::print_stdout,
    clippy::disallowed_methods,
    reason = "spike entry point: it reports on stdout and writes its tables to its directory"
)]
pub(crate) fn run(dir: &std::path::Path, workloads: &[Workload]) -> Result<(), String> {
    let mut summary = String::from(
        "workload\torigin\tsegments\tstatement_starts\tblock_entries\tabrupt_exits\topen_at_end\ttask_switches\tlibrary_statement_starts\tclock_reads\trandom_draws\tseg_tape_match\tseg_tape_slice1_match\tseg_fresh_match\tlife_parks_matched\tlife_end_matched\tlife_counts_equal\tdifferential_counts_equal\toriginal_ns\treplay_ns\tdivergence\n",
    );
    let mut rows = String::from(
        "workload\tsegment\tstatement_starts\tblock_entries\tabrupt_exits\topen_at_end\ttask_switches\ttouched_statements\tlibrary_statement_starts\tclock_reads\trandom_draws\tstate_bytes\ttape_match\tfresh_match\n",
    );
    let mut all_statements = Vec::new();
    let mut all_blocks = Vec::new();
    let mut life_statements = Vec::new();
    let mut life_blocks = Vec::new();
    let mut failures = Vec::new();
    for workload in workloads {
        let (segments, mut frozen, admissions) = original(workload, 0xdead_beef);
        let blocks = {
            let machine =
                KernelMachine::start(workload.program.clone(), BOUNDS, start_of(workload))
                    .map_err(|error| error.to_string())?;
            machine.document_blocks()
        };
        let mut tape_match = 0;
        let mut fresh_match = 0;
        let mut sliced_match = 0;
        let mut differential = true;
        let mut divergence = Vec::new();
        let mut original_ns = 0;
        let mut replay_ns = 0;
        let mut life = Volume::default();
        let (mut clock_reads, mut draws) = (0u64, 0u64);
        for (k, segment) in segments.iter().enumerate() {
            let tape = replay_segment(workload, segment, &mut frozen, &admissions, Reads::Tape);
            let fresh = replay_segment(workload, segment, &mut frozen, &admissions, Reads::Fresh);
            let sliced = replay_segment(
                workload,
                segment,
                &mut frozen,
                &admissions,
                Reads::TapeSliced,
            );
            sliced_match +=
                usize::from(sliced.end == segment.end && sliced.counts == segment.counts);
            original_ns += segment.ns;
            replay_ns += tape.ns;
            let tape_ok = tape.end == segment.end && tape.counts == segment.counts;
            let fresh_ok = fresh.end == segment.end && fresh.counts == segment.counts;
            tape_match += usize::from(tape_ok);
            fresh_match += usize::from(fresh_ok);
            // The differential law: counts derived from the trace equal
            // counts taken directly, in the same run and in the replay.
            let own = histogram(&segment.trace) == segment.counts;
            differential &= own && tape.entries == segment.trace.len();
            if !tape_ok {
                divergence.push(format!(
                    "seg{k} tape: {} vs {}",
                    tape.end.describe(),
                    segment.end.describe()
                ));
            }
            if !fresh_ok {
                let path = if fresh.counts == segment.counts {
                    "same path"
                } else {
                    "other path"
                };
                divergence.push(format!(
                    "seg{k} fresh reads: {path}, {}",
                    if fresh.end == segment.end {
                        "same end"
                    } else {
                        "other end"
                    }
                ));
            }
            let volume = volume(segment, &blocks);
            life.statements += volume.statements;
            life.block_entries += volume.block_entries;
            life.abrupt_exits += volume.abrupt_exits;
            life.open_at_end += volume.open_at_end;
            life.task_switches += volume.task_switches;
            life.library_statements += volume.library_statements;
            clock_reads += u64::from(segment.clock_reads);
            draws += segment.draws.len() as u64;
            all_statements.push(volume.statements);
            all_blocks.push(volume.block_entries);
            rows.push_str(&format!(
                "{}\t{k}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{tape_ok}\t{fresh_ok}\n",
                workload.name,
                volume.statements,
                volume.block_entries,
                volume.abrupt_exits,
                volume.open_at_end,
                volume.task_switches,
                volume.touched,
                volume.library_statements,
                segment.clock_reads,
                segment.draws.len(),
                match &segment.end {
                    Ending::Parked(bytes) => bytes.len(),
                    Ending::Ended(_) => 0,
                },
            ));
        }
        life_statements.push(life.statements);
        life_blocks.push(life.block_entries);
        let (parks, end, life_counts, life_divergence) =
            replay_life(workload, &segments, &mut frozen);
        let summed = sum_counts(segments.iter().map(|segment| &segment.counts));
        let life_equal = life_counts == summed;
        if let Some(life_divergence) = life_divergence {
            divergence.push(format!("life: {life_divergence}"));
        }
        if tape_match != segments.len()
            || sliced_match != segments.len()
            || !differential
            || !life_equal
            || !end
        {
            failures.push(workload.name.clone());
        }
        summary.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{clock_reads}\t{draws}\t{tape_match}/{}\t{sliced_match}/{}\t{fresh_match}/{}\t{parks}/{}\t{end}\t{life_equal}\t{differential}\t{original_ns}\t{replay_ns}\t{}\n",
            workload.name,
            workload.origin,
            segments.len(),
            life.statements,
            life.block_entries,
            life.abrupt_exits,
            life.open_at_end,
            life.task_switches,
            life.library_statements,
            segments.len(),
            segments.len(),
            segments.len(),
            segments.len().saturating_sub(1),
            divergence.join("; "),
        ));
    }
    let mut distribution = String::new();
    distribution.push_str(&format!(
        "statement starts per segment ({} segments): {}\n",
        all_statements.len(),
        quantiles(&mut all_statements.clone())
    ));
    distribution.push_str(&format!(
        "block entries per segment: {}\n",
        quantiles(&mut all_blocks.clone())
    ));
    distribution.push_str(&format!(
        "statement starts per process ({} workloads): {}\n",
        life_statements.len(),
        quantiles(&mut life_statements.clone())
    ));
    distribution.push_str(&format!(
        "block entries per process: {}\n",
        quantiles(&mut life_blocks.clone())
    ));
    for cap in [64_u64, 256, 1024, 4096] {
        let over = all_blocks.iter().filter(|entries| **entries > cap).count();
        let over_life = life_blocks.iter().filter(|entries| **entries > cap).count();
        distribution.push_str(&format!(
            "block-entry cap {cap}: {over}/{} segments and {over_life}/{} processes over it\n",
            all_blocks.len(),
            life_blocks.len()
        ));
    }
    std::fs::write(dir.join("replay-summary.tsv"), &summary).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("replay-segments.tsv"), &rows).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("replay-distribution.txt"), &distribution)
        .map_err(|e| e.to_string())?;
    print!("{summary}");
    print!("{distribution}");
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("replay diverged with the tape: {failures:?}"))
    }
}

/// Effects the workflow workloads perform, each with one `Any` input.
pub(crate) fn workflow_effects() -> BTreeMap<EffectName, Signature> {
    [
        "display.show_message",
        "display.set_status",
        "gmail.list_recent",
        "host.approval",
        "mail.list",
        "mail.classify",
        "mail.label",
        "mail.archive",
        "records.fetch",
        "records.store",
    ]
    .into_iter()
    .map(|name| {
        (
            EffectName::new(name).unwrap_or_else(|error| panic!("{name}: {error:?}")),
            Signature {
                params: vec![Param {
                    name: Name::new("input"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
        )
    })
    .collect()
}

/// The av-probe workflows (`/workspace/tmp/av-probe/*.ts`), the example
/// host flow's process, and three written for this spike: a triage that
/// branches per message, a batch transform with a heavy pure stretch, and
/// a run that reads the clock and draws random bits.
pub(crate) const WORKFLOWS: &[(&str, &str, &str, &str)] = &[
    (
        "digest",
        "av-probe",
        "digest",
        r#"export async function digest(topics) {
  let seen = 0;
  for (const topic of topics) {
    await sleep(7000);
    if (topic.urgent) {
      await sleep(1000);
    } else {
      await sleep(2000);
    }
    seen = seen + 1;
  }
  return { seen };
}"#,
    ),
    (
        "typed",
        "av-probe",
        "digest",
        r#"type Topic = { title: string; urgent: boolean };
export async function digest(topics: Topic[]) {
  let seen = 0;
  for (const topic of topics) {
    await sleep(7000);
    if (topic.urgent) {
      await sleep(1000);
    } else {
      await sleep(2000);
    }
    seen = seen + 1;
  }
  return { seen };
}"#,
    ),
    (
        "inbox",
        "av-probe",
        "digest",
        r#"export async function digest(items) {
  let sent = 0;
  for (const item of items) {
    await sleep(6000);
    await display.show_message({ text: item.title });
    sent = sent + 1;
  }
  return { sent };
}"#,
    ),
    (
        "flat",
        "av-probe",
        "demo",
        r#"export async function demo() {
  const greeting = "Inbox review started";
  await display.show_message({ text: greeting });
  const emails = await gmail.list_recent({ count: 3 });
  await display.set_status({ key: "inbox", value: "Reviewed three mock emails" });
  return emails;
}"#,
    ),
    (
        "approval",
        "examples/typescript-host-flows",
        "approval_process",
        r#"export async function approval_process(request) {
  const decision = await host.approval({ request });
  await sleep(25);
  return { request, decision };
}"#,
    ),
    (
        "triage",
        "spike",
        "triage",
        r#"export async function triage(limit) {
  const messages = await mail.list({ count: limit });
  const counts = { urgent: 0, newsletter: 0, normal: 0 };
  const urgent = [];
  for (const message of messages) {
    if (!message.unread) {
      continue;
    }
    const verdict = await mail.classify({ id: message.id, subject: message.subject });
    counts[verdict.category] = counts[verdict.category] + 1;
    if (verdict.category === "urgent") {
      urgent.push(`${message.sender}: ${message.subject}`);
      await mail.label({ id: message.id, labels: verdict.labels });
    } else if (verdict.category === "newsletter") {
      try {
        await mail.archive({ id: message.id });
      } catch (error) {
        counts.normal = counts.normal + 1;
      }
    } else {
      const words = message.snippet.split(" ");
      let long = 0;
      for (const word of words) {
        if (word.length > 5) {
          long = long + 1;
        }
      }
      if (long > 2) {
        await sleep(1000);
      }
    }
  }
  await display.show_message({ text: urgent.join("\n") });
  return counts;
}"#,
    ),
    (
        "transform",
        "spike",
        "transform",
        r#"export async function transform() {
  const rows = await records.fetch({ page: 1 });
  const totals = {};
  const seen = [];
  for (const record of rows) {
    if (record.amount < 100) {
      continue;
    }
    const key = record.region;
    if (totals[key] === undefined) {
      totals[key] = 0;
    }
    totals[key] = totals[key] + record.amount;
    if (record.amount > 900) {
      seen.push(record.id);
    }
  }
  const ok = await records.store({ totals, seen });
  return { ok, regions: Object.keys(totals).length, seen: seen.length };
}"#,
    ),
    (
        "stamped",
        "spike",
        "stamped",
        r#"export async function stamped(rounds) {
  const started = Date.now();
  let heads = 0;
  for (let i = 0; i < rounds; i++) {
    await sleep(1000);
    if (Math.floor(Date.now() / 1000) % 2 === 0 || Math.random() > 0.5) {
      heads = heads + 1;
    } else {
      await sleep(10);
    }
  }
  return { started, heads };
}"#,
    ),
];

/// The arguments each workflow starts with, by workload name.
pub(crate) fn workflow_args(name: &str, size: usize) -> Vec<Datum> {
    match name {
        "digest" | "typed" => vec![Datum::List(
            (0..size)
                .map(|index| {
                    record([
                        ("title", text(&format!("topic {index}"))),
                        ("urgent", Datum::Bool(index % 3 == 1)),
                    ])
                })
                .collect(),
        )],
        "inbox" => vec![Datum::List(
            (0..size)
                .map(|index| record([("title", text(&format!("item {index}")))]))
                .collect(),
        )],
        "approval" => vec![record([("id", text("req-1"))])],
        "triage" => vec![float(size as f64)],
        "stamped" => vec![float(size as f64)],
        _ => Vec::new(),
    }
}

/// The `lash-durable-test` retained-image sources (`corpus.json`, set
/// `4b1b3a0d68`), as kernel text.
pub(crate) const CORPUS: &[(&str, &str)] = &[
    (
        "corpus-loop-closures-aliases",
        "kernel 1\nnumbers float\nentry worker() -> Any\nfn worker() {\n  let values = [0]\n  let alias = values\n  let count = 0\n  let bump = fn() { set count = num.add(count, 1) return count }\n  for step in [1, 2, 3] {\n    do apply bump()\n    set alias[0] = num.add(values[0], step)\n    do sleep 5000\n  }\n  return (values[0], count, alias[0])\n}\nmain { finish null }\n",
    ),
    (
        "corpus-task-join",
        "kernel 1\nnumbers float\nentry worker() -> Any\nfn child() { do sleep 5000 return \"joined child\" }\nfn worker() { let task = spawn call child() let answer = join task return answer }\nmain { finish null }\n",
    ),
    (
        "spike-kernel-random",
        "kernel 1\nnumbers float\nentry worker() -> Any\nfn worker() {\n  let heads = 0\n  for i in [1, 2, 3, 4, 5, 6, 7, 8] {\n    do sleep 1000\n    if num.lt(random, 0.5) { set heads = num.add(heads, 1) } else { do sleep 10 }\n  }\n  return heads\n}\nmain { finish null }\n",
    ),
    (
        "corpus-suspending-cleanup",
        "kernel 1\nnumbers float\nentry worker() -> Any\nfn worker() {\n  let values = [\"before\"]\n  try { return values } finally {\n    do sleep 5000\n    set values[0] = \"cleanup completed\"\n  }\n}\nmain { finish null }\n",
    ),
];

/// Builds every workload against `registry` and `prepared`.
pub(crate) fn workloads(
    registry: &lash_kernel_doc::FunctionRegistry,
    prepared: &PreparedLibrary,
    library: &NamedLibrary,
    cells: Vec<(String, String)>,
    environment_effects: &BTreeMap<EffectName, Signature>,
) -> Result<Vec<Workload>, String> {
    let mut out = Vec::new();
    let effects = workflow_effects();
    let controls = BTreeMap::from([(
        EffectName::new("finish").map_err(|error| format!("{error:?}"))?,
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let roots: BTreeSet<Name> = [
        "display",
        "gmail",
        "host",
        "mail",
        "records",
        "processes",
        "jobs",
    ]
    .into_iter()
    .map(Name::new)
    .collect();
    let environment = Environment {
        library,
        effects: &effects,
        tool_roots: &roots,
        controls: &controls,
        bindings: &BTreeSet::new(),
        functions: &BTreeMap::new(),
    };
    for (name, origin, entry, source) in WORKFLOWS {
        let sizes: &[usize] = match *name {
            "digest" | "typed" | "inbox" => &[3, 20],
            "triage" => &[12],
            "stamped" => &[8],
            _ => &[0],
        };
        for size in sizes {
            let lowered = lash_dialect_typescript::lower_workflow(source, &environment)
                .map_err(|error| format!("{name}: {error:?}"))?;
            out.push(Workload {
                name: if *size > 0 {
                    format!("{name}[{size}]")
                } else {
                    (*name).to_string()
                },
                origin,
                program: Program {
                    document: Arc::new(lowered.document),
                    library: prepared.clone(),
                },
                target: Target::Entry(Name::new(*entry)),
                args: workflow_args(name, *size),
            });
        }
    }
    let id_of = |wanted: &str| {
        registry
            .iter()
            .find(|(_, entry)| entry.definition().name.to_string() == wanted)
            .map(|(id, _)| *id)
            .ok_or(format!("{wanted} is shipped"))
    };
    let (add, lt) = (id_of("num.add")?, id_of("num.lt")?);
    for (name, source) in CORPUS {
        let source = format!("{source}use num.add = @{add}\nuse num.lt = @{lt}\n");
        let document =
            lash_kernel_doc::parse_document(&source).map_err(|error| format!("{name}: {error}"))?;
        out.push(Workload {
            name: (*name).to_string(),
            origin: if name.starts_with("corpus") {
                "lash-durable-test corpus"
            } else {
                "spike"
            },
            program: Program {
                document: Arc::new(document),
                library: prepared.clone(),
            },
            target: Target::Entry(Name::new("worker")),
            args: Vec::new(),
        });
    }
    let cell_environment = Environment {
        effects: environment_effects,
        ..environment
    };
    for (name, source) in cells {
        let lowered = lash_dialect_typescript::lower(&source, &cell_environment)
            .map_err(|error| format!("{name}: {error:?}"))?;
        out.push(Workload {
            name: format!("cell-{name}"),
            origin: "gate-six cells",
            program: Program {
                document: Arc::new(lowered.document),
                library: prepared.clone(),
            },
            target: Target::Main,
            args: Vec::new(),
        });
    }
    Ok(out)
}
