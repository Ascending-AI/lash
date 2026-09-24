//! Test262 conformance, PR lane (FIG-3646): the policy data is exhaustive and
//! consistent, every census rejection fires, and a stratified sample of the
//! selection keeps its recorded outcomes. `test262_full.rs` runs the whole
//! selection against the same record.
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, BTreeSet};

use lash_typescript::DiagnosticCode;
use lashlang::{
    ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionOutcome, RuntimeError, State,
    Value,
};

#[path = "test262/support/ingest.rs"]
#[allow(
    dead_code,
    reason = "the corpus laws read the harness bindings; the runner does not"
)]
mod ingest;
#[path = "test262/support/metadata.rs"]
mod metadata;
#[path = "test262/support/runner.rs"]
#[allow(
    dead_code,
    reason = "the sample never blesses the record; the full run does"
)]
mod runner;

use ingest::data_path;
use runner::{Host, Outcome, data_lines};

#[test]
fn inventory_census_and_skip_register_are_exhaustive() {
    let inventory = data_lines("inventory.tsv", 2)
        .into_iter()
        .map(|fields| (fields[0].clone(), fields[1].clone()))
        .collect::<BTreeSet<_>>();
    let census_rows = data_lines("census.tsv", 5);
    let census = census_rows
        .iter()
        .map(|fields| ((fields[0].clone(), fields[1].clone()), fields[2].clone()))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        census.keys().cloned().collect::<BTreeSet<_>>(),
        inventory,
        "the Test262 census must have no gap or extra row"
    );
    assert_eq!(census.len(), census_rows.len(), "duplicate census row");

    let refusal_codes = runner::refusal_codes();
    for fields in &census_rows {
        match fields[2].as_str() {
            "accepted" => assert_eq!(fields[3], "-", "accepted row must have reason `-`"),
            "rejected" => assert!(
                refusal_codes.contains(fields[3].as_str()) || fields[3] == "TS_PENDING_TOOL",
                "rejected census row {}:{} names unknown diagnostic {}",
                fields[0],
                fields[1],
                fields[3]
            ),
            "skip" => assert!(
                fields[3].starts_with("ticket-ruling:")
                    || fields[3].starts_with("registered-deviation:"),
                "skip census row {}:{} lacks a ruling or deviation: {}",
                fields[0],
                fields[1],
                fields[3]
            ),
            status => panic!("unknown census status `{status}`"),
        }
        if fields[2] == "rejected" {
            assert_ne!(
                fields[4], "-",
                "rejected census row {}:{} needs a probe or a probe-exempt reason",
                fields[0], fields[1]
            );
        } else {
            assert_eq!(
                fields[4], "-",
                "only rejected census rows carry a probe: {}:{}",
                fields[0], fields[1]
            );
        }
    }

    // Every excluded upstream path names the census row that excludes it, and
    // that row is not accepted; the selection and the exclusions partition
    // the upstream tree.
    let skip_rows = data_lines("skip-register.tsv", 2);
    let skipped = skip_rows
        .iter()
        .map(|fields| fields[0].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        skipped.len(),
        skip_rows.len(),
        "duplicate skip-register path"
    );
    for fields in &skip_rows {
        let (kind, name) = fields[1]
            .split_once(':')
            .unwrap_or_else(|| panic!("{}: exclusion must name a census row", fields[0]));
        let status = census
            .get(&(kind.to_owned(), name.to_owned()))
            .unwrap_or_else(|| {
                panic!("{}: exclusion names no census row {}", fields[0], fields[1])
            });
        assert_ne!(
            status, "accepted",
            "{}: an accepted row excludes nothing",
            fields[0]
        );
    }
    let selected = runner::vendored_tests();
    assert!(
        selected.iter().all(|path| !skipped.contains(path.as_str())),
        "a selected test cannot also be excluded"
    );
    // The selection rule, restated over the vendored bytes rather than
    // trusted from sync.mjs: every census row a selected test touches is
    // accepted.
    let accepted = |kind: &str, name: &str| {
        census
            .get(&(kind.to_owned(), name.to_owned()))
            .map(String::as_str)
            == Some("accepted")
    };
    for path in &selected {
        let meta = metadata::read_metadata(&data_path(path))
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        let mut parts = path.split('/').skip(1);
        let top = parts.next().expect("a top-level directory");
        assert!(
            accepted("directory", top),
            "{path}: directory {top} is not accepted"
        );
        if top == "built-ins"
            && let Some(built_in) = parts.next()
            && census.contains_key(&("feature".to_owned(), built_in.to_owned()))
        {
            assert!(
                accepted("feature", built_in),
                "{path}: built-in {built_in} is not accepted"
            );
        }
        for feature in meta.features.iter() {
            assert!(
                accepted("feature", feature),
                "{path}: feature {feature} is not accepted"
            );
        }
        for flag in meta.flags.iter() {
            assert!(
                accepted("flag", flag.name()),
                "{path}: flag {} is not accepted",
                flag.name()
            );
        }
    }
    let upstream_count = std::fs::read_to_string(data_path("upstream-test-count.txt"))
        .expect("read upstream Test262 count")
        .trim()
        .parse::<usize>()
        .expect("upstream Test262 count is a number");
    assert_eq!(
        selected.len() + skip_rows.len(),
        upstream_count,
        "every upstream test path must be selected or excluded by a census row"
    );
}

/// The outcome record covers the selection exactly and every entry names what
/// owns it. The record's tallies are derived, so they print in the output
/// rather than pinning a second copy that would merge-conflict on every
/// change.
#[test]
fn every_selected_test_has_one_owned_outcome() {
    let outcomes = runner::recorded_outcomes();
    let selected = runner::vendored_tests();
    assert_eq!(
        outcomes.keys().cloned().collect::<BTreeSet<_>>(),
        selected,
        "outcomes.tsv must name every vendored test exactly once"
    );
    let names = runner::refusal_codes();
    let unshimmable = runner::unshimmable_includes();
    // A refusal is only as good as the ruling behind it: each code the
    // selection shows must be the diagnostic a rejected census row names.
    let census_codes = data_lines("census.tsv", 5)
        .into_iter()
        .filter(|fields| fields[2] == "rejected")
        .map(|fields| fields[3].clone())
        .collect::<BTreeSet<_>>();
    for (path, outcome) in &outcomes {
        match outcome {
            Outcome::Pass => {}
            Outcome::Refused(code) => {
                assert!(
                    names.contains(code),
                    "{path}: {code} is not a real diagnostic"
                );
                assert!(
                    census_codes.contains(code),
                    "{path}: refusal {code} has no rejected census row naming it"
                );
            }
            Outcome::Fail(owner) => {
                let ticket = owner.strip_prefix("FIG-").is_some_and(|number| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                });
                let deviation = owner
                    .strip_prefix("registered-deviation:")
                    .is_some_and(|name| !name.is_empty());
                assert!(
                    ticket || deviation,
                    "{path}: a failure must name its ticket or registered deviation, not `{owner}`"
                );
            }
            Outcome::Harness(include) => assert!(
                unshimmable.contains_key(include),
                "{path}: harness outcome {include} has no unshimmable.tsv row"
            ),
        }
    }
    eprintln!("{}", runner::tally_lines(&outcomes));
    for include in unshimmable.keys() {
        assert!(
            ingest::harness_shim(include).is_none(),
            "{include} has a shim; drop its unshimmable.tsv row"
        );
    }
}

#[test]
fn sample_matches_the_ratchet() {
    let recorded = runner::recorded_outcomes();
    let sample = runner::sample_paths();
    assert!(
        sample.iter().all(|path| recorded.contains_key(path)),
        "the sample must be drawn from the selection"
    );
    let observed = runner::run_all(&sample);
    let mismatches = runner::compare(&sample, &observed, &recorded);
    let sampled = sample
        .iter()
        .map(|path| (path.clone(), recorded[path].clone()))
        .collect::<BTreeMap<_, _>>();
    eprintln!("sample: {}", runner::summary(&sampled));
    assert!(
        mismatches.is_empty(),
        "{} sampled Test262 outcomes changed; a new pass must be promoted, a new \
         failure fixed or owned, a changed refusal re-recorded (see README.md):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}
#[test]
fn program_bounds_bypass_guest_catch_and_finally() {
    let source = r#"
        try {
            while (true) {}
        } catch (error) {
            console.log("caught");
        } finally {
            console.log("finally");
        }
        finish(true);
    "#;
    let program = lash_typescript::testing::compile(source).expect("bound test compiles");
    let host = Host::default();
    let environment = ExecutionEnvironment::new(&host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::instructions(32),
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
    ));
    let error =
        futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &environment))
            .expect_err("the instruction bound must terminate the program");
    assert!(matches!(
        error,
        RuntimeError::InstructionBudgetExceeded { limit: 32 }
    ));
    assert!(
        host.prints.lock().expect("print journal").is_empty(),
        "neither catch nor finally may run after a program bound"
    );
}

#[test]
fn typescript_type_syntax_status_is_pinned() {
    let erased = r#"
        interface Box<T> { value: T }
        type Alias<T> = T;
        function identity<T>(value: T): T { return value; }
        const value: Alias<number> = identity<number>((1 as number satisfies number)!);
        finish(value);
    "#;
    let program = lash_typescript::testing::compile(erased).expect("type-only syntax is erased");
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &Host::default(),
    ))
    .expect("erased TypeScript program executes");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));

    let enum_program = lash_typescript::testing::compile("enum E { A } finish(E.A);")
        .expect("runtime enums are accepted TypeScript syntax");
    let enum_outcome = futures::executor::block_on(lashlang::execute(
        &enum_program,
        &mut State::new(),
        &Host::default(),
    ))
    .expect("runtime enum program executes");
    assert_eq!(enum_outcome, ExecutionOutcome::Finished(Value::Number(0.0)));

    for (source, expected) in [
        ("namespace N {}", DiagnosticCode::NamespaceUnsupported),
        ("@sealed class C {}", DiagnosticCode::DecoratorUnsupported),
    ] {
        let error =
            lash_typescript::testing::compile(source).expect_err("construct stays rejected");
        assert_eq!(error.code, expected, "source: {source}");
    }
}

/// Every rejected census row's named diagnostic is the one that actually fires.
///
/// The census is a claim about what this dialect does with each Test262
/// feature, and until now the claim and the code were only connected by whoever
/// wrote the row. They had drifted apart in both directions: `__proto__` was
/// listed as `TS_PROTOTYPE_MUTATION_UNSUPPORTED` while it compiled and ran;
/// `Error.isError` and `Uint8Array.fromBase64` were listed as
/// `TS_METHOD_UNSUPPORTED` while they reported `TS_AWAIT_REQUIRED`, telling the
/// reader to `await` a method that does not exist; `async-iteration`,
/// `await-dictionary`, and `import.meta` each named a diagnostic no code path
/// produces; and `well-formed-json-stringify` named a method rejection for a
/// feature the value model refuses one level down, at the lone surrogate.
///
/// So each rejected row now carries a source that must reject with exactly the
/// diagnostic the row names. A row whose feature has no derivable one-line
/// probe says `probe-exempt:` and why, which is a claim a reader can check
/// rather than an omission they cannot see.
#[test]
fn rejected_census_rows_name_the_diagnostic_that_fires() {
    let mut probed = 0;
    let mut exempt = 0;
    for fields in data_lines("census.tsv", 5)
        .into_iter()
        .filter(|fields| fields[2] == "rejected")
    {
        let [kind, name, _, expected, probe] = &fields[..] else {
            unreachable!("census rows have five columns")
        };
        if let Some(reason) = probe.strip_prefix("probe-exempt:") {
            assert!(
                reason.len() > 20,
                "{kind}:{name} must say why it has no probe"
            );
            exempt += 1;
            continue;
        }
        if expected == "TS_PENDING_TOOL" {
            let program =
                lash_typescript::testing::compile(probe).expect("await resolves at runtime");
            let error = futures::executor::block_on(lashlang::execute(
                &program,
                &mut State::new(),
                &Host::default(),
            ))
            .expect_err("await requires a pending handle");
            assert!(
                matches!(error, RuntimeError::PendingTool { .. }),
                "{probe}: {error}"
            );
            probed += 1;
            continue;
        }
        // `repeat:<count>:<text>` spells a probe too long to write out: the
        // cell-size limit's.
        let probe = match probe.strip_prefix("repeat:") {
            Some(spec) => {
                let (count, text) = spec.split_once(':').expect("repeat:<count>:<text>");
                text.repeat(count.parse().expect("a repeat count"))
            }
            None => probe.clone(),
        };
        match lash_typescript::validate(&probe) {
            Err(error) => assert_eq!(
                error.code.as_str(),
                expected,
                "{kind}:{name} claims {expected} but `{probe}` reports {error}"
            ),
            // A link-time refusal fires on admission, and a shape-dependent
            // one when the program runs, as the deviation register records.
            Ok(()) => {
                let program = match runner::admit(&probe) {
                    Ok(program) => program,
                    Err(error) => {
                        assert_eq!(
                            error.code.as_str(),
                            expected,
                            "{kind}:{name} claims {expected} but `{probe}` reports {error}"
                        );
                        probed += 1;
                        continue;
                    }
                };
                let error = futures::executor::block_on(lashlang::execute(
                    &program,
                    &mut State::new(),
                    &Host::default(),
                ))
                .expect_err(&format!("{kind}:{name} probe `{probe}` must refuse"));
                assert!(
                    error
                        .to_string()
                        .starts_with(&format!("validation failed: {expected}:"))
                        || error.to_string().starts_with(&format!("{expected}:")),
                    "{kind}:{name} claims {expected} but `{probe}` fails with {error}"
                );
            }
        }
        probed += 1;
    }
    assert!(
        probed >= 70,
        "the probe coverage ratchet slipped: only {probed} rows are probed"
    );
    assert!(
        exempt <= 1,
        "{exempt} rows claim exemption; each one is a claim nothing checks"
    );
}

fn run_harnessed(includes: &[&str], body: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let mut source = String::new();
    for include in ["sta.js", "assert.js"].iter().chain(includes) {
        source.push_str(
            &ingest::harness_shim(include).unwrap_or_else(|| panic!("{include} has a shim")),
        );
        source.push('\n');
    }
    source.push_str(body);
    let program = runner::admit(&source)
        .unwrap_or_else(|error| panic!("the harness with {includes:?} must be admitted: {error}"));
    futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &Host::default(),
    ))
}

/// Every harness rendering compiles and runs on its own, so a refusal the
/// runner records is the test's, never the harness's.
#[test]
fn every_harness_rendering_compiles_and_runs() {
    let shims = std::fs::read_dir(data_path("harness-shim"))
        .expect("read harness-shim")
        .map(|entry| {
            entry
                .expect("harness-shim entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .filter(|name| name.ends_with(".js") && name != "sta.js" && name != "assert.js")
        .collect::<BTreeSet<_>>();
    for shim in &shims {
        let includes: &[&str] = if shim == "asyncHelpers.js" {
            &["doneprintHandle.js", "asyncHelpers.js"]
        } else {
            &[shim.as_str()]
        };
        assert_eq!(
            run_harnessed(includes, "finish(true);").expect("the harness runs"),
            ExecutionOutcome::Finished(Value::Bool(true)),
            "{shim}"
        );
    }
    let upstream = std::fs::read_dir(data_path("harness"))
        .expect("read harness")
        .map(|entry| {
            entry
                .expect("harness entry")
                .file_name()
                .into_string()
                .expect("UTF-8 name")
        })
        .collect::<BTreeSet<_>>();
    let unshimmable = runner::unshimmable_includes();
    for include in &upstream {
        assert!(
            ingest::harness_shim(include).is_some() || unshimmable.contains_key(include),
            "vendored harness/{include} needs a rendering or an unshimmable.tsv row"
        );
    }
}

/// The assertion harness fails as Test262 does: a failed assertion throws a
/// `Test262Error`, and `assert.throws` accepts exactly the named class.
#[test]
fn assertion_harness_keeps_upstream_semantics() {
    let thrown_name = |body: &str| match run_harnessed(&[], body) {
        Err(RuntimeError::UncaughtException { value }) => value
            .as_record()
            .and_then(|record| record.get("name").cloned())
            .unwrap_or(value),
        other => panic!("`{body}` must throw, got {other:?}"),
    };
    for body in [
        r#"assert["sameValue"](1, 2, "m");"#,
        r#"assert["sameValue"](0, -0);"#,
        r#"assert["notSameValue"](NaN, NaN);"#,
        r#"__test262Assert(1, "truthy is not true");"#,
        r#"assert["compareArray"]([1, 2], [1, 3]);"#,
        r#"assert["throws"]("TypeError", function () {});"#,
        r#"assert["throws"]("TypeError", function () { throw new RangeError("r"); });"#,
        r#"assert["throws"]("Test262Error", function () { throw new TypeError("t"); });"#,
        r#"verifyProperty({ a: 1 }, "a", { value: 2 });"#,
    ] {
        let includes: &[&str] = if body.starts_with("verify") {
            &["propertyHelper.js"]
        } else {
            &[]
        };
        let source = if includes.is_empty() {
            body.to_owned()
        } else {
            format!(
                "{}\n{body}",
                ingest::harness_shim("propertyHelper.js").expect("propertyHelper shim")
            )
        };
        assert_eq!(
            thrown_name(&source),
            Value::String("Test262Error".into()),
            "{body}"
        );
    }
    for body in [
        r#"assert["sameValue"](NaN, NaN); assert["notSameValue"](0, -0); __test262Assert(true);"#,
        r#"assert["compareArray"]([1, NaN], [1, NaN]);"#,
        r#"assert["throws"]("TypeError", function () { throw new TypeError("t"); });"#,
        r#"assert["throws"]("Test262Error", function () { throw Test262Error("m"); });"#,
        r#"assert["throws"]("Test262Error", function () { __test262ErrorThrower("m"); });"#,
    ] {
        assert_eq!(
            run_harnessed(&[], &format!("{body}\nfinish(true);")).expect("the assertions hold"),
            ExecutionOutcome::Finished(Value::Bool(true)),
            "{body}"
        );
    }
    assert_eq!(
        run_harnessed(
            &["propertyHelper.js"],
            r#"verifyProperty({ a: 1 }, "a", { value: 1, writable: true, enumerable: true, configurable: true }); finish(true);"#
        )
        .expect("a plain data property verifies"),
        ExecutionOutcome::Finished(Value::Bool(true))
    );
}

/// The ratchet's three refusals, over one recorded file: a recorded pass that
/// now fails, a recorded failure that now passes (a pass must be promoted),
/// and a refusal whose code changed each fail the comparison; a failure that
/// still fails, under any evidence, does not.
#[test]
fn the_ratchet_refuses_new_failures_unpromoted_passes_and_changed_refusals() {
    use runner::Observed;
    let recorded = [
        ("a.js", Outcome::Pass),
        ("b.js", Outcome::Fail("FIG-1".to_owned())),
        ("c.js", Outcome::Refused("TS_NEW_UNSUPPORTED".to_owned())),
        ("d.js", Outcome::Fail("FIG-2".to_owned())),
    ]
    .into_iter()
    .map(|(path, outcome)| (path.to_owned(), outcome))
    .collect::<BTreeMap<_, _>>();
    let paths = recorded.keys().cloned().collect::<Vec<_>>();
    let observed = [
        Observed::Diverged("assertion".to_owned()),
        Observed::Pass,
        Observed::Refused("TS_CLASS_UNSUPPORTED".to_owned(), String::new()),
        Observed::Diverged("another assertion".to_owned()),
    ];
    let mismatches = runner::compare(&paths, &observed, &recorded);
    assert_eq!(mismatches.len(), 3, "{mismatches:#?}");
    assert!(
        mismatches[0].starts_with("a.js: recorded `pass -`"),
        "{}",
        mismatches[0]
    );
    assert!(
        mismatches[1].starts_with("b.js: recorded `fail FIG-1`, observed pass"),
        "{}",
        mismatches[1]
    );
    assert!(
        mismatches[2].starts_with("c.js: recorded `refused TS_NEW_UNSUPPORTED`"),
        "{}",
        mismatches[2]
    );
    assert_eq!(
        Observed::Diverged(String::new()).outcome(Some(&Outcome::Pass)),
        Outcome::Fail("UNTRIAGED".to_owned()),
        "a blessed new failure has no owner until a ticket takes it"
    );
}
