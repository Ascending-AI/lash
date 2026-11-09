//! The cross-process continuation runner (`r57/l7-runner-interface.md`,
//! version 2): every corpus case is run once uninterrupted, and saved at
//! every legal pause of the conformance sweep, so that a second build can
//! open each saved image in a fresh machine and run it to its end.
//!
//! ```text
//! continuation-compat export --corpus CORPUS --out EXPORT_DIR
//! continuation-compat uninterrupted --corpus CORPUS --out RUNS_JSON
//! continuation-compat resume --corpus CORPUS --imports EXPORT_DIR --out RESUMED_JSON
//! continuation-compat resume --expect-refusal --expected N --observed N --corpus CORPUS --imports EXPORT_DIR --out REFUSALS_JSON
//! ```
//!
//! The sweep pauses once before the first step, then runs with slice 1 and
//! pauses after every slice return, at every park before its scripted
//! deliveries and again after them. Scripted host
//! answers and delivery batches are consumed by an explicit cursor, which an
//! image saves, so an imported continuation goes on under the same script.
//!
//! Every failure exits nonzero; an engine that refuses to go on from a
//! state it reached itself fails the case.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{
    Checkpoint, PageAnswer, PageBody, PageKind, PageSource, StatedFormat,
};
use lash_kernel_value::sites::WaitId;
use lash_kernel_vm::{
    CellStart, Delivered, Host, HostFault, KernelMachine, Machine, MachineError, PreparedLibrary,
    Program, Request, StartError, Step, Target,
};
use serde::{Deserialize, Serialize};

use crate::{Case, CaseBounds, ExpectedEnd, HarnessError, HostAnswer, Observations, Shard, Trace};

/// The runner's export and observation protocol with
/// `scripts/continuation_compat.py`.
/// version_surface = "coexist"
/// format_outside_manifest = "conformance runner exchange files read by the compatibility gate; not durable execution state"
/// version_guard(roots(Manifest, Image, Runs, Refusal))
const VERSION: u32 = 2;
/// The sweep's slice (`r57/l7-runner-interface.md`: slice size 1).
const SLICE: u64 = 1;
/// Steps after which a run is an error rather than an observation.
const STEP_BOUND: usize = 10_000_000;

fn fault(error: impl std::fmt::Display) -> HarnessError {
    HarnessError(error.to_string())
}

/// The registry every corpus shard runs against: the kernel's machine
/// functions and the standard natives the shards name.
pub fn registry() -> Result<lash_kernel_doc::FunctionRegistry, HarnessError> {
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    lash_kernel_vm::register_machine_functions(&mut registry).map_err(fault)?;
    lash_kernel_lib::register_numbers(&mut registry).map_err(fault)?;
    lash_kernel_lib::register_text_json(&mut registry).map_err(fault)?;
    lash_kernel_lib::register_collections(&mut registry).map_err(fault)?;
    lash_ext_regex_ecma::register(&mut registry).map_err(fault)?;
    lash_ext_regex_ecma::implement(
        &mut registry,
        &Arc::new(lash_ext_regex_ecma::Engine::new(4)),
    )
    .map_err(fault)?;
    Ok(registry)
}

/// Runs one command line (without the program name).
pub fn cli(args: &[String]) -> Result<(), HarnessError> {
    let usage = || {
        fault(
            "usage: continuation-compat (export|uninterrupted|resume [--expect-refusal \
             --expected N --observed N]) --corpus CORPUS [--imports EXPORT_DIR] --out PATH",
        )
    };
    let mut args = args.iter();
    let command = args.next().ok_or_else(usage)?.clone();
    let (mut corpus, mut imports, mut out, mut refusal) = (None, None, None, false);
    let (mut expected, mut observed) = (None, None);
    let witness = |value: Option<&String>| value.and_then(|value| value.parse::<u32>().ok());
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--corpus" => corpus = args.next().map(PathBuf::from),
            "--imports" => imports = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--expect-refusal" if command == "resume" => refusal = true,
            "--expected" => expected = Some(witness(args.next()).ok_or_else(usage)?),
            "--observed" => observed = Some(witness(args.next()).ok_or_else(usage)?),
            _ => return Err(usage()),
        }
    }
    let corpus = load(&corpus.ok_or_else(usage)?)?;
    let out = out.ok_or_else(usage)?;
    let library = PreparedLibrary::new(Arc::new(registry()?));
    match (command.as_str(), imports) {
        ("export", None) => export(&library, &corpus, &out),
        ("uninterrupted", None) => uninterrupted(&library, &corpus, &out),
        ("resume", Some(imports)) if refusal => {
            // The parked-state witnesses live with the embedding, outside the
            // kernel crates this runner may name; the orchestrator states both
            // builds' values, as a store states its envelope's.
            let stated = StatedFormat {
                surface: "KERNEL_PARKED_STATE_VERSION",
                expected: expected.ok_or_else(usage)?,
                observed: observed.ok_or_else(usage)?,
            };
            refuse(&library, &corpus, &imports, &out, stated)
        }
        _ if expected.is_some() || observed.is_some() => Err(usage()),
        ("resume", Some(imports)) => resume(&library, &corpus, &imports, &out),
        _ => Err(usage()),
    }
}

/// Every case of every shard under `root`, by id (`<shard path>#<index>`),
/// in path order.
#[expect(
    clippy::disallowed_methods,
    reason = "the runner is a command-line host: it reads the corpus and images it is given and writes its output files"
)]
fn load(root: &Path) -> Result<Vec<(String, Case)>, HarnessError> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(fault)? {
            let path = entry.map_err(fault)?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                files.push(path);
            }
        }
    }
    let mut cases = Vec::new();
    let mut shards: Vec<(String, PathBuf)> = files
        .into_iter()
        .map(|path| {
            let relative = path
                .strip_prefix(root)
                .map_err(fault)?
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            Ok((relative, path))
        })
        .collect::<Result<_, HarnessError>>()?;
    shards.sort();
    for (relative, path) in shards {
        let text = std::fs::read_to_string(&path).map_err(fault)?;
        let shard: Shard =
            serde_json::from_str(&text).map_err(|error| fault(format!("{relative}: {error}")))?;
        for (index, case) in shard.cases.into_iter().enumerate() {
            cases.push((format!("{relative}#{index}"), case));
        }
    }
    if cases.is_empty() {
        return Err(fault("the corpus has no cases"));
    }
    Ok(cases)
}

/// What a run observed, in the interface's canonical shape.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observed {
    prints: Vec<Datum>,
    end: Option<ExpectedEnd>,
    trace: Vec<Trace>,
    charged: Option<u64>,
    charge_tape: Vec<Charge>,
    steps: Vec<StepRecord>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Charge {
    Units(u64),
    Atomic(u64),
}

impl From<lash_kernel_exec::Weight> for Charge {
    fn from(weight: lash_kernel_exec::Weight) -> Self {
        match weight {
            lash_kernel_exec::Weight::Units(n) => Self::Units(n),
            lash_kernel_exec::Weight::Atomic(n) => Self::Atomic(n),
        }
    }
}

/// One legal return to the embedder: its phase, the run's charge there,
/// and how many prints, requests and charges preceded it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepRecord {
    phase: Phase,
    charged: Option<u64>,
    prints: usize,
    requests: usize,
    charges: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Start,
    Slice,
    Park,
    Delivered,
    End,
}

/// Where a run is in its script.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    /// Host answers consumed.
    answers: usize,
    /// Delivery batches consumed.
    batches: usize,
    /// Every request's wait, in issue order.
    waits: Vec<WaitId>,
}

/// A run in progress: its machine, script position and observations.
struct Running {
    machine: KernelMachine,
    tape: lash_kernel_vm::ChargeTape,
    /// The charges recorded before this machine was opened.
    earlier: Vec<Charge>,
    cursor: Cursor,
    observed: Observed,
    operations: BTreeMap<lash_kernel_value::OperationUse, String>,
}

impl Running {
    fn new(
        mut machine: KernelMachine,
        program: &Program,
        cursor: Cursor,
        observed: Observed,
    ) -> Self {
        let tape = lash_kernel_vm::ChargeTape::new();
        machine.record_charges(tape.clone());
        Self {
            machine,
            tape,
            earlier: observed.charge_tape.clone(),
            cursor,
            observed,
            operations: program
                .operation_imports()
                .map(|(use_, import)| (use_, import.name.to_string()))
                .collect(),
        }
    }

    fn charges(&self) -> Vec<Charge> {
        let mut charges = self.earlier.clone();
        charges.extend(self.tape.steps().into_iter().map(Charge::from));
        charges
    }

    fn record(&mut self, phase: Phase) {
        let charges = self.earlier.len() + self.tape.len();
        self.observed.steps.push(StepRecord {
            phase,
            charged: self.machine.meters().charged,
            prints: self.observed.prints.len(),
            requests: self.observed.trace.len(),
            charges,
        });
    }

    /// The observations so far, as an image saves them.
    fn prefix(&self) -> Observed {
        Observed {
            charged: self.machine.meters().charged,
            charge_tape: self.charges(),
            ..self.observed.clone()
        }
    }

    /// Delivers the next scripted batch at a park, then pauses.
    fn deliver(
        &mut self,
        case: &Case,
        pause: &mut dyn FnMut(&mut Self, Phase) -> Result<(), HarnessError>,
    ) -> Result<(), HarnessError> {
        let batch = case
            .environment
            .deliveries
            .get(self.cursor.batches)
            .ok_or_else(|| {
                fault(format!(
                    "park {} has no scripted delivery batch",
                    self.cursor.batches + 1
                ))
            })?;
        self.cursor.batches += 1;
        for delivery in batch {
            let wait = self.cursor.waits.get(delivery.request).ok_or_else(|| {
                fault(format!("request {} has not been issued", delivery.request))
            })?;
            let delivered = self
                .machine
                .deliver(*wait, delivery.outcome.clone().into())
                .map_err(fault)?;
            if (delivered == Delivered::Dropped) != delivery.dropped {
                return Err(fault(format!("unexpected delivery result {delivered:?}")));
            }
        }
        self.record(Phase::Delivered);
        pause(self, Phase::Delivered)
    }

    /// Runs to the end, calling `pause` at every legal pause.
    fn drive(
        mut self,
        case: &Case,
        pause: &mut dyn FnMut(&mut Self, Phase) -> Result<(), HarnessError>,
    ) -> Result<Observed, HarnessError> {
        let env = &case.environment;
        for _ in 0..STEP_BOUND {
            let mut host = Script {
                answers: &env.host,
                cursor: &mut self.cursor.answers,
                prints: &mut self.observed.prints,
                fault: None,
                operations: &self.operations,
            };
            let step = self.machine.run(&mut host, SLICE);
            if let Some(problem) = host.fault.take() {
                return Err(fault(problem));
            }
            let end = match step {
                Ok(Step::Slice) => {
                    self.record(Phase::Slice);
                    pause(&mut self, Phase::Slice)?;
                    continue;
                }
                Ok(Step::Parked(park)) => {
                    for request in &park.requests {
                        let Request::Effect(effect) = request;
                        self.cursor.waits.push(effect.wait);
                        self.observed.trace.push(Trace::Effect {
                            identity: Some(
                                lash_kernel_value::kernel_internals::effect_parts(&effect.identity)
                                    .clone(),
                            ),
                            effect: self
                                .operations
                                .get(&effect.use_)
                                .cloned()
                                .ok_or_else(|| fault("undeclared Operation Use"))?,
                            args: effect.args.clone(),
                            result: effect.result.clone(),
                        });
                    }
                    self.record(Phase::Park);
                    pause(&mut self, Phase::Park)?;
                    self.deliver(case, pause)?;
                    continue;
                }
                Ok(Step::Ended(end)) => ExpectedEnd::from_end(end),
                Err(MachineError::Host(_)) => ExpectedEnd::HostFault,
                Err(error) => return Err(fault(error.to_string())),
            };
            if self.cursor.batches < env.deliveries.len() || self.cursor.answers < env.host.len() {
                return Err(fault("the run ended before its script was consumed"));
            }
            self.record(Phase::End);
            self.observed.charged = self.machine.meters().charged;
            self.observed.charge_tape = self.charges();
            self.observed.end = Some(end);
            return Ok(self.observed);
        }
        Err(fault(format!("the run passed {STEP_BOUND} steps")))
    }
}

/// The scripted host: answers read-bound performs and cancellation polls
/// from the case's host script, at the cursor.
struct Script<'r> {
    answers: &'r [HostAnswer],
    cursor: &'r mut usize,
    prints: &'r mut Vec<Datum>,
    fault: Option<String>,
    operations: &'r BTreeMap<lash_kernel_value::OperationUse, String>,
}

impl Host for Script<'_> {
    fn perform(
        &mut self,
        effect: &lash_kernel_value::OperationUse,
        args: &[Datum],
        _site: &lash_kernel_value::sites::PositionLabel,
        _origin: &lash_kernel_value::sites::Site,
    ) -> Result<Result<Datum, lash_kernel_doc::ErrorDatum>, HostFault> {
        match self.answers.get(*self.cursor) {
            Some(HostAnswer::Perform(perform))
                if self
                    .operations
                    .get(effect)
                    .is_some_and(|name| name == perform.effect.as_str())
                    && perform.args == args =>
            {
                *self.cursor += 1;
                perform
                    .answer
                    .clone()
                    .ok_or_else(|| HostFault::new("the scripted host cannot answer the read"))
            }
            answer => {
                self.fault.get_or_insert_with(|| {
                    format!(
                        "host requested a matching read-bound perform, next answer was {answer:?}"
                    )
                });
                Err(HostFault::new("unscripted read-bound perform"))
            }
        }
    }

    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        match self.answers.get(*self.cursor) {
            Some(HostAnswer::Cancel(cancel)) => {
                *self.cursor += 1;
                *cancel
            }
            _ => false,
        }
    }
}

/// A case's start, as the corpus harness builds it.
fn begin(
    library: &PreparedLibrary,
    case: &Case,
    pages: &Pages,
) -> Result<Option<(Program, KernelMachine)>, HarnessError> {
    let Some(program) = crate::machine::case_program(library, case) else {
        return Ok(None);
    };
    let env = &case.environment;
    let reads = |name: &str| env.reads.iter().any(|read| read.as_str() == name);
    if env.reads.iter().any(|name| {
        !program
            .operation_imports()
            .any(|(_, import)| name.as_str() == import.name.to_string())
    }) {
        return Ok(None);
    }
    let start = CellStart {
        target: env.entry.clone().map_or(Target::Main, Target::Entry),
        args: env.args.clone(),
        session: None,
        owner: crate::pages::OWNER,
        modes: program
            .operation_imports()
            .filter(|(_, import)| reads(&import.name.to_string()))
            .map(|(use_, _)| (use_, lash_kernel_value::OperationMode::Read))
            .collect(),
        host: Vec::new(),
    };
    match KernelMachine::start(
        program.clone(),
        env.bounds.into(),
        start,
        Box::new(pages.clone()),
    ) {
        Ok(machine) => Ok(Some((program, machine))),
        Err(StartError::NotAdmitted(_) | StartError::UndeclaredRead { .. }) => Ok(None),
        Err(error) => Err(fault(error)),
    }
}

fn refused() -> Observed {
    Observed {
        end: Some(ExpectedEnd::Refused),
        ..Observed::default()
    }
}

/// Checks the corpus's literal expectations against what a run observed.
fn check(id: &str, case: &Case, observed: &Observed) -> Result<(), HarnessError> {
    let actual = Observations {
        prints: observed.prints.clone(),
        end: observed
            .end
            .clone()
            .ok_or_else(|| fault("a run without an end"))?,
        trace: observed.trace.clone(),
        charged: observed.charged,
        parks: observed
            .steps
            .iter()
            .filter(|step| step.phase == Phase::Park)
            .count(),
        returns: None,
    };
    case.expected
        .check(&actual)
        .map_err(|error| fault(format!("{id}: {error}")))
}

/// One uninterrupted run of `case`, pausing at every legal pause.
fn sweep(
    library: &PreparedLibrary,
    id: &str,
    case: &Case,
    pause: &mut dyn FnMut(&mut Running, Phase, &Pages) -> Result<(), HarnessError>,
) -> Result<Observed, HarnessError> {
    let pages = Pages::default();
    let Some((program, machine)) = begin(library, case, &pages)? else {
        let observed = refused();
        check(id, case, &observed)?;
        return Ok(observed);
    };
    let mut running = Running::new(machine, &program, Cursor::default(), Observed::default());
    running.record(Phase::Start);
    pause(&mut running, Phase::Start, &pages)?;
    let observed = running
        .drive(case, &mut |running, phase| pause(running, phase, &pages))
        .map_err(|error| fault(format!("{id}: {error}")))?;
    check(id, case, &observed)?;
    Ok(observed)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    cases: Vec<ManifestCase>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestCase {
    id: String,
    name: String,
    pauses: Vec<Pause>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pause {
    id: String,
    kind: Phase,
    image: String,
}

/// A saved pause: its sealed root and every page it reaches, its bounds,
/// script cursor and the observations up to it.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    version: u32,
    case: String,
    pause: String,
    /// The pause's phase: a park image takes its delivery batch first.
    phase: Phase,
    root: Page,
    pages: Vec<Page>,
    bounds: CaseBounds,
    cursor: Cursor,
    prefix: Observed,
}

/// A page's identity and original bytes, hex-encoded.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    id: String,
    grant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    children: Vec<Page>,
}

impl Page {
    fn of_ref(page: &PageRef) -> Self {
        Self {
            id: hex(&page.id.0),
            grant: hex(&page.grant.0),
            kind: None,
            tokens: None,
            children: Vec::new(),
        }
    }

    fn of_answer(answer: &PageAnswer) -> Self {
        Self {
            kind: Some(answer.kind.0),
            tokens: Some(hex(&answer.tokens)),
            children: answer.children.iter().map(Self::of_ref).collect(),
            ..Self::of_ref(&answer.page)
        }
    }

    fn to_ref(&self) -> Result<PageRef, HarnessError> {
        Ok(PageRef {
            id: PageId(unhex(&self.id)?),
            grant: PageGrant(unhex(&self.grant)?),
        })
    }

    fn to_answer(&self) -> Result<PageAnswer, HarnessError> {
        Ok(PageAnswer {
            page: self.to_ref()?,
            kind: PageKind(
                self.kind
                    .ok_or_else(|| fault("a stored page without a kind"))?,
            ),
            tokens: unhex_vec(self.tokens.as_deref().unwrap_or_default())?.into(),
            children: self
                .children
                .iter()
                .map(Self::to_ref)
                .collect::<Result<_, _>>()?,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex_vec(text: &str) -> Result<Vec<u8>, HarnessError> {
    if !text.len().is_multiple_of(2) {
        return Err(fault("odd hex length"));
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).map_err(fault))
        .collect()
}

fn unhex<const N: usize>(text: &str) -> Result<[u8; N], HarnessError> {
    unhex_vec(text)?
        .try_into()
        .map_err(|_| fault("a page identity of the wrong width"))
}

/// The pages `root` reaches, root first, each once.
fn reachable(pages: &Pages, root: &PageRef) -> Result<Vec<PageAnswer>, HarnessError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut pending = vec![*root];
    let mut found = Vec::new();
    while let Some(page) = pending.pop() {
        if !seen.insert(page.id) {
            continue;
        }
        let answer = pages
            .0
            .borrow()
            .get(&page.id)
            .cloned()
            .ok_or_else(|| fault(format!("sealed page {} is missing", hex(&page.id.0))))?;
        pending.extend(answer.children.iter().rev().copied());
        found.push(answer);
    }
    Ok(found)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the runner is a command-line host: it reads the corpus and images it is given and writes its output files"
)]
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), HarnessError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(fault)?;
    }
    let text = serde_json::to_string(value).map_err(fault)?;
    std::fs::write(path, text + "\n").map_err(fault)
}

#[expect(
    clippy::disallowed_methods,
    reason = "the runner is a command-line host: it reads the corpus and images it is given and writes its output files"
)]
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, HarnessError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| fault(format!("{}: {error}", path.display())))?;
    serde_json::from_str(&text).map_err(|error| fault(format!("{}: {error}", path.display())))
}

fn export(
    library: &PreparedLibrary,
    corpus: &[(String, Case)],
    out: &Path,
) -> Result<(), HarnessError> {
    let mut manifest = Manifest {
        version: VERSION,
        cases: Vec::new(),
    };
    let mut images = 0usize;
    for (id, case) in corpus {
        let mut pauses = Vec::new();
        sweep(library, id, case, &mut |running, phase, pages| {
            let sealed = running
                .machine
                .seal(crate::pages::BUDGET)
                .map_err(|error| fault(format!("seal: {error}")))?
                .ok_or_else(|| fault(format!("{id}: unbounded seal incomplete")))?;
            let mut reached = reachable(pages, &sealed.page)?;
            let root = Page::of_answer(&reached.remove(0));
            let pause = pauses.len().to_string();
            let image = format!("images/{images}.json");
            images += 1;
            write_json(
                &out.join(&image),
                &Image {
                    version: VERSION,
                    case: id.clone(),
                    pause: pause.clone(),
                    phase,
                    root,
                    pages: reached.iter().map(Page::of_answer).collect(),
                    bounds: case.environment.bounds,
                    cursor: running.cursor.clone(),
                    prefix: running.prefix(),
                },
            )?;
            pauses.push(Pause {
                id: pause,
                kind: phase,
                image,
            });
            Ok(())
        })?;
        manifest.cases.push(ManifestCase {
            id: id.clone(),
            name: case.name.clone(),
            pauses,
        });
    }
    if images == 0 {
        return Err(fault("no corpus case reached a legal pause"));
    }
    write_json(&out.join("manifest.json"), &manifest)
}

#[derive(Serialize)]
struct Runs<R> {
    version: u32,
    runs: Vec<R>,
}

#[derive(Serialize)]
struct Uninterrupted {
    case: String,
    observation: Observation,
}

#[derive(Serialize)]
struct Resumed {
    case: String,
    pause: String,
    observation: Observation,
}

/// An observation in the interface's field set.
#[derive(Serialize)]
struct Observation {
    prints: Vec<Datum>,
    end: ExpectedEnd,
    trace: Vec<Trace>,
    charged: Option<u64>,
    charge_tape: Vec<Charge>,
    steps: Vec<StepRecord>,
}

impl Observation {
    fn of(observed: Observed) -> Result<Self, HarnessError> {
        Ok(Self {
            end: observed.end.ok_or_else(|| fault("a run without an end"))?,
            prints: observed.prints,
            trace: observed.trace,
            charged: observed.charged,
            charge_tape: observed.charge_tape,
            steps: observed.steps,
        })
    }
}

fn uninterrupted(
    library: &PreparedLibrary,
    corpus: &[(String, Case)],
    out: &Path,
) -> Result<(), HarnessError> {
    let mut runs = Vec::new();
    let mut failures = Vec::new();
    for (id, case) in corpus {
        let observed = sweep(library, id, case, &mut |_, _, _| Ok(())).and_then(Observation::of);
        match observed {
            Ok(observation) => runs.push(Uninterrupted {
                case: id.clone(),
                observation,
            }),
            Err(error) => failures.push(error.0),
        }
    }
    write_json(
        out,
        &Runs {
            version: VERSION,
            runs,
        },
    )?;
    failed("uninterrupted runs", &failures)
}

/// Fails with every one of `failures`, if there are any.
fn failed(what: &str, failures: &[String]) -> Result<(), HarnessError> {
    if failures.is_empty() {
        return Ok(());
    }
    Err(fault(format!(
        "{} {what} failed:\n{}",
        failures.len(),
        failures.join("\n")
    )))
}

/// Every pause of the export, with its case.
fn exported<'c>(
    corpus: &'c [(String, Case)],
    imports: &Path,
) -> Result<Vec<(&'c Case, String, PathBuf)>, HarnessError> {
    let manifest: Manifest = read_json(&imports.join("manifest.json"))?;
    if manifest.version != VERSION {
        return Err(fault(format!(
            "export manifest version {}, not {VERSION}",
            manifest.version
        )));
    }
    let cases: BTreeMap<&str, &Case> = corpus
        .iter()
        .map(|(id, case)| (id.as_str(), case))
        .collect();
    let mut pauses = Vec::new();
    for entry in manifest.cases {
        let case = cases
            .get(entry.id.as_str())
            .ok_or_else(|| fault(format!("export names unknown case {}", entry.id)))?;
        for pause in entry.pauses {
            let path = imports.join(&pause.image);
            if !path.starts_with(imports) || pause.image.contains("..") {
                return Err(fault(format!(
                    "image path {} leaves the export",
                    pause.image
                )));
            }
            pauses.push((*case, pause.id, path));
        }
    }
    Ok(pauses)
}

/// Opens `image` in a fresh machine over a fresh program and page source.
fn open(
    library: &PreparedLibrary,
    case: &Case,
    image: &Image,
    stated: Option<StatedFormat>,
) -> Result<
    (
        Program,
        Pages,
        Result<KernelMachine, lash_kernel_vm::OpenError>,
    ),
    HarnessError,
> {
    let program = crate::machine::case_program(library, case)
        .ok_or_else(|| fault(format!("{}: an exported case no longer admits", image.case)))?;
    let pages = Pages::default();
    for page in std::iter::once(&image.root).chain(&image.pages) {
        let answer = page.to_answer()?;
        pages.0.borrow_mut().insert(answer.page.id, answer);
    }
    let root = Checkpoint::of_answer(&image.root.to_answer()?).map_err(fault)?;
    let root = match stated {
        Some(format) => root.stated(format),
        None => root,
    };
    let opened = KernelMachine::open(
        program.clone(),
        image.bounds.into(),
        &root,
        Box::new(pages.clone()),
    );
    Ok((program, pages, opened))
}

fn resume(
    library: &PreparedLibrary,
    corpus: &[(String, Case)],
    imports: &Path,
    out: &Path,
) -> Result<(), HarnessError> {
    let mut runs = Vec::new();
    let mut failures = Vec::new();
    for (case, pause, path) in exported(corpus, imports)? {
        let image: Image = read_json(&path)?;
        let id = image.case.clone();
        match continue_image(library, case, image) {
            Ok(observation) => runs.push(Resumed {
                case: id,
                pause,
                observation,
            }),
            Err(error) => failures.push(format!("{id} pause {pause}: {}", error.0)),
        }
    }
    write_json(
        out,
        &Runs {
            version: VERSION,
            runs,
        },
    )?;
    failed("imported continuations", &failures)
}

/// Opens `image` and runs it to its end under its saved script cursor.
fn continue_image(
    library: &PreparedLibrary,
    case: &Case,
    image: Image,
) -> Result<Observation, HarnessError> {
    let (program, _pages, opened) = open(library, case, &image, None)?;
    let machine = opened.map_err(|error| fault(format!("open: {error}")))?;
    let mut running = Running::new(machine, &program, image.cursor, image.prefix);
    if image.phase == Phase::Park {
        running.deliver(case, &mut |_, _| Ok(()))?;
    }
    let observed = running.drive(case, &mut |_, _| Ok(()))?;
    check(&image.case, case, &observed)?;
    Observation::of(observed)
}

#[derive(Serialize)]
struct Refusal {
    case: String,
    pause: String,
    opened: bool,
    refusal: Option<FormatRefusal>,
    mutations: Vec<String>,
    executed_steps: u64,
}

/// A typed format refusal (`r57/l7-runner-interface.md`).
#[derive(Serialize)]
struct FormatRefusal {
    error: &'static str,
    kind: &'static str,
    surface: String,
    expected: u64,
    observed: u64,
}

/// The typed format refusal `error` carries; any other open error is not one.
fn format_refusal(error: &lash_kernel_vm::OpenError) -> Option<FormatRefusal> {
    match *error {
        lash_kernel_vm::OpenError::Format {
            surface,
            expected,
            observed,
        } => Some(FormatRefusal {
            error: "OpenError",
            kind: "format",
            surface: surface.to_owned(),
            expected: u64::from(expected),
            observed: u64::from(observed),
        }),
        _ => None,
    }
}

fn refuse(
    library: &PreparedLibrary,
    corpus: &[(String, Case)],
    imports: &Path,
    out: &Path,
    stated: StatedFormat,
) -> Result<(), HarnessError> {
    let mut runs = Vec::new();
    let mut failures = Vec::new();
    for (case, pause, path) in exported(corpus, imports)? {
        let image: Image = read_json(&path)?;
        let (_, pages, opened) = open(library, case, &image, Some(stated))?;
        let mutations = pages.1.get();
        let record = Refusal {
            case: image.case.clone(),
            pause: pause.clone(),
            opened: opened.is_ok(),
            refusal: opened.as_ref().err().and_then(format_refusal),
            mutations: (0..mutations).map(|n| format!("page write {n}")).collect(),
            executed_steps: 0,
        };
        if record.refusal.is_none() || !record.mutations.is_empty() {
            failures.push(format!(
                "{} pause {pause}: {}",
                image.case,
                match &opened {
                    Ok(_) => "the image opened".to_owned(),
                    Err(error) => format!("not a typed format refusal: {error}"),
                }
            ));
        }
        runs.push(record);
    }
    let total = runs.len();
    write_json(
        out,
        &Runs {
            version: VERSION,
            runs,
        },
    )?;
    match failures.first() {
        None => Ok(()),
        Some(first) => Err(fault(format!(
            "{} of {total} images did not refuse with a typed format error; first: {first}",
            failures.len()
        ))),
    }
}

/// A page store shared by a run's machines, content-addressed as the
/// corpus harness's, and the number of pages written to it.
#[derive(Clone, Default)]
struct Pages(
    std::rc::Rc<std::cell::RefCell<BTreeMap<PageId, PageAnswer>>>,
    std::rc::Rc<std::cell::Cell<usize>>,
);

impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                self.0
                    .borrow()
                    .get(&page.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }

    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.1.set(self.1.get() + 1);
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}
