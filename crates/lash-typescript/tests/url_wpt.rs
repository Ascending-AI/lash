use std::collections::BTreeMap;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};
use serde_json::Value as JsonValue;

const URL_TESTS: &str = include_str!("wpt-url/urltestdata.json");
const SETTER_TESTS: &str = include_str!("wpt-url/setters_tests.json");
const PERCENT_ENCODING_TESTS: &str = include_str!("wpt-url/percent-encoding.json");
const SKIPS: &str = include_str!("wpt-url/skips.tsv");

// Ratchets: a WPT update or a changed skip register must update these numbers
// deliberately. An uncensused row is never silently ignored.
const URL_ROW_COUNT: usize = 891;
const SETTER_ROW_COUNT: usize = 278;
const PERCENT_ENCODING_ROW_COUNT: usize = 7;
const URL_PASS_COUNT: usize = 525;
const SETTER_PASS_COUNT: usize = 145;
const URL_SKIP_COUNT: usize = 366;
const SETTER_SKIP_COUNT: usize = 133;
const SKIP_COUNT: usize = URL_SKIP_COUNT + SETTER_SKIP_COUNT;

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unexpected WPT URL ability")),
        }
    }
}

fn run(source: &str) -> Result<Value, String> {
    let program = lash_typescript::compile(source).map_err(|error| error.to_string())?;
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .map_err(|error| error.to_string())?
    {
        ExecutionOutcome::Finished(value) => Ok(value),
        other => Err(format!("expected finish, got {other:?}")),
    }
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).expect("WPT strings serialize as JavaScript literals")
}

fn skips() -> BTreeMap<(&'static str, String), &'static str> {
    let mut entries = BTreeMap::new();
    for line in SKIPS.lines().skip(1).filter(|line| !line.is_empty()) {
        let columns = line.splitn(3, '\t').collect::<Vec<_>>();
        assert_eq!(columns.len(), 3, "malformed URL WPT skip row: {line}");
        entries.insert((columns[0], columns[1].to_string()), columns[2]);
    }
    assert_eq!(entries.len(), SKIP_COUNT, "URL WPT skip ratchet changed");
    entries
}

fn string_array(value: Value) -> Vec<String> {
    let Value::List(values) = value else {
        panic!("WPT result must be an array, got {value:?}");
    };
    values
        .iter()
        .map(|value| match value {
            Value::String(value) => value.to_string(),
            other => panic!("WPT result member must be a string, got {other:?}"),
        })
        .collect()
}

#[test]
fn canonical_urltestdata_runs_through_authored_typescript() {
    let rows: Vec<JsonValue> = serde_json::from_str(URL_TESTS).expect("valid WPT URL JSON");
    let tests = rows
        .iter()
        .filter_map(JsonValue::as_object)
        .collect::<Vec<_>>();
    assert_eq!(tests.len(), URL_ROW_COUNT);
    let skips = skips();
    let mut failures = Vec::new();
    let properties = [
        "href", "protocol", "username", "password", "host", "hostname", "port", "pathname",
        "search", "hash",
    ];
    let mut passed = 0;
    for (index, row) in tests.into_iter().enumerate() {
        let input = row["input"].as_str().expect("WPT input string");
        let base = row.get("base").and_then(JsonValue::as_str);
        let case = index.to_string();
        let source = format!(
            "const u = new URL({}, {}); finish([{}]);",
            js_string(input),
            base.map_or_else(|| "undefined".to_string(), js_string),
            properties
                .iter()
                .map(|property| format!("u.{property}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let outcome = run(&source);
        let failure_expected = row
            .get("failure")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let result = if failure_expected {
            outcome
                .err()
                .map(|_| ())
                .ok_or_else(|| "expected URL parse failure, but execution succeeded".to_string())
        } else {
            outcome.and_then(|value| {
                let actual = string_array(value);
                let expected = properties
                    .iter()
                    .map(|property| {
                        row[*property]
                            .as_str()
                            .expect("successful WPT row has every URL property")
                            .to_string()
                    })
                    .collect::<Vec<_>>();
                (actual == expected)
                    .then_some(())
                    .ok_or_else(|| format!("expected {expected:?}, got {actual:?}"))
            })
        };
        match (result, skips.get(&("urltestdata", case.clone()))) {
            (Ok(()), None) => passed += 1,
            (Err(_), Some(_)) => {}
            (Ok(()), Some(reason)) => failures.push(format!(
                "urltestdata row {case} unexpectedly passes; remove skip: {reason}"
            )),
            (Err(error), None) => {
                failures.push(format!("urltestdata row {case} input {input:?}: {error}"))
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(passed, URL_PASS_COUNT, "URL WPT pass ratchet changed");
    assert_eq!(URL_PASS_COUNT + URL_SKIP_COUNT, URL_ROW_COUNT);
}

#[test]
fn canonical_setter_tests_run_through_authored_typescript() {
    let groups: JsonValue = serde_json::from_str(SETTER_TESTS).expect("valid WPT setter JSON");
    let groups = groups.as_object().expect("WPT setter groups object");
    let skips = skips();
    let properties = [
        "href", "protocol", "username", "password", "host", "hostname", "port", "pathname",
        "search", "hash",
    ];
    let mut row_count = 0;
    let mut passed = 0;
    let mut failures = Vec::new();
    for (setter, rows) in groups {
        let Some(rows) = rows.as_array() else {
            continue;
        };
        if setter == "comment" {
            continue;
        }
        for (index, row) in rows.iter().enumerate() {
            row_count += 1;
            let href = row["href"].as_str().expect("setter href");
            let value = row["new_value"].as_str().expect("setter value");
            let case = format!("{setter}:{index}");
            let source = format!(
                "const u = new URL({}); u.{} = {}; finish([{}]);",
                js_string(href),
                setter,
                js_string(value),
                properties
                    .iter()
                    .map(|property| format!("u.{property}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let expected = row["expected"].as_object().expect("setter expected object");
            let result = run(&source).and_then(|value| {
                let actual = string_array(value);
                for (property_index, property) in properties.iter().enumerate() {
                    if let Some(expected) = expected.get(*property).and_then(JsonValue::as_str)
                        && actual[property_index] != expected
                    {
                        return Err(format!(
                            "{property}: expected {expected:?}, got {:?}",
                            actual[property_index]
                        ));
                    }
                }
                Ok(())
            });
            match (result, skips.get(&("setters", case.clone()))) {
                (Ok(()), None) => passed += 1,
                (Err(_), Some(_)) => {}
                (Ok(()), Some(reason)) => failures.push(format!(
                    "setter row {case} unexpectedly passes; remove skip: {reason}"
                )),
                (Err(error), None) => {
                    failures.push(format!("setter row {case} href {href:?}: {error}"))
                }
            }
        }
    }
    assert_eq!(row_count, SETTER_ROW_COUNT);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(
        passed, SETTER_PASS_COUNT,
        "URL setter WPT pass ratchet changed"
    );
    assert_eq!(SETTER_PASS_COUNT + SETTER_SKIP_COUNT, SETTER_ROW_COUNT);
}

#[test]
fn percent_encoding_fixture_runs_every_utf8_row_through_the_url_setter() {
    let rows: Vec<JsonValue> =
        serde_json::from_str(PERCENT_ENCODING_TESTS).expect("valid WPT percent JSON");
    let tests = rows
        .iter()
        .filter_map(JsonValue::as_object)
        .collect::<Vec<_>>();
    assert_eq!(tests.len(), PERCENT_ENCODING_ROW_COUNT);
    for (index, row) in tests.into_iter().enumerate() {
        let input = row["input"].as_str().expect("percent input string");
        let expected = row["output"]["utf-8"]
            .as_str()
            .expect("percent UTF-8 output string");
        let source = format!(
            "const u = new URL('https://example.test/'); u.search = {}; finish(u.search.substring(1));",
            js_string(input)
        );
        assert_eq!(
            run(&source).unwrap_or_else(|error| panic!("percent row {index} failed: {error}")),
            Value::String(expected.into()),
            "percent row {index} input {input:?}"
        );
    }
    // URLSearchParams has the distinct application/x-www-form-urlencoded
    // encode set (including space-to-plus), so pin it separately.
    assert_eq!(
        run("const p = new URLSearchParams(); p.set('q', '† á|'); finish(p.toString());")
            .expect("URLSearchParams form encoding"),
        Value::String("q=%E2%80%A0+%C3%A1%7C".into())
    );
}
