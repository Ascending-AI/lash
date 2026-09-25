//! The Test262 conformance runner shared by the PR sample and the full
//! selection: how one vendored test is run and which outcome class it lands
//! in, and the ratchet files that pin every outcome.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use lash_typescript::DiagnosticCode;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, RuntimeError, State, Value,
};

use super::ingest::{
    data_path, harness_bindings, harness_shim, source_for, source_without_unshimmed, test_script,
};
use super::metadata::{self, Phase, TestFlag};

/// The instruction budget one test runs under. It is deterministic, unlike a
/// deadline, so a test that exhausts it does so on every run; the slowest
/// selected test uses well under a tenth of it.
const INSTRUCTION_BUDGET: u64 = 4_000_000_000;

/// The wall-clock bound one test may take, on top of the instruction budget.
/// A worker cannot be killed once a test starts, so the bound is enforced by
/// the orchestrating thread: past it, the run reports the unfinished tests by
/// name and exits non-zero rather than letting the lane hang.
const TEST_WALL_CLOCK: Duration = Duration::from_secs(300);

/// The diagnostics that report an ECMAScript early error: what a negative
/// test of phase `parse` expects as its `SyntaxError`.
const EARLY_ERROR_CODES: [DiagnosticCode; 5] = [
    DiagnosticCode::SyntaxError,
    DiagnosticCode::RegexInvalid,
    DiagnosticCode::DuplicateBinding,
    DiagnosticCode::ReturnOutsideFunction,
    DiagnosticCode::LoopControlOutsideLoop,
];

/// The `harness` qualifier of a test that fits the dialect's cell-size limit
/// alone but not with the harness prepended to it.
pub(crate) const PROGRAM_SIZE: &str = "program-size";

/// The `harness` qualifier of a test that performs a journaled host effect
/// (`Date.now()`, `Math.random()`, a tool), which the runner's host does not
/// answer.
pub(crate) const HOST_EFFECTS: &str = "host-effects";

/// The `harness` qualifier of a selected test whose full run exceeds the CI
/// lane's cost bound. `harness-cost.tsv` names each, with the ticket that owns
/// making it affordable; the runner records the qualifier without executing
/// the test.
pub(crate) const INSTRUCTION_COST: &str = "instruction-cost";

/// The `harness` qualifier of a test whose own declarations collide with a
/// name a shim binds. Upstream runs each harness file as its own Script, so a
/// test may redeclare what it bound (upstream's `assert` is a var-scoped
/// function). The runner concatenates shim and test into one cell, where the
/// shim's lexical bindings meet the test's `var`, `let`, `const` or `function`
/// of the same name as an ECMA early error the test alone does not carry.
pub(crate) const BINDING_COLLISION: &str = "binding-collision";

const UNEXPECTED_ABILITY: &str = "the Test262 host answers no host effect";

/// The one outcome class a selected test has, as the record's shards record it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Outcome {
    /// It runs and meets the specification.
    Pass,
    /// The dialect refuses it with this real `TS_*` diagnostic, statically or
    /// as a registered shape-dependent refusal at run time.
    Refused(String),
    /// It runs and diverges from the specification; the qualifier names the
    /// ticket or registered deviation that owns the divergence.
    Fail(String),
    /// It needs this harness include, which has no in-dialect rendering;
    /// `harness-shim/unshimmable.tsv` names the missing capability.
    Harness(String),
}

impl Outcome {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Refused(_) => "refused",
            Self::Fail(_) => "fail",
            Self::Harness(_) => "harness",
        }
    }

    pub(crate) fn detail(&self) -> &str {
        match self {
            Self::Pass => "-",
            Self::Refused(detail) | Self::Fail(detail) | Self::Harness(detail) => detail,
        }
    }

    fn parse(class: &str, detail: &str) -> Result<Self, String> {
        match (class, detail) {
            ("pass", "-") => Ok(Self::Pass),
            ("refused", code) => Ok(Self::Refused(code.to_owned())),
            ("fail", owner) => Ok(Self::Fail(owner.to_owned())),
            ("harness", include) => Ok(Self::Harness(include.to_owned())),
            _ => Err(format!("unknown outcome `{class}\t{detail}`")),
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}", self.class(), self.detail())
    }
}

/// What one run of a test observed. A divergence carries its evidence, which
/// the ratchet does not pin: a recorded `fail` matches any divergence.
#[derive(Clone, Debug)]
pub(crate) enum Observed {
    Pass,
    /// The refusing diagnostic, and its message as evidence.
    Refused(String, String),
    Diverged(String),
    Harness(String),
}

impl Observed {
    pub(crate) fn matches(&self, recorded: &Outcome) -> bool {
        match (self, recorded) {
            (Self::Pass, Outcome::Pass) | (Self::Diverged(_), Outcome::Fail(_)) => true,
            (Self::Refused(code, _), Outcome::Refused(recorded))
            | (Self::Harness(code), Outcome::Harness(recorded)) => code == recorded,
            _ => false,
        }
    }

    /// The outcome to record for this observation when nothing is recorded,
    /// or when the record no longer matches: a divergence keeps the owner
    /// already recorded for the test, and is `UNTRIAGED` otherwise, which the
    /// data checks refuse.
    pub(crate) fn outcome(&self, recorded: Option<&Outcome>) -> Outcome {
        match self {
            Self::Pass => Outcome::Pass,
            Self::Refused(code, _) => Outcome::Refused(code.clone()),
            Self::Harness(include) => Outcome::Harness(include.clone()),
            Self::Diverged(_) => match recorded {
                Some(Outcome::Fail(owner)) => Outcome::Fail(owner.clone()),
                _ => Outcome::Fail("UNTRIAGED".to_owned()),
            },
        }
    }
}

impl fmt::Display for Observed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => formatter.write_str("pass"),
            Self::Refused(code, evidence) => write!(formatter, "refused {code} ({evidence})"),
            Self::Diverged(evidence) => write!(formatter, "fail ({evidence})"),
            Self::Harness(include) => write!(formatter, "harness {include}"),
        }
    }
}

#[derive(Default)]
pub(crate) struct Host {
    pub(crate) prints: Mutex<Vec<Value>>,
}

/// The Test262 host's answer to a journaled host read: a fixed clock and a
/// fixed draw, since a conformance test may call either but never depends on
/// its value.
fn host_read(call: &lashlang::ResourceOperation) -> Result<Value, ExecutionHostError> {
    match call.operation.as_str() {
        lashlang::LANGUAGE_RUNTIME_NOW_OPERATION => Ok(Value::Number(1_700_000_000_000.0)),
        lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION => Ok(Value::Number(0.5)),
        _ => Err(ExecutionHostError::new(UNEXPECTED_ABILITY)),
    }
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(call) => host_read(&call).map(AbilityResult::Value),
            AbilityOp::ResourceOperationBatch(batch) => {
                let answers = batch
                    .leaves
                    .iter()
                    .filter_map(lashlang::ResourceOperationBatchLeaf::operation)
                    .map(|call| host_read(call).map(lashlang::ResourceOperationResult::Value))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(AbilityResult::ResourceOperationBatch(
                    batch.answer_in_leaf_order(answers),
                ))
            }
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(value) => {
                self.prints.lock().expect("print journal").push(value);
                Ok(AbilityResult::Value(Value::Null))
            }
            _ => Err(ExecutionHostError::new(UNEXPECTED_ABILITY)),
        }
    }
}

pub(crate) fn data_lines(relative: &str, columns: usize) -> Vec<Vec<String>> {
    let path = data_path(relative);
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|(line_index, line)| {
            let fields = line.split('\t').map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                fields.len(),
                columns,
                "{}:{} must have {columns} tab-separated columns",
                path.display(),
                line_index + 1
            );
            fields
        })
        .collect()
}

pub(crate) fn diagnostic_names() -> BTreeSet<&'static str> {
    DiagnosticCode::ALL
        .iter()
        .map(|code| code.as_str())
        .collect()
}

/// Every code that names a refusal: the static diagnostics, plus the runtime
/// refusal codes the crate README's deviation register names (a
/// shape-dependent refusal reports its code at the head of its reason). A
/// `TS_*` string the register does not name is not a ruling, so an error
/// carrying one is a divergence.
pub(crate) fn refusal_codes() -> BTreeSet<String> {
    let readme = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("read the crate README");
    let mut codes = diagnostic_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut rest = readme.as_str();
    while let Some(start) = rest.find("TS_") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|character: char| {
                !(character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_')
            })
            .unwrap_or(candidate.len());
        if end > 3 {
            codes.insert(candidate[..end].to_owned());
        }
        rest = &candidate[end.max(3)..];
    }
    codes
}

/// Every `outcomes/<shard>.tsv`, sorted by shard, as `(shard, rows)` (FIG-3727):
/// one file per top-level test directory, so two lanes that touch different
/// directories never share a file.
fn outcome_shards() -> Vec<(String, Vec<Vec<String>>)> {
    let directory = data_path("outcomes");
    let mut names = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| {
            entry
                .expect("an outcomes entry")
                .file_name()
                .into_string()
                .expect("UTF-8 outcomes name")
        })
        .filter(|name| name.ends_with(".tsv"))
        .collect::<Vec<_>>();
    names.sort();
    assert!(!names.is_empty(), "{} is empty", directory.display());
    names
        .into_iter()
        .map(|name| {
            (
                name.trim_end_matches(".tsv").to_owned(),
                data_lines(&format!("outcomes/{name}"), 3),
            )
        })
        .collect()
}

/// The recorded outcome of every selected test, the union of every
/// `outcomes/<shard>.tsv`. A row lives in the file its top-level directory
/// names, sorted with the rest, so merges meet only on a real overlap.
pub(crate) fn recorded_outcomes() -> BTreeMap<String, Outcome> {
    let mut outcomes = BTreeMap::new();
    for (shard, rows) in outcome_shards() {
        let mut last = String::new();
        for fields in &rows {
            let path = fields[0].as_str();
            assert_eq!(
                path.split('/').nth(1).unwrap_or_default(),
                shard,
                "{path}: outcomes/{shard}.tsv holds a row outside its directory"
            );
            assert!(
                path > last.as_str(),
                "outcomes/{shard}.tsv rows are unsorted: {last} then {path}"
            );
            last = path.to_owned();
            let outcome = Outcome::parse(&fields[1], &fields[2])
                .unwrap_or_else(|error| panic!("outcomes/{shard}.tsv {path}: {error}"));
            assert!(
                outcomes.insert(path.to_owned(), outcome).is_none(),
                "{path} has an outcome in two shards"
            );
        }
    }
    outcomes
}

/// Every vendored test path, `test/...`, sorted: the selection.
pub(crate) fn vendored_tests() -> BTreeSet<String> {
    fn walk(directory: &std::path::Path, relative: &str, into: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(directory).expect("read vendored Test262 directory") {
            let entry = entry.expect("read vendored Test262 entry");
            let name = entry.file_name().into_string().expect("UTF-8 Test262 path");
            let child = format!("{relative}/{name}");
            if entry.file_type().expect("entry type").is_dir() {
                walk(&entry.path(), &child, into);
            } else {
                into.insert(child);
            }
        }
    }
    let mut tests = BTreeSet::new();
    walk(&data_path("test"), "test", &mut tests);
    tests
}

/// The PR lane's stratified sample, from `sample.tsv`.
pub(crate) fn sample_paths() -> Vec<String> {
    data_lines("sample.tsv", 1)
        .into_iter()
        .map(|mut fields| fields.remove(0))
        .collect()
}

/// `LASH_QUICK` (AGENTS.md): the opt-in iteration knob for the heavy lanes.
/// Set -- any value but `0` -- `quick_selection` narrows the full run to a
/// deterministic subset. CI never sets it; the full selection stays the
/// default and the release gate.
fn quick_requested() -> bool {
    std::env::var("LASH_QUICK").is_ok_and(|value| !value.is_empty() && value != "0")
}

/// FNV-1a over the path: the fixed hash the sample order is written against.
/// `DefaultHasher`'s seed is unspecified, so the subset's order key is
/// written out here.
fn fnv1a64(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The `LASH_QUICK` subset of `paths`: each stratum (the two directory levels
/// under `test/`, as `sync.mjs` stratifies) contributes `ceil(count / 10)`
/// paths in FNV-1a order, and every `includes` entry adds tests -- a shard
/// name keeps that shard's tests whole, a `test/...` entry adds itself, `*`
/// keeps everything. `scripts/dev-test.py` derives `includes` from the diff.
pub(crate) fn quick_subset(paths: &[String], includes: &BTreeSet<String>) -> BTreeSet<String> {
    if includes.contains("*") {
        return paths.iter().cloned().collect();
    }
    let mut strata: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for path in paths {
        let stratum = path
            .split('/')
            .skip(1)
            .take(2)
            .collect::<Vec<_>>()
            .join("/");
        strata.entry(stratum).or_default().push(path);
    }
    let mut subset = BTreeSet::new();
    for members in strata.values() {
        let mut ordered = members
            .iter()
            .map(|path| (fnv1a64(path), *path))
            .collect::<Vec<_>>();
        ordered.sort_unstable();
        subset.extend(
            ordered
                .into_iter()
                .take(members.len().div_ceil(10))
                .map(|(_, path)| path.clone()),
        );
    }
    for include in includes {
        if include.starts_with("test/") {
            if paths.iter().any(|path| path == include) {
                subset.insert(include.clone());
            }
        } else {
            subset.extend(
                paths
                    .iter()
                    .filter(|path| path.split('/').nth(1) == Some(include.as_str()))
                    .cloned(),
            );
        }
    }
    subset
}

/// The paths the full selection runs: all of them, or `quick_subset`'s when
/// `LASH_QUICK` is set. `LASH_TEST262_QUICK_INCLUDE` is the comma-separated
/// include list `quick_subset` resolves.
pub(crate) fn quick_selection(paths: &[String]) -> Option<Vec<String>> {
    if !quick_requested() {
        return None;
    }
    let includes = std::env::var("LASH_TEST262_QUICK_INCLUDE")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    Some(quick_subset(paths, &includes).into_iter().collect())
}

/// The harness includes with no in-dialect rendering, each with the dialect
/// capability its rendering would need.
pub(crate) fn unshimmable_includes() -> BTreeMap<String, String> {
    data_lines("harness-shim/unshimmable.tsv", 2)
        .into_iter()
        .map(|fields| (fields[0].clone(), fields[1].clone()))
        .collect()
}

/// The cost register, `harness-cost.tsv`: each selected test whose full run
/// exceeds the CI cost bound, with the reason and the ticket that owns making
/// it affordable. The register only shrinks: a test leaves it by running
/// inside the bound again, not by being edited out.
pub(crate) fn cost_register() -> BTreeMap<String, (String, String)> {
    data_lines("harness-cost.tsv", 3)
        .into_iter()
        .map(|fields| (fields[0].clone(), (fields[1].clone(), fields[2].clone())))
        .collect()
}

/// The first real `TS_*` diagnostic `text` names, if any: a runtime refusal
/// reports its code at the head of its reason, and one caught and rethrown
/// inside a message still names it.
fn named_diagnostic(text: &str, names: &BTreeSet<String>) -> Option<String> {
    let mut rest = text;
    while let Some(start) = rest.find("TS_") {
        let candidate = &rest[start..];
        let end = candidate
            .find(|character: char| {
                !(character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_')
            })
            .unwrap_or(candidate.len());
        let code = &candidate[..end];
        if names.contains(code) {
            return Some(code.to_owned());
        }
        rest = &candidate[end.max(3)..];
    }
    None
}

/// Why a program was not admitted: the diagnostic's code and its message.
pub(crate) struct Rejection {
    pub(crate) code: DiagnosticCode,
    message: String,
}

impl fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Admits `source` the way a cell is admitted: lowered, then linked against
/// a host environment (where link-time refusals such as the closed-shape
/// field guard fire), then compiled from the linked artifact.
pub(crate) fn admit(source: &str) -> Result<lashlang::CompiledProgram, Rejection> {
    static ENVIRONMENT: OnceLock<lashlang::LashlangHostEnvironment> = OnceLock::new();
    let environment = ENVIRONMENT.get_or_init(|| {
        let mut environment = lashlang::testing::harness::test_environment();
        // The journaled reads behind `Date.now()` and `Math.random()`, bound
        // as the production host binds them.
        for operation in [
            lashlang::LANGUAGE_RUNTIME_NOW_OPERATION,
            lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION,
        ] {
            environment
                .resources
                .add_module_operation(
                    [lashlang::LANGUAGE_RUNTIME_MODULE_PATH],
                    lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE,
                    operation,
                    operation,
                    lashlang::TypeExpr::Any,
                    lashlang::TypeExpr::Any,
                )
                .expect("the runtime operations are unique");
        }
        environment
    });
    let linked = lash_typescript::link(source, environment).map_err(|diagnostic| Rejection {
        code: diagnostic.code,
        message: diagnostic.to_string(),
    })?;
    lashlang::compile(
        &linked.artifact,
        lashlang::Entry::Main,
        Some(linked.spans()),
    )
    .map_err(|error| Rejection {
        code: DiagnosticCode::InvalidAst,
        message: format!("{}: {error}", DiagnosticCode::InvalidAst.as_str()),
    })
}

/// The `name` of an uncaught thrown value: an ECMA error object or the
/// harness's Test262Error record.
fn thrown_name(error: &RuntimeError) -> Option<String> {
    let RuntimeError::UncaughtException { value } = error else {
        return None;
    };
    match value.as_record()?.get("name")? {
        Value::String(name) => Some(name.to_string()),
        _ => None,
    }
}

/// Whether `error` is a binding collision the harness causes, not the test:
/// the duplicated name must be one a shim binds (`assert`, `Test262Error`,
/// `verifyProperty`, …), and the test's own script must not duplicate it on
/// its own — a test that redeclares the same name twice is invalid whether
/// or not the harness is in the cell.
fn collides_with_harness(path: &Path, meta: &metadata::Metadata, error: &Rejection) -> bool {
    let message = error.to_string();
    match error.code {
        // The parser's duplicate-declaration error and the lowerer's both
        // name the colliding binding.
        DiagnosticCode::DuplicateBinding => {}
        DiagnosticCode::SyntaxError if message.contains("already been declared") => {}
        _ => return false,
    }
    let Some(name) = message.split('`').nth(1) else {
        return false;
    };
    if !harness_bindings(meta).contains(name) {
        return false;
    }
    // The test's own script must be clean of the same collision — a body
    // that already duplicates the name, or has a syntax error of its own, is
    // invalid whether or not the harness is in the cell.
    !matches!(
        admit(&test_script(path, meta, false)),
        Err(alone)
            if matches!(
                alone.code,
                DiagnosticCode::DuplicateBinding | DiagnosticCode::SyntaxError
            )
    )
}

fn evidence(text: impl fmt::Display) -> String {
    let text = text.to_string().replace(['\n', '\t'], " ");
    match text.char_indices().nth(240) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// Runs one vendored test and classifies what happened.
pub(crate) fn run(relative: &str) -> Observed {
    static NAMES: OnceLock<BTreeSet<String>> = OnceLock::new();
    static UNSHIMMABLE: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    static COST: OnceLock<BTreeMap<String, (String, String)>> = OnceLock::new();
    let names = NAMES.get_or_init(refusal_codes);
    let unshimmable = UNSHIMMABLE.get_or_init(unshimmable_includes);
    if COST.get_or_init(cost_register).contains_key(relative) {
        return Observed::Harness(INSTRUCTION_COST.to_owned());
    }
    let path = data_path(relative);
    let meta = metadata::read_metadata(&path).unwrap_or_else(|error| panic!("{relative}: {error}"));
    if let Some(include) = meta
        .includes
        .iter()
        .find(|include| harness_shim(include).is_none())
    {
        assert!(
            unshimmable.contains_key(include.as_ref()),
            "{relative}: include {include} has neither a shim nor an unshimmable.tsv row"
        );
        // The test's own body may refuse regardless of the include; that
        // refusal is the more exact outcome. A body that only lacks the
        // include's bindings is the include's.
        let body = source_without_unshimmed(&path, &meta, meta.negative.is_none());
        return match admit(&body) {
            Err(error) if error.code != DiagnosticCode::UnknownBinding => {
                Observed::Refused(error.code.as_str().to_owned(), evidence(&error))
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
    let program = match admit(&source) {
        Ok(program) => program,
        Err(error) => {
            if EARLY_ERROR_CODES.contains(&error.code) {
                // An early error is the expected answer of a parse-negative
                // test. Anywhere else the program is valid ECMAScript, so the
                // front end rejecting it is a divergence, not a ruling —
                // unless the collision is the harness's own.
                return if parse_negative {
                    Observed::Pass
                } else if collides_with_harness(&path, &meta, &error) {
                    Observed::Harness(BINDING_COLLISION.to_owned())
                } else {
                    Observed::Diverged(evidence(format!(
                        "the front end rejects a valid program: {error}"
                    )))
                };
            }
            if error.code == DiagnosticCode::LinkError
                && error.to_string().contains("unknown module")
            {
                // A script has no module paths of its own; an identifier the
                // linker resolves as one is a front-end defect, not a ruling.
                return Observed::Diverged(evidence(format!(
                    "an identifier linked as a module: {error}"
                )));
            }
            if error.code == DiagnosticCode::SourceTooLarge {
                // The harness and the test share one cell. When the test
                // alone fits the cell limit, the refusal is the harness's.
                let alone = std::fs::read_to_string(&path).expect("read vendored Test262 test");
                if admit(&alone)
                    .err()
                    .is_none_or(|alone| alone.code != DiagnosticCode::SourceTooLarge)
                {
                    return Observed::Harness(PROGRAM_SIZE.to_owned());
                }
            }
            return Observed::Refused(error.code.as_str().to_owned(), evidence(&error));
        }
    };
    if let Some(negative) = meta.negative.as_ref().filter(|_| parse_negative) {
        return Observed::Diverged(format!(
            "expected an early {} but the program compiled",
            negative.error_type.as_str()
        ));
    }
    let host = Host::default();
    let environment = ExecutionEnvironment::new(&host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::instructions(INSTRUCTION_BUDGET),
        ExecutionBound::Unbounded,
    ));
    let result =
        futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &environment));
    let prints = host.prints.lock().expect("print journal").clone();
    classify_run(result, &meta, is_async, &prints, names)
}

fn classify_run(
    result: Result<ExecutionOutcome, RuntimeError>,
    meta: &metadata::Metadata,
    is_async: bool,
    prints: &[Value],
    names: &BTreeSet<String>,
) -> Observed {
    match (result, &meta.negative) {
        (Err(error), Some(negative))
            if thrown_name(&error).as_deref() == Some(negative.error_type.as_str()) =>
        {
            Observed::Pass
        }
        (Err(error), _) if error.to_string().contains(UNEXPECTED_ABILITY) => {
            Observed::Harness(HOST_EFFECTS.to_owned())
        }
        (Err(error), _) => match named_diagnostic(&error.to_string(), names) {
            Some(code) => Observed::Refused(code, evidence(&error)),
            None => Observed::Diverged(evidence(&error)),
        },
        (Ok(outcome), Some(negative)) => Observed::Diverged(format!(
            "expected a runtime {} but the program ended with {}",
            negative.error_type.as_str(),
            evidence(format!("{outcome:?}"))
        )),
        (Ok(outcome), None) if is_async => {
            let printed = prints
                .iter()
                .filter_map(|value| match value {
                    Value::String(text) => Some(text.as_str()),
                    _ => None,
                })
                .find(|text| text.starts_with("Test262:AsyncTest"));
            match printed {
                Some("Test262:AsyncTestComplete") => Observed::Pass,
                Some(failure) => match named_diagnostic(failure, names) {
                    Some(code) => Observed::Refused(code, evidence(failure)),
                    None => Observed::Diverged(evidence(failure)),
                },
                None => Observed::Diverged(format!(
                    "$DONE was never called; the program ended with {}",
                    evidence(format!("{outcome:?}"))
                )),
            }
        }
        (Ok(ExecutionOutcome::Finished(Value::Bool(true))), None) => Observed::Pass,
        (Ok(outcome), None) => Observed::Diverged(format!(
            "expected finish(true), got {}",
            evidence(format!("{outcome:?}"))
        )),
    }
}

/// Runs `paths` on every available core and returns what each observed, in
/// input order. A panic inside the dialect is itself a divergence. A test that
/// runs past the wall-clock bound fails the run with the unfinished tests'
/// names: a running worker cannot be killed, so the orchestrating thread
/// watches the per-test start times and exits non-zero itself.
pub(crate) fn run_all(paths: &[String]) -> Vec<Observed> {
    let next = AtomicUsize::new(0);
    let started = Mutex::new(vec![None; paths.len()]);
    let results = Mutex::new(vec![None; paths.len()]);
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else { break };
                    started.lock().expect("start times")[index] = Some(Instant::now());
                    let observed = std::panic::catch_unwind(|| run(path)).unwrap_or_else(|panic| {
                        let message = panic
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                            .unwrap_or_default();
                        Observed::Diverged(evidence(format!("panicked: {message}")))
                    });
                    results.lock().expect("results")[index] = Some(observed);
                }
            });
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let remaining: Vec<usize> = {
                let results = results.lock().expect("results");
                (0..paths.len())
                    .filter(|index| results[*index].is_none())
                    .collect()
            };
            if remaining.is_empty() {
                break;
            }
            let now = Instant::now();
            let overdue = {
                let started = started.lock().expect("start times");
                remaining.iter().any(|index| {
                    started[*index].is_some_and(|start| now.duration_since(start) > TEST_WALL_CLOCK)
                })
            };
            if overdue {
                eprintln!(
                    "a Test262 test exceeded the {} s wall-clock bound; unfinished: {}",
                    TEST_WALL_CLOCK.as_secs(),
                    remaining
                        .iter()
                        .map(|index| paths[*index].as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::process::exit(1);
            }
        }
    });
    results
        .into_inner()
        .expect("results")
        .into_iter()
        .map(|observed| observed.expect("every path ran"))
        .collect()
}

/// The record's tallies, derived from `outcomes`: the selection size, then
/// each class, then each class and qualifier. Nothing pins them; the tests
/// print them so a change of outcome still shows in the totals.
pub(crate) fn tally(outcomes: &BTreeMap<String, Outcome>) -> BTreeMap<(String, String), usize> {
    let mut counts = BTreeMap::new();
    *counts
        .entry(("selected".to_owned(), "-".to_owned()))
        .or_default() += outcomes.len();
    for outcome in outcomes.values() {
        *counts
            .entry((outcome.class().to_owned(), "*".to_owned()))
            .or_default() += 1;
        if !matches!(outcome, Outcome::Pass) {
            *counts
                .entry((outcome.class().to_owned(), outcome.detail().to_owned()))
                .or_default() += 1;
        }
    }
    counts
}

/// The tallies as `class\tqualifier\tcount` lines, for the tests' output.
pub(crate) fn tally_lines(outcomes: &BTreeMap<String, Outcome>) -> String {
    tally(outcomes)
        .iter()
        .map(|((class, detail), count)| format!("{class}\t{detail}\t{count}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The comparison of `observed` with the record, and the rewritten record
/// when blessing: every mismatch, as `path: recorded -> observed`.
pub(crate) fn compare(
    paths: &[String],
    observed: &[Observed],
    recorded: &BTreeMap<String, Outcome>,
) -> Vec<String> {
    paths
        .iter()
        .zip(observed)
        .filter_map(|(path, observed)| {
            let recorded = recorded.get(path);
            match recorded {
                Some(outcome) if observed.matches(outcome) => None,
                Some(outcome) => Some(format!("{path}: recorded `{outcome}`, observed {observed}")),
                None => Some(format!("{path}: not recorded, observed {observed}")),
            }
        })
        .collect()
}

/// Rewrites the `outcomes/<shard>.tsv` files in the source tree from a full
/// run, when `TEST262_BLESS` is set under `kiln run` (which exports
/// `BUILD_WORKSPACE_DIRECTORY`): one file per top-level test directory, and a
/// shard file whose directory no longer selects a test is removed, like
/// `generate.mjs` removes a table whose findings file is gone. A divergence
/// keeps its recorded owner or becomes `UNTRIAGED`, which the data checks
/// refuse until a ticket owns it. Returns whether it blessed.
pub(crate) fn bless(
    paths: &[String],
    observed: &[Observed],
    recorded: &BTreeMap<String, Outcome>,
) -> bool {
    if std::env::var_os("TEST262_BLESS").is_none() {
        return false;
    }
    assert!(
        !quick_requested(),
        "TEST262_BLESS rewrites the record from the run; unset LASH_QUICK so the whole selection is recorded"
    );
    let workspace = std::env::var("BUILD_WORKSPACE_DIRECTORY")
        .expect("TEST262_BLESS writes the source tree; run it through `kiln run`");
    let directory = Path::new(&workspace).join("crates/lash-typescript/tests/test262");
    let outcomes = paths
        .iter()
        .zip(observed)
        .map(|(path, observed)| (path.clone(), observed.outcome(recorded.get(path))))
        .collect::<BTreeMap<_, _>>();
    let mut shards: BTreeMap<String, Vec<(&String, &Outcome)>> = BTreeMap::new();
    for (path, outcome) in &outcomes {
        let shard = path.split('/').nth(1).expect("a test262 path's directory");
        shards
            .entry(shard.to_owned())
            .or_default()
            .push((path, outcome));
    }
    for (shard, rows) in &shards {
        let mut text = String::from("# test262-path\tclass\tqualifier\n");
        for (path, outcome) in rows {
            text.push_str(&format!(
                "{path}\t{}\t{}\n",
                outcome.class(),
                outcome.detail()
            ));
        }
        std::fs::write(
            directory.join("outcomes").join(format!("{shard}.tsv")),
            text,
        )
        .unwrap_or_else(|error| panic!("write outcomes/{shard}.tsv: {error}"));
    }
    for entry in std::fs::read_dir(directory.join("outcomes")).expect("read outcomes/") {
        let entry = entry.expect("an outcomes entry");
        let name = entry
            .file_name()
            .into_string()
            .expect("UTF-8 outcomes name");
        if let Some(shard) = name.strip_suffix(".tsv")
            && !shards.contains_key(shard)
        {
            std::fs::remove_file(entry.path())
                .unwrap_or_else(|error| panic!("remove stale outcomes/{name}: {error}"));
        }
    }
    eprintln!("{}", tally_lines(&outcomes));
    if let Ok(evidence_path) = std::env::var("TEST262_EVIDENCE") {
        let mut text = String::new();
        for (path, observed) in paths.iter().zip(observed) {
            match observed {
                Observed::Diverged(evidence) => {
                    text.push_str(&format!("{path}\tfail\t{evidence}\n"))
                }
                Observed::Refused(code, evidence) => {
                    text.push_str(&format!("{path}\trefused {code}\t{evidence}\n"));
                }
                Observed::Pass | Observed::Harness(_) => {}
            }
        }
        std::fs::write(evidence_path, text).expect("write divergence evidence");
    }
    true
}

/// The figures the README and the PR report: over the selection, and over
/// its executable part (every test not refused and not blocked on a harness
/// include).
pub(crate) fn summary(outcomes: &BTreeMap<String, Outcome>) -> String {
    let count = |class: &str| {
        outcomes
            .values()
            .filter(|outcome| outcome.class() == class)
            .count()
    };
    let (pass, refused, fail, harness) = (
        count("pass"),
        count("refused"),
        count("fail"),
        count("harness"),
    );
    let executable = pass + fail;
    let percent = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            100.0 * part as f64 / whole as f64
        }
    };
    format!(
        "Test262: selected={}, pass={pass}, refused={refused}, fail={fail}, harness={harness}; \
         pass rate {:.1}% of selected, {:.1}% of executable ({executable})",
        outcomes.len(),
        percent(pass, outcomes.len()),
        percent(pass, executable),
    )
}
