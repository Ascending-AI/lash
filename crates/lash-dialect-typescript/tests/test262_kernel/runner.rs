//! Runs vendored Test262 programs through the kernel: lowered by this
//! crate, then run on a kernel machine.
//!
//! One test has one observation: it passed, the front end refused it, it
//! diverged, or it lowered and no machine is installed to run it. The
//! report counts observations by directory beside the class main's record
//! holds for the same test, so a lane sees its family's distance to the
//! record.

// This file is test tooling; ambient filesystem access is sanctioned here.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use lash_dialect_typescript::{DiagnosticCode, define_helpers, provisional};
use lash_kernel_dialect::{Environment, Lowered, NamedLibrary};
use lash_kernel_doc::{Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, Machine, Program, RunError, Start, Step, Target,
};

use super::ingest::{data_path, harness_shim, source_for, source_without_unshimmed};
use super::metadata::{self, Metadata, Phase, TestFlag};

/// What the front end raises for an ECMAScript early error. A parse-negative
/// test expects one of these.
const EARLY_ERROR_CODES: [DiagnosticCode; 5] = [
    DiagnosticCode::SyntaxError,
    DiagnosticCode::DuplicateBinding,
    DiagnosticCode::ReturnOutsideFunction,
    DiagnosticCode::LoopControlOutsideLoop,
    DiagnosticCode::RegexInvalid,
];

/// The bounds one test runs under. A program that passes one diverges.
const BOUNDS: Bounds = Bounds {
    charge: 4_000_000_000,
    memory: 1 << 30,
    call_depth: 1_000,
    live_tasks: 1_024,
    requests_per_park: 1_024,
    join_members: 1_024,
};

/// What one test did on the kernel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observed {
    Pass,
    /// The front end refused the program, with this diagnostic code.
    Refused(String),
    /// The program ran and did not do what the test expects.
    Diverged(String),
    /// The test needs a harness include that has no rendering.
    Harness(String),
    /// The program lowered; no machine is installed to run it.
    NotRun,
}

impl Observed {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Refused(_) => "refused",
            Self::Diverged(_) => "fail",
            Self::Harness(_) => "harness",
            Self::NotRun => "not-run",
        }
    }
}

/// What runs a lowered program. `None` until a kernel machine exists; the
/// lane that lands one returns `Some(Box::new(OnMachine::<TheMachine>::new(registry)))`
/// here, with the kernel library's natives in the registry.
pub(crate) fn executor() -> Option<Box<dyn Executor>> {
    None
}

pub(crate) trait Executor: Sync {
    fn run(&self, lowered: &Lowered) -> Result<End, String>;
}

/// Runs a document on any implementation of the kernel's machine interface.
pub(crate) struct OnMachine<M> {
    registry: Arc<FunctionRegistry>,
    machine: std::marker::PhantomData<fn() -> M>,
}

impl<M: Machine> OnMachine<M> {
    #[expect(dead_code, reason = "constructed by the lane that installs a machine")]
    pub(crate) fn new(registry: Arc<FunctionRegistry>) -> Self {
        Self {
            registry,
            machine: std::marker::PhantomData,
        }
    }
}

/// The host of a conformance run: it answers no effect, and its clock and
/// random source are constants.
#[derive(Default)]
struct TestHost {
    prints: Vec<Datum>,
}

impl Host for TestHost {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, _request: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".to_string(),
            message: "the Test262 host has no projection".to_string(),
            data: Datum::Null,
        })
    }

    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

impl<M: Machine> Executor for OnMachine<M> {
    fn run(&self, lowered: &Lowered) -> Result<End, String> {
        let program = Program {
            document: Arc::new(lowered.document.clone()),
            registry: Arc::clone(&self.registry),
        };
        let start = Start {
            target: Target::Main,
            args: Vec::new(),
            bindings: Bindings::default(),
        };
        let mut machine = M::start(program, BOUNDS, start).map_err(|error| error.to_string())?;
        let mut host = TestHost::default();
        loop {
            match machine
                .run(&mut host, u64::MAX)
                .map_err(|error| error.to_string())?
            {
                Step::Ended(end) => return Ok(end),
                Step::Slice => {}
                Step::Parked(park) => {
                    return Err(format!(
                        "the program waits on {} request(s); the Test262 host answers none",
                        park.requests.len()
                    ));
                }
            }
        }
    }
}

/// The stand-in kernel library with the dialect's helpers, until the
/// kernel library crate replaces the stand-in.
fn library() -> &'static NamedLibrary {
    static LIBRARY: std::sync::OnceLock<NamedLibrary> = std::sync::OnceLock::new();
    LIBRARY.get_or_init(|| {
        let mut library = provisional::kernel_library();
        if let Err(error) = define_helpers(&mut library) {
            panic!("{error}");
        }
        library
    })
}

/// The JavaScript error class an uncaught value belongs to: the kernel's
/// own kinds by the name JavaScript gives them, any other kind as written,
/// and a thrown object by its `name`.
fn thrown_name(error: &ErrorDatum) -> String {
    if let Datum::Record(fields) = &error.data
        && let Some((_, Datum::Text(name))) = fields.iter().find(|(field, _)| field == "name")
    {
        return name.clone();
    }
    match error.kind.as_str() {
        "type_error" | "key_missing" | "invalid_key" | "not_data" | "cycle" => "TypeError".into(),
        "index_out_of_range" | "number_range" => "RangeError".into(),
        "unbound_variable" => "ReferenceError".into(),
        other => other.to_string(),
    }
}

fn judge(end: End, meta: &Metadata) -> Observed {
    match (end, &meta.negative) {
        (End::Error(RunError::Uncaught(error)), Some(negative))
            if thrown_name(&error) == negative.error_type.as_str() =>
        {
            Observed::Pass
        }
        (End::Finished(finished), None) if finished.result == Datum::Bool(true) => Observed::Pass,
        (End::Finished(finished), _) => {
            Observed::Diverged(format!("the program finished with {:?}", finished.result))
        }
        (End::Error(error), _) => Observed::Diverged(error.to_string()),
        (End::Failed(reason), _) => Observed::Diverged(format!("the program failed: {reason:?}")),
        (End::Cancelled, _) => Observed::Diverged("the run was cancelled".to_string()),
    }
}

fn lower(source: &str) -> Result<Lowered, lash_dialect_typescript::Diagnostic> {
    let effects = BTreeMap::new();
    let bindings = BTreeSet::new();
    let environment = Environment {
        library: library(),
        effects: &effects,
        bindings: &bindings,
    };
    lash_dialect_typescript::lower(source, &environment)
}

/// Lowers and runs one vendored test, `test/...`.
pub(crate) fn run(relative: &str, executor: Option<&dyn Executor>) -> Observed {
    let path = data_path(relative);
    let meta = metadata::read_metadata(&path).unwrap_or_else(|error| panic!("{relative}: {error}"));
    if let Some(include) = meta
        .includes
        .iter()
        .find(|include| harness_shim(include).is_none())
    {
        // The test's own body may be refused whatever the include holds;
        // that refusal is the more exact observation.
        let body = source_without_unshimmed(&path, &meta, meta.negative.is_none());
        return match lower(&body) {
            Err(diagnostic) if diagnostic.code != DiagnosticCode::UnknownBinding => {
                Observed::Refused(diagnostic.code.as_str().to_string())
            }
            _ => Observed::Harness(include.to_string()),
        };
    }
    let parse_negative = meta
        .negative
        .as_ref()
        .is_some_and(|negative| negative.phase != Phase::Runtime);
    let is_async = meta.flags.contains(&TestFlag::Async);
    let source = source_for(&path, &meta, meta.negative.is_none() && !is_async);
    let lowered = match lower(&source) {
        Ok(lowered) => lowered,
        Err(diagnostic) if parse_negative && EARLY_ERROR_CODES.contains(&diagnostic.code) => {
            return Observed::Pass;
        }
        Err(diagnostic) => return Observed::Refused(diagnostic.code.as_str().to_string()),
    };
    if let Some(negative) = meta.negative.as_ref().filter(|_| parse_negative) {
        return Observed::Diverged(format!(
            "expected an early {} but the program lowered",
            negative.error_type.as_str()
        ));
    }
    match executor {
        None => Observed::NotRun,
        Some(executor) => match executor.run(&lowered) {
            Ok(end) => judge(end, &meta),
            Err(problem) => Observed::Diverged(problem),
        },
    }
}

/// The class main's record holds for every selected test: `pass`,
/// `refused`, `fail` or `harness`.
pub(crate) fn recorded_classes() -> BTreeMap<String, String> {
    fn tables(directory: &Path, into: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = std::fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                tables(&entry, into);
                continue;
            }
            let contents = std::fs::read_to_string(&entry)
                .unwrap_or_else(|error| panic!("read {}: {error}", entry.display()));
            for line in contents.lines() {
                if line.trim().is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut fields = line.split('\t');
                if let (Some(path), Some(class)) = (fields.next(), fields.next()) {
                    into.insert(path.to_string(), class.to_string());
                }
            }
        }
    }
    let mut classes = BTreeMap::new();
    tables(&data_path("outcomes"), &mut classes);
    classes
}

/// The stratified sample of the selection, `sample.tsv`.
pub(crate) fn sample_paths() -> Vec<String> {
    std::fs::read_to_string(data_path("sample.tsv"))
        .expect("read the Test262 sample")
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// A test's directory, two levels below `test/`: `language/expressions`,
/// `built-ins/Array`.
fn directory(path: &str) -> String {
    path.split('/')
        .skip(1)
        .take(2)
        .collect::<Vec<_>>()
        .join("/")
}

/// One line per directory: how many of its tests the kernel passed,
/// refused, failed, could not run for a missing harness include or
/// machine, and how many of them main's record marks `pass`.
pub(crate) fn report(
    observations: &[(String, Observed)],
    recorded: &BTreeMap<String, String>,
) -> String {
    #[derive(Default)]
    struct Row {
        classes: BTreeMap<&'static str, usize>,
        recorded_pass: usize,
        regressed: usize,
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for (path, observed) in observations {
        let row = rows.entry(directory(path)).or_default();
        *row.classes.entry(observed.class()).or_default() += 1;
        if recorded.get(path).is_some_and(|class| class == "pass") {
            row.recorded_pass += 1;
            if *observed != Observed::Pass {
                row.regressed += 1;
            }
        }
    }
    let mut out = String::from(
        "directory\tpass\trefused\tfail\tharness\tnot-run\trecorded-pass\tof-those-not-passing\n",
    );
    for (directory, row) in &rows {
        let count = |class: &str| row.classes.get(class).copied().unwrap_or(0);
        writeln!(
            out,
            "{directory}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            count("pass"),
            count("refused"),
            count("fail"),
            count("harness"),
            count("not-run"),
            row.recorded_pass,
            row.regressed
        )
        .expect("write to a string");
    }
    // Why the front end refused, most frequent first: the built-in or
    // construct a family is waiting on.
    let mut codes: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, observed) in observations {
        if let Observed::Refused(code) = observed {
            *codes.entry(code.as_str()).or_default() += 1;
        }
    }
    let mut codes: Vec<_> = codes.into_iter().collect();
    codes.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(right.0)));
    out.push_str("\nrefusal\tcount\n");
    for (code, count) in codes {
        writeln!(out, "{code}\t{count}").expect("write to a string");
    }
    out
}
