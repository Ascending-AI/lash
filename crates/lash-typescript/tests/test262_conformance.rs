use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use lash_typescript::DiagnosticCode;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionBound, ExecutionBounds, ExecutionEnvironment, ExecutionHost,
    ExecutionHostError, ExecutionOutcome, RuntimeError, State, Value,
};

#[path = "test262/support/metadata.rs"]
mod metadata;

use metadata::{ErrorType, Phase, TestFlag};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/test262");

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestEntry {
    path: String,
    area: String,
    disposition: String,
    expectation: String,
}

#[derive(Default)]
struct Host {
    prints: Mutex<Vec<Value>>,
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(value) => {
                self.prints.lock().expect("print journal").push(value);
                Ok(AbilityResult::Value(Value::Null))
            }
            _ => Err(ExecutionHostError::new("unexpected Test262 ability")),
        }
    }
}

fn data_path(relative: &str) -> PathBuf {
    Path::new(ROOT).join(relative)
}

fn data_lines(relative: &str, columns: usize) -> Vec<Vec<String>> {
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

fn manifest() -> Vec<ManifestEntry> {
    data_lines("manifest.tsv", 4)
        .into_iter()
        .map(|fields| ManifestEntry {
            path: fields[0].clone(),
            area: fields[1].clone(),
            disposition: fields[2].clone(),
            expectation: fields[3].clone(),
        })
        .collect()
}

fn diagnostic_names() -> BTreeSet<&'static str> {
    DiagnosticCode::ALL
        .iter()
        .map(|code| code.as_str())
        .collect()
}

#[test]
fn inventory_census_and_skip_register_are_exhaustive() {
    let inventory = data_lines("inventory.tsv", 2)
        .into_iter()
        .map(|fields| (fields[0].clone(), fields[1].clone()))
        .collect::<BTreeSet<_>>();
    let census_rows = data_lines("census.tsv", 4);
    let census = census_rows
        .iter()
        .map(|fields| (fields[0].clone(), fields[1].clone()))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        census, inventory,
        "the Test262 census must have no gap or extra row"
    );
    assert_eq!(census.len(), census_rows.len(), "duplicate census row");

    let diagnostic_names = diagnostic_names();
    for fields in census_rows {
        match fields[2].as_str() {
            "accepted" => assert_eq!(fields[3], "-", "accepted row must have reason `-`"),
            "rejected" => assert!(
                diagnostic_names.contains(fields[3].as_str()),
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
    }

    let entries = manifest();
    let selected_paths = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        selected_paths.len(),
        entries.len(),
        "duplicate manifest path"
    );
    let passing_paths = entries
        .iter()
        .filter(|entry| entry.disposition == "pass")
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();

    let skip_rows = data_lines("skip-register.tsv", 2);
    let skipped_paths = skip_rows
        .iter()
        .map(|fields| fields[0].as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        skipped_paths.len(),
        skip_rows.len(),
        "duplicate skip-register path"
    );
    assert!(
        passing_paths.is_disjoint(&skipped_paths),
        "passing paths cannot be in the skip register"
    );
    for fields in skip_rows
        .iter()
        .filter(|fields| fields[0].ends_with("#strict"))
    {
        assert_eq!(
            fields[1], "strict-mode-variant:n.a.",
            "strict variants have one explicit reason"
        );
        assert!(
            passing_paths.contains(fields[0].trim_end_matches("#strict")),
            "only a selected passing path can own a strict-variant skip"
        );
    }
    for entry in entries.iter().filter(|entry| entry.disposition == "skip") {
        assert!(
            skipped_paths.contains(entry.path.as_str()),
            "ratcheted skip {} is missing from the exhaustive register",
            entry.path
        );
    }

    let upstream_count = std::fs::read_to_string(data_path("upstream-test-count.txt"))
        .expect("read upstream Test262 count")
        .trim()
        .parse::<usize>()
        .expect("upstream Test262 count is a number");
    assert_eq!(
        skip_rows
            .iter()
            .filter(|fields| !fields[0].ends_with("#strict"))
            .count()
            + passing_paths.len(),
        upstream_count,
        "every upstream test path must be selected to pass or explicitly skipped"
    );
}

fn load_census() -> BTreeMap<(String, String), (String, String)> {
    data_lines("census.tsv", 4)
        .into_iter()
        .map(|fields| {
            (
                (fields[0].clone(), fields[1].clone()),
                (fields[2].clone(), fields[3].clone()),
            )
        })
        .collect()
}

fn supply_assertion_message(source: String, callee: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut remaining = source.as_str();
    while let Some(start) = remaining.find(callee) {
        let arguments_start = start + callee.len();
        output.push_str(&remaining[..arguments_start]);
        let bytes = remaining.as_bytes();
        let mut stack = vec![b'('];
        let mut quote = None;
        let mut escaped = false;
        let mut commas = 0;
        let mut end = arguments_start;
        for (offset, byte) in bytes[arguments_start..].iter().copied().enumerate() {
            end = arguments_start + offset;
            if let Some(active_quote) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == active_quote {
                    quote = None;
                }
                continue;
            }
            match byte {
                b'\'' | b'"' | b'`' => quote = Some(byte),
                b'(' | b'[' | b'{' => stack.push(byte),
                b',' if stack.len() == 1 => commas += 1,
                b')' => {
                    if stack.pop() == Some(b'(') && stack.is_empty() {
                        break;
                    }
                }
                b']' => {
                    assert_eq!(stack.pop(), Some(b'['), "balanced assertion argument");
                }
                b'}' => {
                    assert_eq!(stack.pop(), Some(b'{'), "balanced assertion argument");
                }
                _ => {}
            }
        }
        assert!(stack.is_empty(), "unterminated Test262 assertion call");
        output.push_str(&remaining[arguments_start..end]);
        if commas == 1 {
            output.push_str(", undefined");
        }
        remaining = &remaining[end..];
    }
    output.push_str(remaining);
    output
}

fn source_for(path: &Path, test_metadata: &metadata::Metadata, finish: bool) -> String {
    let test = std::fs::read_to_string(path).expect("read vendored Test262 test");
    if test_metadata.flags.contains(&TestFlag::Raw) {
        return test;
    }

    // The dialect intentionally distinguishes known method calls from calls
    // through computed function-valued properties. The shim is a plain record,
    // not a production runtime global, so bridge only Test262's assertion
    // namespace to the latter spelling. Vendored tests remain byte-identical.
    let test = test
        .replace("new Test262Error(", "Test262Error(")
        .replace("assert.sameValue", "assert[\"sameValue\"]")
        .replace("assert.notSameValue", "assert[\"notSameValue\"]")
        .replace("assert.compareArray", "assert[\"compareArray\"]");
    let test = [
        "assert[\"sameValue\"](",
        "assert[\"notSameValue\"](",
        "assert[\"compareArray\"](",
    ]
    .into_iter()
    .fold(test, supply_assertion_message);

    let mut source = String::new();
    for harness in ["sta.js", "assert.js", "compareArray.js"] {
        source.push_str(
            &std::fs::read_to_string(data_path(&format!("harness-shim/{harness}")))
                .expect("read Test262 harness shim"),
        );
        source.push('\n');
    }
    for include in test_metadata.includes.iter() {
        if include.as_ref() == "compareArray.js" {
            continue;
        }
        let include_path = data_path(&format!("harness-shim/{include}"));
        source.push_str(
            &std::fs::read_to_string(&include_path).unwrap_or_else(|error| {
                panic!(
                    "{} requires missing harness shim {}: {error}",
                    path.display(),
                    include_path.display()
                )
            }),
        );
        source.push('\n');
    }
    source.push_str(&test);
    if finish {
        source.push_str("\nfinish(true);\n");
    }
    source
}

fn execute_positive(path: &Path, source: &str) -> Result<(), String> {
    let program =
        lash_typescript::compile(source).map_err(|error| format!("{}: {error}", path.display()))?;
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &Host::default(),
    ))
    .map_err(|error| format!("{}: {error}", path.display()))?;
    match outcome {
        ExecutionOutcome::Finished(Value::Bool(true)) => Ok(()),
        other => Err(format!(
            "{}: expected finish(true), got {other:?}",
            path.display()
        )),
    }
}

fn execute_runtime_negative(path: &Path, source: &str, expected: ErrorType) -> Result<(), String> {
    let program = lash_typescript::compile(source).map_err(|error| error.to_string())?;
    match futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &Host::default(),
    )) {
        Err(error) if error.to_string().contains(expected.as_str()) => Ok(()),
        Err(error) => Err(format!(
            "{}: expected runtime {}, got {error}",
            path.display(),
            expected.as_str()
        )),
        Ok(outcome) => Err(format!(
            "{}: expected runtime {}, got {outcome:?}",
            path.display(),
            expected.as_str()
        )),
    }
}

#[test]
fn selected_test262_cases_match_the_ratchet() {
    let entries = manifest();
    let census = load_census();
    let expected_counts = data_lines("expected-counts.tsv", 4)
        .into_iter()
        .map(|fields| {
            let counts = fields[1..]
                .iter()
                .map(|field| field.parse::<usize>().expect("expected count is numeric"))
                .collect::<Vec<_>>();
            (fields[0].clone(), (counts[0], counts[1], counts[2]))
        })
        .collect::<BTreeMap<_, _>>();

    let mut actual_counts = BTreeMap::<String, (usize, usize, usize)>::new();
    let mut failures = Vec::new();
    for entry in &entries {
        let counts = actual_counts.entry(entry.area.clone()).or_default();
        counts.0 += 1;
        match entry.disposition.as_str() {
            "pass" => counts.1 += 1,
            "skip" => counts.2 += 1,
            other => panic!("{} has unknown disposition `{other}`", entry.path),
        }

        let path = data_path(&entry.path);
        let test_metadata = metadata::read_metadata(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", entry.path));
        assert!(
            !test_metadata.description.trim().is_empty(),
            "{} has an empty description",
            entry.path
        );
        if entry.disposition == "pass" {
            for feature in test_metadata.features.iter() {
                let Some((status, reason)) =
                    census.get(&("feature".to_owned(), feature.to_string()))
                else {
                    panic!("{} uses uncensused feature `{feature}`", entry.path);
                };
                assert_eq!(
                    status, "accepted",
                    "{} uses non-accepted feature `{feature}` ({reason})",
                    entry.path
                );
            }
            for forbidden in [
                TestFlag::OnlyStrict,
                TestFlag::Module,
                TestFlag::Async,
                TestFlag::NonDeterministic,
            ] {
                assert!(
                    !test_metadata.flags.contains(&forbidden),
                    "{} has unsupported Test262 flag {forbidden:?}",
                    entry.path
                );
            }
        }

        let result = if entry.disposition == "skip" {
            let source = source_for(&path, &test_metadata, false);
            match lash_typescript::compile(&source) {
                Err(error) if error.code.as_str() == entry.expectation => Ok(()),
                Err(error) => Err(format!(
                    "{}: skip expected {}, got {} ({error})",
                    entry.path,
                    entry.expectation,
                    error.code.as_str()
                )),
                Ok(_) => Err(format!(
                    "{}: skip unexpectedly compiled; promote it and update the count pin",
                    entry.path
                )),
            }
        } else if let Some(negative) = &test_metadata.negative {
            match negative.phase {
                Phase::Parse | Phase::Resolution => match lash_typescript::compile(
                    &std::fs::read_to_string(&path).expect("read negative Test262 test"),
                ) {
                    Err(error)
                        if negative.error_type == ErrorType::SyntaxError
                            && (entry.expectation == "-"
                                || error.code.as_str() == entry.expectation) =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(format!(
                        "{}: expected {:?} {}, got {} ({error})",
                        entry.path,
                        negative.phase,
                        negative.error_type.as_str(),
                        error.code.as_str()
                    )),
                    Ok(_) => Err(format!(
                        "{}: negative test unexpectedly compiled",
                        entry.path
                    )),
                },
                Phase::Runtime => {
                    let source = source_for(&path, &test_metadata, false);
                    execute_runtime_negative(&path, &source, negative.error_type)
                }
            }
        } else {
            let source = source_for(&path, &test_metadata, true);
            execute_positive(&path, &source)
        };
        if let Err(error) = result {
            failures.push(error);
        }
    }

    let totals = actual_counts.values().fold((0, 0, 0), |mut total, counts| {
        total.0 += counts.0;
        total.1 += counts.1;
        total.2 += counts.2;
        total
    });
    actual_counts.insert("TOTAL".to_owned(), totals);
    assert_eq!(actual_counts, expected_counts, "Test262 count pin drifted");
    eprintln!(
        "Test262: selected={}, pass={}, skip={}",
        totals.0, totals.1, totals.2
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
    let program = lash_typescript::compile(source).expect("bound test compiles");
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
    let program = lash_typescript::compile(erased).expect("type-only syntax is erased");
    let outcome = futures::executor::block_on(lashlang::execute(
        &program,
        &mut State::new(),
        &Host::default(),
    ))
    .expect("erased TypeScript program executes");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(1.0)));

    let enum_program = lash_typescript::compile("enum E { A } finish(E.A);")
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
        let error = lash_typescript::compile(source).expect_err("construct stays rejected");
        assert_eq!(error.code, expected, "source: {source}");
    }
}
