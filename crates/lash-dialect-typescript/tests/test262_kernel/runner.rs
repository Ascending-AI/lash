//! Runs vendored Test262 programs through the kernel: lowered by this
//! crate, then run on a kernel machine.
//!
//! One test has one observation: it passed, the front end refused it, it
//! diverged, or needs an unavailable harness include. The
//! report counts observations by directory beside the class main's record
//! holds for the same test, so a lane sees its family's distance to the
//! record.

// This file is test tooling; ambient filesystem access is sanctioned here.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use lash_dialect_typescript::{DiagnosticCode, define_helpers};
use lash_kernel_dialect::{Environment, Lowered, NamedLibrary};
use lash_kernel_doc::{Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Name, Timestamp};
use lash_kernel_vm::{
    Bindings, Bounds, End, Host, KernelMachine, Machine, Program, RunError, Start, Step, Target,
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

/// Fixed measurement bounds, identical for every Test262 case. Crossing one is
/// a failure with the kernel's typed bound error, never a conformance exemption.
/// These cases test semantics, not billion-element materialization. Four million
/// charge units and 8 MiB of guest memory allow ordinary harnesses and programs
/// while bounding dense representations of JavaScript's huge sparse arrays.
const BOUNDS: Bounds = Bounds {
    charge: 4_000_000,
    memory: 8 << 20,
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
    Refused(String, String),
    /// The program ran and did not do what the test expects.
    Diverged(String),
    /// The test needs a harness include that has no rendering.
    Harness(String),
}

impl Observed {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Refused(_, _) => "refused",
            Self::Diverged(_) => "fail",
            Self::Harness(_) => "harness",
        }
    }
}

/// Runs lowered programs on the production kernel machine.
pub(crate) fn executor() -> Box<dyn Executor> {
    Box::new(OnMachine::<KernelMachine>::new(Arc::clone(registry())))
}

/// All lowering and execution use the same content-addressed definitions.
/// Missing library dependencies fail setup rather than being reported as a pass.
fn registry() -> &'static Arc<FunctionRegistry> {
    static REGISTRY: std::sync::OnceLock<Arc<FunctionRegistry>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = FunctionRegistry::new();
        lash_kernel_lib::register_numbers(&mut registry).expect("numeric library registration");
        lash_kernel_lib::register_text_json(&mut registry).expect("text library registration");
        lash_kernel_vm::register_machine_functions(&mut registry)
            .expect("machine library registration");
        lash_kernel_lib::register_collections(&mut registry)
            .expect("collection library registration");
        lash_ext_regex_ecma::register(
            &mut registry,
            &Arc::new(lash_ext_regex_ecma::Engine::new(32)),
        )
        .expect("regex extension registration");
        lash_ext_date_ecma::register(&mut registry).expect("date extension registration");
        lash_ext_url_whatwg::register(&mut registry).expect("URL extension registration");
        let mut library = NamedLibrary::from_registry(&registry).expect("unique library names");
        for definition in define_helpers(&mut library).expect("dialect library dependencies") {
            registry
                .register(definition, None)
                .expect("dialect helper registration");
        }
        Arc::new(registry)
    })
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
        // The value the program's `finish` call ended it with.
        let mut finished_with = None;
        loop {
            match machine
                .run(&mut host, u64::MAX)
                .map_err(|error| error.to_string())?
            {
                Step::Ended(mut end) => {
                    if let (End::Finished(finished), Some(value)) = (&mut end, finished_with) {
                        finished.result = value;
                    }
                    return Ok(end);
                }
                Step::Slice => {}
                Step::Parked(park) => match park.requests.as_slice() {
                    [lash_kernel_vm::Request::Effect(effect)]
                        if effect.effect.as_str() == "finish" && finished_with.is_none() =>
                    {
                        finished_with = Some(effect.args.first().cloned().unwrap_or(Datum::Null));
                        machine
                            .deliver(effect.wait, lash_kernel_vm::Outcome::Completed(Datum::Null))
                            .map_err(|error| error.to_string())?;
                    }
                    requests => {
                        return Err(format!(
                            "the program waits on {} request(s); the Test262 host answers only `finish`",
                            requests.len()
                        ));
                    }
                },
            }
        }
    }
}

/// The test host's turn-ending tool: a program ends with
/// `await finish(value)`, as a model's cell ends with `control.finish`.
fn finish_effect() -> BTreeMap<lash_kernel_doc::EffectName, lash_kernel_doc::Signature> {
    BTreeMap::from([(
        lash_kernel_doc::EffectName::new("finish").expect("a tool's name"),
        lash_kernel_doc::Signature {
            params: vec![lash_kernel_doc::Param {
                name: Name::new("value"),
                ty: lash_kernel_doc::Type::Any,
                optional: false,
            }],
            result: lash_kernel_doc::Type::Any,
        },
    )])
}

/// [`finish_effect`]'s control: its call ends the program.
fn finish_control()
-> &'static BTreeMap<lash_kernel_doc::EffectName, BTreeSet<lash_kernel_dialect::EffectControl>> {
    static CONTROLS: std::sync::OnceLock<
        BTreeMap<lash_kernel_doc::EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>,
    > = std::sync::OnceLock::new();
    CONTROLS.get_or_init(|| {
        finish_effect()
            .into_keys()
            .map(|name| {
                (
                    name,
                    BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
                )
            })
            .collect()
    })
}

/// The same real definitions the executor runs, indexed by name for lowering.
fn library() -> &'static NamedLibrary {
    static LIBRARY: std::sync::OnceLock<NamedLibrary> = std::sync::OnceLock::new();
    LIBRARY.get_or_init(|| NamedLibrary::from_registry(registry()).expect("unique library names"))
}

/// The JavaScript error class an uncaught value belongs to: the kernel's
/// own kinds by the name JavaScript gives them, any other kind as written,
/// and a thrown object by its `name`.
fn thrown_name(value: &Datum) -> Option<String> {
    let (record, kind) = match value {
        Datum::Record(_) => (Some(value), None),
        Datum::Error(error) => (Some(&error.data), Some(error.kind.as_str())),
        _ => (None, None),
    };
    if let Some(Datum::Record(fields)) = record
        && let Some((_, Datum::Text(name))) = fields.iter().find(|(field, _)| field == "name")
    {
        return Some(name.clone());
    }
    kind.map(|kind| match kind {
        "type_error" | "key_missing" | "invalid_key" | "not_data" | "cycle" => "TypeError".into(),
        "index_out_of_range" | "number_range" => "RangeError".into(),
        "unbound_variable" => "ReferenceError".into(),
        other => other.to_string(),
    })
}

/// Whether the run reached the end of its test: the script's last
/// statement binds [`super::ingest::COMPLETED`] to `true`. No call marks
/// it, so the test's code lowers as it is written, with no top-level
/// `await`.
fn completed(finished: &lash_kernel_vm::Finished) -> bool {
    finished
        .bindings
        .variables
        .get(&Name::new(super::ingest::COMPLETED))
        == Some(&lash_kernel_doc::Value::Bool(true))
}

fn judge(end: End, meta: &Metadata) -> Observed {
    match (end, &meta.negative) {
        (End::Error(RunError::Uncaught(error)), Some(negative))
            if thrown_name(&error).as_deref() == Some(negative.error_type.as_str()) =>
        {
            Observed::Pass
        }
        (End::Finished(finished), None) if completed(&finished) => Observed::Pass,
        (End::Finished(finished), _) => {
            Observed::Diverged(format!("the program finished with {:?}", finished.result))
        }
        (End::Error(error), _) => Observed::Diverged(error.to_string()),
        (End::Failed(reason), _) => Observed::Diverged(format!("the program failed: {reason:?}")),
        (End::Cancelled, _) => Observed::Diverged("the run was cancelled".to_string()),
    }
}

fn lower(source: &str) -> Result<Lowered, lash_dialect_typescript::Diagnostic> {
    let effects = finish_effect();
    let bindings = BTreeSet::new();
    let environment = Environment {
        library: library(),
        effects: &effects,
        controls: finish_control(),
        bindings: &bindings,
        functions: &std::collections::BTreeMap::new(),
    };
    lash_dialect_typescript::lower(source, &environment)
}

/// Lowers and runs one vendored test, `test/...`.
pub(crate) fn run(relative: &str, executor: &dyn Executor) -> Observed {
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
                Observed::Refused(diagnostic.code.as_str().to_string(), diagnostic.message)
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
        Err(diagnostic) => {
            return Observed::Refused(diagnostic.code.as_str().to_string(), diagnostic.message);
        }
    };
    if let Some(negative) = meta.negative.as_ref().filter(|_| parse_negative) {
        return Observed::Diverged(format!(
            "expected an early {} but the program lowered",
            negative.error_type.as_str()
        ));
    }
    match executor.run(&lowered) {
        Ok(end) => judge(end, &meta),
        Err(problem) => Observed::Diverged(problem),
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
/// refused, failed, could not run for a missing harness include, and how
/// many of them main's record marks `pass`.
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
        "directory\tpass\trefused\tfail\tharness\trecorded-pass\tof-those-not-passing\n",
    );
    for (directory, row) in &rows {
        let count = |class: &str| row.classes.get(class).copied().unwrap_or(0);
        writeln!(
            out,
            "{directory}\t{}\t{}\t{}\t{}\t{}\t{}",
            count("pass"),
            count("refused"),
            count("fail"),
            count("harness"),
            row.recorded_pass,
            row.regressed
        )
        .expect("write to a string");
    }
    // Why the front end refused, most frequent first: the built-in or
    // construct a family is waiting on.
    let mut codes: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, observed) in observations {
        if let Observed::Refused(code, _) = observed {
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

/// The printer law for every vendored program the front end accepts.
/// Equality of the resulting kernel program preserves every possible machine
/// outcome, including bounds, host reads, effects and session mutation.
pub(crate) fn printing_relowers(relative: &str) -> bool {
    let path = data_path(relative);
    let meta = metadata::read_metadata(&path).unwrap_or_else(|error| panic!("{relative}: {error}"));
    // A Test262 program is the test body; the harness is supplied by the
    // test host. Lower the bridged body against its declared harness bindings.
    let source = super::ingest::test_script(
        &path,
        &meta,
        meta.negative.is_none() && !meta.flags.contains(&TestFlag::Async),
    );
    let effects = finish_effect();
    let bindings = [
        "assert",
        "__test262Assert",
        "__test262SameValue",
        "__test262Throws",
        "__test262CompareArray",
        "__test262ErrorThrower",
        "Test262Error",
        "$DONE",
        "$DONOTEVALUATE",
        "compareArray",
        "verifyProperty",
        "verifyEqualTo",
        "verifyWritable",
        "verifyNotWritable",
        "verifyEnumerable",
        "verifyNotEnumerable",
        "verifyConfigurable",
        "verifyNotConfigurable",
    ]
    .into_iter()
    .map(Name::new)
    .collect();
    let environment = Environment {
        library: library(),
        effects: &effects,
        controls: finish_control(),
        bindings: &bindings,
        functions: &std::collections::BTreeMap::new(),
    };
    let Ok(original) = lash_dialect_typescript::lower(&source, &environment) else {
        return false;
    };
    let printed =
        lash_dialect_typescript::print(&original.document).expect("an admitted document prints");
    let re_lowered = lash_dialect_typescript::lower_kernel_text(&printed, &environment)
        .unwrap_or_else(|error| panic!("{relative}: {error}\n{printed}"));
    assert_eq!(original.document, re_lowered.document, "{relative}");
    true
}

/// String positions count UTF-16 units, while codePointAt combines a pair.
#[test]
fn string_positions_count_utf16_units() {
    let source = "await finish(['😀'.length, '😀'.charCodeAt(0), '😀'.charCodeAt(1), '😀'.codePointAt(0), '😀x'.indexOf('x'), '😀'.slice(0, 2)]);";
    let result = execute(source);
    let expected = Datum::List(vec![
        Datum::Float(lash_kernel_doc::Float::new(2.0)),
        Datum::Float(lash_kernel_doc::Float::new(55357.0)),
        Datum::Float(lash_kernel_doc::Float::new(56832.0)),
        Datum::Float(lash_kernel_doc::Float::new(128512.0)),
        Datum::Float(lash_kernel_doc::Float::new(2.0)),
        Datum::Text("😀".into()),
    ]);
    assert_finished(result, expected);
}

/// §2.4: the helper writes lastIndex to the receiver that its aliases share.
#[test]
fn regexp_aliases_observe_coerced_last_index_and_failure_reset() {
    let result = execute(
        "const r = /x/g; const alias = r; r.lastIndex = '1'; const match = r.exec('xx'); const index = alias.lastIndex; const missed = r.test('x'); await finish([index, match.index, match[0], missed, alias.lastIndex]);",
    );
    assert_finished(
        result,
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(2.0)),
            Datum::Float(lash_kernel_doc::Float::new(1.0)),
            Datum::Text("x".into()),
            Datum::Bool(false),
            Datum::Float(lash_kernel_doc::Float::new(0.0)),
        ]),
    );
}

/// ECMA URI codecs distinguish reserved URI escapes from component escapes.
#[test]
fn uri_codecs_preserve_reserved_escape_spelling() {
    assert_finished(
        execute(
            "await finish([encodeURI('é?#'), encodeURIComponent('é?#'), decodeURI('%2f%C3%A9'), decodeURIComponent('%2f%C3%A9')]);",
        ),
        Datum::List(vec![
            Datum::Text("%C3%A9?#".into()),
            Datum::Text("%C3%A9%3F%23".into()),
            Datum::Text("%2fé".into()),
            Datum::Text("/é".into()),
        ]),
    );
}

/// ECMA Decode rejects UTF-8 encodings of surrogates with a typed URIError.
#[test]
fn uri_decode_refuses_surrogate_utf8() {
    let result = execute("await finish(decodeURIComponent('%ED%A0%80'));");
    assert!(
        matches!(result, End::Error(RunError::Uncaught(error)) if thrown_name(&error).as_deref() == Some("URIError"))
    );
}

/// §2.2: each template substitution is converted before the next is read.
#[test]
fn template_substitutions_convert_in_source_order() {
    assert_finished(
        execute(
            "let n = 0; const value = {toString() { n = n + 1; return String(n); }}; await finish(`${value}${value}`);",
        ),
        Datum::Text("12".into()),
    );
}

#[cfg(test)]
fn execute(source: &str) -> End {
    let lowered = lower(source).expect("a supported dialect witness lowers");
    executor()
        .run(&lowered)
        .expect("the kernel executes the witness")
}

#[cfg(test)]
fn assert_finished(end: End, expected: Datum) {
    match end {
        End::Finished(finished) => assert_eq!(finished.result, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

/// K-BND-001: dense materialization of a huge JavaScript sparse index must
/// end at the memory bound, so the next conformance case can still run.
#[test]
fn huge_sparse_array_writes_end_at_the_memory_bound() {
    let end = execute("const xs = []; xs[2147483648] = 1; await finish(xs.length);");
    assert!(
        matches!(end, End::Error(RunError::Bound(lash_kernel_vm::BoundExceeded {
        bound: lash_kernel_vm::Bound::Memory,
        limit,
        ..
    })) if limit == BOUNDS.memory),
        "{end:?}"
    );
}

/// ECMA Array.prototype.unshift: with no arguments, only ToLength and the
/// final length write occur; a huge array-like object is never traversed.
#[test]
fn unshift_without_arguments_does_not_traverse_the_receiver() {
    assert_finished(
        execute(
            "const xs = {length: Infinity}; const length = Array.prototype.unshift.call(xs); await finish([length, xs.length]);",
        ),
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(9007199254740991.0)),
            Datum::Float(lash_kernel_doc::Float::new(9007199254740991.0)),
        ]),
    );
}

/// OrdinaryToPrimitive searches inherited defaults in hint order, but an
/// explicitly shadowed non-callable method is skipped rather than restored.
#[test]
fn ordinary_to_primitive_uses_defaults_only_for_missing_methods() {
    assert_finished(
        execute(
            "const text = String({valueOf() { return 1; }}); const number = Number({valueOf() { return {}; }}); let refused = false; try { Number({valueOf: undefined, toString: undefined}); } catch (e) { refused = e.name === 'TypeError'; } await finish([text, Number.isNaN(number), refused]);",
        ),
        Datum::List(vec![
            Datum::Text("[object Object]".into()),
            Datum::Bool(true),
            Datum::Bool(true),
        ]),
    );
}

/// SerializeJSONProperty continues with the value returned by toJSON, including
/// omitted object properties and null array entries when it returns undefined.
#[test]
fn json_stringify_rechecks_the_kind_after_to_json() {
    assert_finished(
        execute(
            "const obj = {toJSON() { return undefined; }}; await finish([JSON.stringify(obj) === undefined, JSON.stringify([1, obj, 3]), JSON.stringify({key: obj})]);",
        ),
        Datum::List(vec![
            Datum::Bool(true),
            Datum::Text("[1,null,3]".into()),
            Datum::Text("{}".into()),
        ]),
    );
}

/// Array search returns immediately for an empty receiver, before fromIndex coercion.
#[test]
fn empty_array_search_does_not_convert_from_index() {
    assert_finished(
        execute(
            "const from = {valueOf() { throw new Error('converted'); }}; await finish([ [].indexOf(1, from), [].lastIndexOf(1, from), [].includes(1, from) ]);",
        ),
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(-1.0)),
            Datum::Float(lash_kernel_doc::Float::new(-1.0)),
            Datum::Bool(false),
        ]),
    );
}

/// ECMA whitespace includes BOM and excludes NEL for both trim and number parsing.
#[test]
fn trim_and_number_parsing_use_ecma_whitespace() {
    assert_finished(
        execute(
            "await finish(['\\uFEFF x \\uFEFF'.trim(), '\\uFEFFx'.trimStart(), 'x\\uFEFF'.trimEnd(), '\\u0085x\\u0085'.trim(), Number('\\uFEFF1'), parseInt('\\uFEFF1'), parseFloat('\\uFEFF1'), Number.isNaN(Number('\\u00851'))]);",
        ),
        Datum::List(vec![
            Datum::Text("x".into()),
            Datum::Text("x".into()),
            Datum::Text("x".into()),
            Datum::Text("\u{85}x\u{85}".into()),
            Datum::Float(lash_kernel_doc::Float::new(1.0)),
            Datum::Float(lash_kernel_doc::Float::new(1.0)),
            Datum::Float(lash_kernel_doc::Float::new(1.0)),
            Datum::Bool(true),
        ]),
    );
}

/// MakeTime and MakeDate use the specified left-to-right floating-point evaluation.
#[test]
fn date_utc_preserves_floating_point_evaluation_order() {
    assert_finished(
        execute(
            "await finish([Date.UTC(1970, 0, 1, 80063993375, 29, 1, -288230376151711740), Date.UTC(1970, 0, 213503982336, 0, 0, 0, -18446744073709552000)]);",
        ),
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(29312.0)),
            Datum::Float(lash_kernel_doc::Float::new(34447360.0)),
        ]),
    );
}

/// Array-pattern iteration yields undefined for holes and creates dense rest elements.
#[test]
fn destructuring_holes_apply_defaults_and_make_dense_rest() {
    assert_finished(
        execute(
            "const [x = 23, ...tail] = [, , 4]; await finish([x, Object.hasOwn(tail, '0'), tail[0] === undefined, tail[1]]);",
        ),
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(23.0)),
            Datum::Bool(true),
            Datum::Bool(true),
            Datum::Float(lash_kernel_doc::Float::new(4.0)),
        ]),
    );
}

/// A logical assignment returns its stored value even when storing consumes a temporary.
#[test]
fn logical_assignment_returns_the_stored_closure() {
    assert_finished(
        execute(
            "let a; let b = false; let c = true; const x = (a ??= function() { return 7; }); const y = (b ||= function() { return 8; }); const z = (c &&= function() { return 9; }); await finish([x(), y(), z(), a(), b(), c()]);",
        ),
        Datum::List(
            (7..=9)
                .chain(7..=9)
                .map(|n| Datum::Float(lash_kernel_doc::Float::new(f64::from(n))))
                .collect(),
        ),
    );
}

/// ToIntegerOrInfinity normalizes negative zero before returning an array index.
#[test]
fn array_search_returns_positive_zero_and_empty_shift_skips_index_zero() {
    assert_finished(
        execute(
            "const obj = {length: -1, 0: 99}; const shifted = Array.prototype.shift.call(obj); await finish([1 / [true].indexOf(true, -0), 1 / [true].lastIndexOf(true, -0), shifted === undefined, obj.length, obj[0]]);",
        ),
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(f64::INFINITY)),
            Datum::Float(lash_kernel_doc::Float::new(f64::INFINITY)),
            Datum::Bool(true),
            Datum::Float(lash_kernel_doc::Float::new(0.0)),
            Datum::Float(lash_kernel_doc::Float::new(99.0)),
        ]),
    );
}

/// Boolean.valueOf requires a Boolean receiver.
#[test]
fn borrowed_boolean_value_of_rejects_other_kinds() {
    assert_finished(
        execute(
            "let typeError = false; try { Boolean.prototype.valueOf.call({}); } catch (e) { typeError = e.name === 'TypeError'; } await finish(typeError);",
        ),
        Datum::Bool(true),
    );
}

/// Bind rejects a non-callable target before creating a bound function.
#[test]
fn borrowed_bind_rejects_non_callable_targets() {
    assert_finished(
        execute(
            "let count = 0; for (const value of [undefined, null, true, 1, 'x', [], {}]) { try { Function.prototype.bind.call(value); } catch (e) { if (e.name === 'TypeError') { count++; } } } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(7.0)),
    );
}

/// Map prototype methods require the Map internal slot, even for other containers.
#[test]
fn borrowed_map_methods_require_map_receivers() {
    assert_finished(
        execute(
            "let count = 0; const set = []; try { Map.prototype.set.call(set, 1, 2); } catch (e) { if (e.name === 'TypeError') count++; } try { Map.prototype.clear.call(set); } catch (e) { if (e.name === 'TypeError') count++; } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(2.0)),
    );
}

/// Set prototype methods require the Set internal slot, even for other containers.
#[test]
fn borrowed_set_methods_require_set_receivers() {
    assert_finished(
        execute(
            "let count = 0; const map = new Map(); try { Set.prototype.add.call(map, 1); } catch (e) { if (e.name === 'TypeError') count++; } try { Set.prototype.entries.call([]); } catch (e) { if (e.name === 'TypeError') count++; } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(2.0)),
    );
}

/// Date.toJSON is generic and invokes the receiver's toISOString.
#[test]
fn date_to_json_is_generic_and_date_only_methods_check_the_receiver() {
    assert_finished(
        execute(
            "let count = 0; try { Date.prototype.getDate.call({}); } catch (e) { if (e.name === 'TypeError') count++; } try { Date.prototype.setDate.call({}, 1); } catch (e) { if (e.name === 'TypeError') count++; } const obj = {valueOf() {return 0;}, toISOString() {return 'custom';}}; await finish([Date.prototype.toJSON.call(obj), Date.prototype.toJSON.call(new Date(NaN)), count]);",
        ),
        Datum::List(vec![
            Datum::Text("custom".into()),
            Datum::Null,
            Datum::Float(lash_kernel_doc::Float::new(2.0)),
        ]),
    );
}

/// ECMAScript exponentiation returns NaN for either unit magnitude raised to infinity.
#[test]
fn exponentiation_of_unit_magnitude_by_infinity_is_nan() {
    assert_finished(
        execute(
            "await finish([Number.isNaN((-1) ** Infinity), Number.isNaN((-1) ** -Infinity), Number.isNaN(1 ** Infinity), (-1) ** 0]);",
        ),
        Datum::List(vec![
            Datum::Bool(true),
            Datum::Bool(true),
            Datum::Bool(true),
            Datum::Float(lash_kernel_doc::Float::new(1.0)),
        ]),
    );
}

/// Compound assignment converts a computed property key once; object literals
/// convert each key before evaluating its value.
#[test]
fn computed_property_keys_convert_once_and_before_object_values() {
    assert_finished(
        execute(
            "let count = 0; const key = {toString() { count++; return 'x'; }}; const obj = {x: 1}; obj[key] += 2; const value = {[key]: count}; await finish([count, obj.x, value.x]);",
        ),
        Datum::List(vec![
            Datum::Float(lash_kernel_doc::Float::new(2.0)),
            Datum::Float(lash_kernel_doc::Float::new(3.0)),
            Datum::Float(lash_kernel_doc::Float::new(2.0)),
        ]),
    );
}

/// Computed member reads reject a nullish base before coercing the key.
#[test]
fn computed_member_reads_check_the_base_before_key_conversion() {
    assert_finished(
        execute(
            "const key = {toString() { throw new Error('key'); }}; let count = 0; for (const base of [null, undefined]) { try { base[key]; } catch (e) { if (e.name === 'TypeError') count++; } } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(2.0)),
    );
}

/// K-VAL-006: kernel Text holds scalar values, so a lone surrogate has no value.
#[test]
fn lone_surrogate_literals_and_results_have_typed_refusals() {
    let diagnostic = lower(r#"await finish("\uD800");"#).expect_err("a lone surrogate is refused");
    assert_eq!(
        diagnostic.code,
        DiagnosticCode::LoneSurrogateLiteralUnsupported
    );
    let end = execute("await finish(String.fromCharCode(0xD800));");
    assert!(
        matches!(end, End::Error(RunError::Uncaught(error)) if thrown_name(&error).as_deref() == Some("TS_LONE_SURROGATE_UNSUPPORTED"))
    );
}

/// Function.toString checks callability before refusing unavailable source text.
#[test]
fn function_to_string_rejects_non_callable_receivers() {
    assert_finished(
        execute(
            "let count = 0; for (const value of [undefined, null, {}]) { try { Function.prototype.toString.call(value); } catch (e) { if (e.name === 'TypeError') count++; } } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(3.0)),
    );
}

/// Map.groupBy validates the callback even when its input is empty.
#[test]
fn map_group_by_checks_the_callback_before_iteration() {
    assert_finished(
        execute(
            "let count = 0; for (const callback of [undefined, null, {}]) { try { Map.groupBy([], callback); } catch (e) { if (e.name === 'TypeError') count++; } } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(3.0)),
    );
}

/// Object.groupBy validates the callback even when its input is empty.
#[test]
fn object_group_by_checks_the_callback_before_iteration() {
    assert_finished(
        execute(
            "let count = 0; for (const callback of [undefined, null, {}]) { try { Object.groupBy([], callback); } catch (e) { if (e.name === 'TypeError') count++; } } await finish(count);",
        ),
        Datum::Float(lash_kernel_doc::Float::new(3.0)),
    );
}
