//! Every refusal the deviation register promises is executable (FIG-3604).
//!
//! ADR 0062 register entry 5 promised that mutable captures are "rejected on
//! both the read and the write path", and only the write path was: a closure
//! reading a `let` reassigned after it was created silently answered a stale
//! value. Nothing connected the promise to the code, so nothing noticed. The
//! Test262 census already closes that gap for rejected features, where each
//! rejected row carries a probe that must reject with the diagnostic it names;
//! this is the same rule for the register.
//!
//! Two texts are read. ADR 0062's numbered register is the decision list, and
//! the crate README's register is its executable counterpart. The rules:
//!
//! - Every ADR entry that says it refuses something — *reject*, *refuse*, or
//!   *fail closed*, in any inflection — and is not retired has at least one
//!   probe, and every `TS_*` code the entry names is the refusal of one of its
//!   probes.
//! - Every `TS_*` code a README register item names, where that item says it
//!   refuses something, is the refusal of some probe.
//! - Every probe names a live ADR entry or none, and every probe fires: its
//!   source is refused while it compiles with exactly its code, or it compiles
//!   and then fails while running with that refusal — a runtime error's own
//!   code, or the `TS_*:` prefix the runtime puts on the dialect refusals it
//!   raises.
//!
//! A new register entry that refuses without a probe fails here, and so does a
//! probe whose refusal stops firing.

use std::collections::BTreeSet;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

const ADR: &str =
    include_str!("../../../docs/adr/0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md");
const README: &str = include_str!("../README.md");

/// How a probe's source is obtained and where it runs.
enum Source {
    Text(&'static str),
    /// A source too large to write inline.
    Generated(fn() -> String),
    /// Parsed under a 2 GiB address-space limit, in a child process.
    UnderAddressLimit(fn() -> String),
    /// Linked the way a cell is admitted, for a refusal the linker makes.
    Linked(&'static str),
}

struct Probe {
    /// The ADR 0062 register entry the probe proves, if any.
    entry: Option<u32>,
    /// The refusal that must fire: a `TS_*` diagnostic, or a runtime error
    /// code.
    refusal: &'static str,
    source: Source,
}

const fn probe(entry: u32, refusal: &'static str, source: &'static str) -> Probe {
    Probe {
        entry: Some(entry),
        refusal,
        source: Source::Text(source),
    }
}

const fn linked_probe(entry: u32, refusal: &'static str, source: &'static str) -> Probe {
    Probe {
        entry: Some(entry),
        refusal,
        source: Source::Linked(source),
    }
}

const fn readme_probe(refusal: &'static str, source: &'static str) -> Probe {
    Probe {
        entry: None,
        refusal,
        source: Source::Text(source),
    }
}

fn oversized_source() -> String {
    format!(
        "const x = '{}'; finish(x);",
        "a".repeat(lash_typescript::MAX_SOURCE_BYTES)
    )
}

fn cap_sized_source() -> String {
    format!(
        "const x = '{}';finish(x);",
        "a".repeat(lash_typescript::MAX_SOURCE_BYTES - 32)
    )
}

fn repeated_label_source() -> String {
    format!("{}1;", "a:".repeat(4 * 1024))
}

fn nested_source() -> String {
    format!("finish({}1{});", "(".repeat(64), ")".repeat(64))
}

fn long_regex_source() -> String {
    format!("/{}/;", "a".repeat(4_097))
}

fn deep_regex_source() -> String {
    format!("/{}a{}/;", "(".repeat(33), ")".repeat(33))
}

const PROBES: &[Probe] = &[
    // 2. Effects in builtin callbacks: an `await` inside one is a parse-level
    // rejection.
    probe(
        2,
        "TS_SYNTAX_ERROR",
        "const r = [1].map((x) => { await sleep(1); return x; });",
    ),
    // 4. Cycles at durable capture.
    probe(
        4,
        "TS_CYCLIC_VALUE_UNSUPPORTED",
        "const node: any = {}; node.self = node; finish(1);",
    ),
    // 5. Mutable captures, on the read path and on the write path.
    probe(
        5,
        "TS_MUTABLE_CAPTURE_UNSUPPORTED",
        "let n = 0; const f = () => n; n = 1; finish(f());",
    ),
    probe(
        5,
        "TS_MUTABLE_CAPTURE_UNSUPPORTED",
        "let n = 0; const f = () => { n = 1; }; f(); finish(n);",
    ),
    // 6. Mutual recursion.
    probe(
        6,
        "TS_MUTUAL_RECURSION_UNSUPPORTED",
        "function isEven(n: number): boolean { return n === 0 ? true : isOdd(n - 1); } function isOdd(n: number): boolean { return n === 0 ? false : isEven(n - 1); } finish(isEven(2));",
    ),
    // 8. Lone surrogates: the literal, and the four runtime paths.
    probe(
        8,
        "TS_LONE_SURROGATE_LITERAL_UNSUPPORTED",
        "finish('\\uD800');",
    ),
    probe(8, "TS_LONE_SURROGATE_UNSUPPORTED", "finish('😀'[0]);"),
    probe(
        8,
        "TS_LONE_SURROGATE_UNSUPPORTED",
        "finish(Object.values('😀'));",
    ),
    probe(
        8,
        "TS_LONE_SURROGATE_UNSUPPORTED",
        "finish('😀'.split(''));",
    ),
    probe(
        8,
        "TS_LONE_SURROGATE_UNSUPPORTED",
        "finish('😀'.replaceAll('', '-'));",
    ),
    // 9. Source size and address space.
    Probe {
        entry: Some(9),
        refusal: "TS_SOURCE_TOO_LARGE",
        source: Source::Generated(oversized_source),
    },
    Probe {
        entry: Some(9),
        refusal: "TS_PARSE_RESOURCES_UNAVAILABLE",
        source: Source::UnderAddressLimit(cap_sized_source),
    },
    // 10. The preflight rejects the shapes whose parse would allocate without
    // bound, such as one repeated label.
    Probe {
        entry: Some(10),
        refusal: "TS_SOURCE_NESTING_LIMIT",
        source: Source::Generated(repeated_label_source),
    },
    // 11. The nesting budget.
    Probe {
        entry: Some(11),
        refusal: "TS_SOURCE_NESTING_LIMIT",
        source: Source::Generated(nested_source),
    },
    // 12. Dense arrays.
    probe(
        12,
        "TS_SPARSE_ARRAY_UNSUPPORTED",
        "const a = [1]; a[3] = 9; finish(a.length);",
    ),
    probe(
        12,
        "TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED",
        "const a: any = [1]; a[-1] = 9; finish(a.length);",
    ),
    // 13. String coercion of a value whose only string is a type tag.
    probe(13, "TS_OBJECT_STRING_COERCION", "finish('' + { a: 1 });"),
    // 16. `for...of` snapshots, and a classic-loop `continue` across a
    // `finally`.
    probe(
        16,
        "TS_FOR_OF_UNSUPPORTED",
        "const xs = [1]; for (const x of xs) { xs.push(x); } finish(xs.length);",
    ),
    probe(
        16,
        "TS_FOR_UNSUPPORTED",
        "for (let i = 0; i < 2; i++) { try { continue; } finally { } } finish(1);",
    ),
    // 22. The closed-shape field guard, on the read path and the write path.
    linked_probe(
        22,
        "TS_LINK_ERROR",
        "const point = { x: 1, y: 2 }; finish(point.z);",
    ),
    linked_probe(
        22,
        "TS_LINK_ERROR",
        "const point = { x: 1 }; point.y = 2; finish(point.x);",
    ),
    // The README register's refusals that have no numbered ADR entry.
    readme_probe(
        "TS_PROTOTYPE_MUTATION_UNSUPPORTED",
        "const o: any = {}; o.__proto__ = {};",
    ),
    // A process body has no live view of session state (FIG-3620).
    readme_probe(
        "TS_NON_LIFTABLE_CAPTURE",
        "var budget = 1; const spend = async () => globalThis.budget;",
    ),
    Probe {
        entry: None,
        refusal: "TS_REGEX_PATTERN_TOO_LONG",
        source: Source::Generated(long_regex_source),
    },
    Probe {
        entry: None,
        refusal: "TS_REGEX_PATTERN_NESTING_LIMIT",
        source: Source::Generated(deep_regex_source),
    },
    readme_probe(
        "TS_REGEX_INDICES_FLAG_UNSUPPORTED",
        "finish(/a/d.test('a'));",
    ),
    readme_probe(
        "TS_REGEX_UNICODE_SETS_FLAG_UNSUPPORTED",
        "finish(/a/v.test('a'));",
    ),
    readme_probe(
        "TS_REGEX_LONE_SURROGATE_MATCH_UNSUPPORTED",
        "finish('😀'.match(/./));",
    ),
    readme_probe(
        "TS_REGEX_ITERATOR_POSITION",
        "const matches = 'aa'.matchAll(/a/g); finish(1);",
    ),
    readme_probe(
        "TS_DELETE_ARRAY_INDEX_UNSUPPORTED",
        "const a = [1, 2]; delete a[0]; finish(a.length);",
    ),
    readme_probe(
        "TS_DATE_PARSE_NON_ISO",
        "finish(Date.parse('March 7, 2024'));",
    ),
    readme_probe(
        "TS_DATE_IMMUTABLE",
        "const d = new Date(0); d.setUTCFullYear(2020); finish(1);",
    ),
    readme_probe(
        "TS_DATE_STRING_COERCION_PENDING",
        "finish('' + new Date(0));",
    ),
    readme_probe(
        "TS_URL_SCHEME_UNSUPPORTED",
        "finish(new URL('file:///tmp/x').href);",
    ),
    readme_probe(
        "TS_URL_IDNA_BACKING_DIVERGENCE",
        "const u = new URL('https://a.test'); u.hostname = 'xn--'; finish(u.href);",
    ),
    readme_probe(
        "TS_URL_PERCENT_ENCODING_BACKING_DIVERGENCE",
        "finish(new URL('https://a.test/^').href);",
    ),
    readme_probe(
        "TS_URL_RELATIVE_SLASH_BACKING_DIVERGENCE",
        "finish(new URL('///x', 'https://a.test').href);",
    ),
    readme_probe(
        "TS_URL_SETTER_BACKING_DIVERGENCE",
        "const u = new URL('https://a.test:8080'); u.port = '\\t'; finish(u.href);",
    ),
    readme_probe("TS_URL_PARSE_ERROR", "finish(new URL('not a url').href);"),
];

/// A numbered ADR 0062 register entry.
struct Entry {
    number: u32,
    text: String,
}

impl Entry {
    fn retired(&self) -> bool {
        self.text.contains("retired by")
    }
}

/// The body of the `## Deviation register` section of `document`.
fn register_section(document: &str) -> &str {
    document
        .split("\n## Deviation register\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("the document has a deviation register section")
}

fn adr_entries() -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in register_section(ADR).lines() {
        let numbered = line
            .split_once(". ")
            .and_then(|(number, rest)| Some((number.parse::<u32>().ok()?, rest)));
        if let Some((number, rest)) = numbered {
            entries.push(Entry {
                number,
                text: rest.to_string(),
            });
        } else if line.starts_with(' ')
            && let Some(entry) = entries.last_mut()
        {
            entry.text.push(' ');
            entry.text.push_str(line.trim());
        }
    }
    let numbers = entries.iter().map(|entry| entry.number).collect::<Vec<_>>();
    assert_eq!(
        numbers,
        (1..=u32::try_from(entries.len()).expect("a small register")).collect::<Vec<_>>(),
        "the ADR register is numbered from 1 without gaps"
    );
    entries
}

/// The README register's items, each one bullet with its continuation lines.
fn readme_items() -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut open = false;
    for line in register_section(README).lines() {
        if let Some(rest) = line.strip_prefix("- ") {
            items.push(rest.to_string());
            open = true;
        } else if open && line.starts_with("  ") {
            if let Some(item) = items.last_mut() {
                item.push(' ');
                item.push_str(line.trim());
            }
        } else if !line.trim().is_empty() {
            open = false;
        }
    }
    assert!(items.len() > 10, "the README register lists its deviations");
    items
}

fn says_it_refuses(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("fails closed")
        || lower.contains("fail closed")
        || lower
            .split(|character: char| !character.is_ascii_alphabetic())
            .any(|word| {
                matches!(
                    word,
                    "reject"
                        | "rejects"
                        | "rejected"
                        | "rejection"
                        | "refuse"
                        | "refuses"
                        | "refused"
                        | "refusal"
                )
            })
}

/// Every `TS_*` code `text` names.
fn named_codes(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|token| {
            token
                .strip_prefix("TS_")
                .is_some_and(|rest| rest.chars().any(|character| character.is_ascii_uppercase()))
        })
        .map(str::to_string)
        .collect()
}

#[test]
fn every_refusing_register_entry_has_a_probe() {
    let entries = adr_entries();
    let live = entries
        .iter()
        .filter(|entry| !entry.retired())
        .map(|entry| entry.number)
        .collect::<BTreeSet<_>>();
    for probe in PROBES {
        if let Some(entry) = probe.entry {
            assert!(
                live.contains(&entry),
                "a {} probe names ADR 0062 register entry {entry}, which is not a live entry",
                probe.refusal
            );
        }
    }

    let mut missing = Vec::new();
    let mut refusing = 0;
    for entry in entries
        .iter()
        .filter(|entry| !entry.retired() && says_it_refuses(&entry.text))
    {
        refusing += 1;
        let probed = PROBES
            .iter()
            .filter(|probe| probe.entry == Some(entry.number))
            .map(|probe| probe.refusal.to_string())
            .collect::<BTreeSet<_>>();
        if probed.is_empty() {
            missing.push(format!("entry {}: no probe", entry.number));
        }
        for code in named_codes(&entry.text).difference(&probed) {
            missing.push(format!("entry {}: no probe fires {code}", entry.number));
        }
    }
    // A vocabulary check that matched nothing would pass vacuously.
    assert!(
        refusing >= 10,
        "only {refusing} ADR register entries read as refusals; the vocabulary check has stopped matching"
    );

    let probed = PROBES
        .iter()
        .map(|probe| probe.refusal.to_string())
        .collect::<BTreeSet<_>>();
    for item in readme_items().iter().filter(|item| says_it_refuses(item)) {
        for code in named_codes(item).difference(&probed) {
            missing.push(format!("README register: no probe fires {code}"));
        }
    }
    assert!(
        missing.is_empty(),
        "the deviation register promises refusals nothing proves:\n{}",
        missing.join("\n")
    );
}

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new(
                "unexpected deviation-probe ability",
            )),
        }
    }
}

/// What a probe's source did, as a refusal code and its rendering, or `None`
/// if it compiled and ran to completion.
fn fire(source: &str) -> Option<(String, String)> {
    let program = match lash_typescript::testing::compile(source) {
        Ok(program) => program,
        Err(diagnostic) => {
            return Some((diagnostic.code.as_str().to_string(), diagnostic.to_string()));
        }
    };
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host)) {
        Ok(ExecutionOutcome::Finished(_)) => None,
        Ok(other) => Some(("<unfinished>".to_string(), format!("{other:?}"))),
        Err(error) => Some((error.code().to_string(), error.to_string())),
    }
}

fn fires(refusal: &str, source: &str) -> Result<(), String> {
    match fire(source) {
        None => Err("compiled and ran to completion".to_string()),
        Some((code, rendered)) if code == refusal || rendered.contains(&format!("{refusal}:")) => {
            Ok(())
        }
        Some((code, rendered)) => Err(format!("refused as {code}: {rendered}")),
    }
}

/// Whether linking `source` as a cell is refused with `refusal`.
fn link_fires(refusal: &str, source: &str) -> Result<(), String> {
    match lash_typescript::link(source, &lashlang::testing::harness::test_environment()) {
        Ok(_) => Err("linked".to_string()),
        Err(diagnostic) if diagnostic.code.as_str() == refusal => Ok(()),
        Err(diagnostic) => Err(format!(
            "refused as {}: {diagnostic}",
            diagnostic.code.as_str()
        )),
    }
}

#[test]
fn every_register_probe_fires_its_refusal() {
    let mut failures = Vec::new();
    for probe in PROBES {
        let source = match &probe.source {
            Source::Text(source) => (*source).to_string(),
            Source::Generated(build) => build(),
            Source::UnderAddressLimit(_) => continue,
            Source::Linked(source) => {
                if let Err(outcome) = link_fires(probe.refusal, source) {
                    failures.push(format!("{} probe `{source}`: {outcome}", probe.refusal));
                }
                continue;
            }
        };
        if let Err(outcome) = fires(probe.refusal, &source) {
            let shown = source.chars().take(120).collect::<String>();
            failures.push(format!("{} probe `{shown}`: {outcome}", probe.refusal));
        }
    }
    assert!(
        failures.is_empty(),
        "register probes that do not fire their refusal:\n{}",
        failures.join("\n")
    );
}

/// The address-space refusal needs the host to be short of address space, so
/// it runs in a child under `ulimit -v`, as the parser's own guard test does.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "test code: the probe re-runs this test binary in a child under `ulimit -v`, which needs the executable path, a marker variable and a spawned shell"
)]
fn register_probes_under_an_address_limit_fire_their_refusal() {
    const CHILD_ENV: &str = "LASH_TS_DEVIATION_RLIMIT_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        for probe in PROBES {
            let Source::UnderAddressLimit(build) = &probe.source else {
                continue;
            };
            let error = lash_typescript::validate(&build())
                .expect_err("the reservation cannot be met under this limit");
            assert_eq!(error.code.as_str(), probe.refusal, "{error}");
        }
        return;
    }
    let executable = std::env::current_exe().expect("test executable");
    let command = format!(
        "ulimit -v 2097152 && exec {} deviation_register::register_probes_under_an_address_limit_fire_their_refusal --exact --nocapture",
        executable.display()
    );
    let output = std::process::Command::new("bash")
        .args(["-c", &command])
        .env(CHILD_ENV, "1")
        .output()
        .expect("the limited child starts");
    assert!(
        output.status.success(),
        "the address-space-limited child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
