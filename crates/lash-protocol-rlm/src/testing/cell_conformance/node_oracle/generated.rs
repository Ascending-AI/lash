//! Generated differential sessions and the snapshot round-trip law (FIG-3608).
//!
//! The [`generator`](super::generator) draws a session from a seed. The
//! bounded corpus is the first [`BOUNDED_SEEDS`] seeds; `generated.json`
//! checks in Node's answer for each of its sessions and for each row of the
//! [`round_trip`](super::round_trip) law, and is written by one deliberate,
//! byte-identical step that asks the pinned Node live:
//!
//! ```console
//! kiln run //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test -- \
//!     --ignored --exact testing::cell_conformance::node_oracle::generated::write_the_generated_corpus
//! ```
//!
//! The cacheable test partition regenerates every session from its seed,
//! requires it to be the one checked in (a generator change is a deliberate
//! corpus change), and runs it through lash, live and reloading between every
//! pair of cells, against Node's answer under the `closure-boundary` rule.
//! Longer runs draw fresh seeds and ask Node live
//! ([`generated_sessions_against_live_node`]); every divergence they find is
//! [`minimize`](super::minimize)d into a session-corpus row, which is the
//! ratchet.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::super::harness::HarnessMode;
use super::generator::{GRAMMAR, GeneratedSession, Grammar, OPEN_DEFECT_EXCLUSIONS, generate};
use super::node::{NodeCell, NodeOracle, NodeSession, sessions_directory};
use super::round_trip::{self, NOT_A_DIALECT_VALUE, ROWS};
use super::{Observation, PINNED_NODE, run_session};

/// The one deliberate step that rewrites `generated.json`, spelled exactly so
/// every drift failure names it. The generator draws its rejected cells from
/// the census's probe list, so a census change can change what a seed draws:
/// regenerate after one.
const REGENERATE: &str = "kiln run //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test -- \
     --ignored --exact testing::cell_conformance::node_oracle::generated::write_the_generated_corpus";

const GENERATED: &str =
    include_str!("../../../../../lash-typescript/tests/differential/sessions/generated.json");
const CENSUS: &str = include_str!("../../../../../lash-typescript/tests/test262/census.tsv");
const README: &str = include_str!("../../../../../lash-typescript/README.md");
const CORPUS: &str =
    include_str!("../../../../../lash-typescript/tests/differential/sessions/expectations.json");

/// The bounded corpus: seeds `0..BOUNDED_SEEDS`, checked in with Node's
/// answers and run in the cacheable test partition.
const BOUNDED_SEEDS: u64 = 96;

/// The bounded corpus runs as this many tests, so the harness spreads it
/// across its threads.
const SHARDS: u64 = 4;

#[derive(Deserialize, Serialize)]
struct GeneratedCorpus {
    node: String,
    seeds: u64,
    sessions: Vec<StoredSession>,
    round_trip: Vec<StoredRow>,
}

#[derive(Deserialize, Serialize)]
struct StoredSession {
    seed: u64,
    probe: Vec<String>,
    cells: Vec<StoredCell>,
}

#[derive(Deserialize, Serialize)]
struct StoredCell {
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reject: Option<String>,
    node: Observation,
}

#[derive(Deserialize, Serialize)]
struct StoredRow {
    name: String,
    create: String,
    uses: String,
    node: Vec<Observation>,
}

fn corpus() -> GeneratedCorpus {
    let corpus: GeneratedCorpus = serde_json::from_str(GENERATED).expect("generated.json parses");
    assert_eq!(
        corpus.node, PINNED_NODE,
        "the generated corpus pins the oracle's Node"
    );
    corpus
}

/// What lash must observe for a cell Node observed as `node`: Node's
/// observation under the `closure-boundary` rule (the generator draws
/// closures freely and no later cell reads one), and the census diagnostic
/// for a rejected cell.
fn expected(node: &Observation, reject: Option<&str>) -> Observation {
    let mut expected = node.clone();
    expected.diagnostic = reject.map(str::to_string);
    for name in &node.closures {
        expected
            .probes
            .insert(name.clone(), super::DROPPED.to_string());
    }
    expected.closures.clear();
    expected
}

/// Where a session first departs from Node in one harness mode.
#[derive(Debug)]
pub(super) struct Divergence {
    pub(super) cell: usize,
    pub(super) detail: String,
    pub(super) lash: Observation,
    pub(super) expected: Observation,
}

impl Divergence {
    /// What kind of divergence this is, for a minimizer that must keep it
    /// the same one: how lash ended the cell and which parts of the cell's
    /// observation differ.
    pub(super) fn signature(&self) -> String {
        let lash = self.lash.comparable();
        let expected = self.expected.comparable();
        let parts = [
            ("outcome", lash.outcome != expected.outcome),
            ("error", lash.error != expected.error),
            ("diagnostic", lash.diagnostic != expected.diagnostic),
            ("finish", lash.finish != expected.finish),
            ("prints", lash.prints != expected.prints),
            ("probes", lash.probes != expected.probes),
        ];
        let differing = parts
            .iter()
            .filter(|(_, differs)| *differs)
            .map(|(name, _)| *name)
            .collect::<Vec<_>>();
        format!(
            "{} {:?} {:?} {differing:?}",
            self.detail, lash.outcome, lash.diagnostic
        )
    }
}

/// The first divergence of `session` from Node's `answers` in `mode`, if any.
pub(super) fn first_divergence(
    session: &GeneratedSession,
    answers: &[Observation],
    mode: HarnessMode,
) -> Option<Divergence> {
    let sources = session
        .cells
        .iter()
        .map(|cell| cell.source())
        .collect::<Vec<_>>();
    let lash = run_session(mode, &session.probe, &sources);
    for (index, ((lash, answer), cell)) in lash.iter().zip(answers).zip(&session.cells).enumerate()
    {
        let expected = expected(answer, cell.reject.as_deref());
        let inherited = lash.observation.detail.as_deref().and_then(|message| {
            super::super::INHERITED_DIAGNOSTICS
                .iter()
                .find(|fragment| message.contains(**fragment))
        });
        let detail = if !lash.stray.is_empty() {
            Some(format!(
                "session globals no probe answers as bound: {:?}",
                lash.stray
            ))
        } else if let Some(fragment) = inherited {
            Some(format!(
                "a cell failed for an earlier cell's program (`{fragment}`)"
            ))
        } else if !lash.observation.agrees_with(&expected) {
            Some("lash and Node observe the cell differently".to_string())
        } else {
            None
        };
        if let Some(detail) = detail {
            return Some(Divergence {
                cell: index,
                detail,
                lash: lash.observation.clone(),
                expected,
            });
        }
    }
    None
}

/// Node's answer for every cell of `session`, from the live oracle.
pub(super) fn node_answers(
    oracle: &mut NodeOracle,
    session: &GeneratedSession,
) -> Vec<Observation> {
    let sources = session
        .cells
        .iter()
        .map(|cell| cell.source())
        .collect::<Vec<_>>();
    oracle.observe(&NodeSession {
        probe: &session.probe,
        cells: session
            .cells
            .iter()
            .zip(&sources)
            .map(|(cell, source)| NodeCell {
                source,
                reject: cell.reject.as_deref(),
            })
            .collect(),
    })
}

/// Writes `generated.json`: the bounded corpus and the round-trip rows, each
/// with the pinned Node's answer. Deliberate, like `generate.mjs`.
#[test]
#[ignore = "asks the pinned Node live and writes generated.json; run through `kiln run`"]
#[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the live Node oracle is a test host capability.
fn write_the_generated_corpus() {
    let mut oracle = NodeOracle::start();
    let sessions = (0..BOUNDED_SEEDS)
        .map(|seed| {
            let session = generate(seed);
            let answers = node_answers(&mut oracle, &session);
            StoredSession {
                seed,
                probe: session.probe.clone(),
                cells: session
                    .cells
                    .iter()
                    .zip(answers)
                    .map(|(cell, node)| StoredCell {
                        source: cell.source(),
                        reject: cell.reject.clone(),
                        node,
                    })
                    .collect(),
            }
        })
        .collect();
    let round_trip = ROWS
        .iter()
        .map(|row| {
            let sources = [format!("{}\n", row.create), format!("{}\n", row.uses)];
            StoredRow {
                name: row.name.to_string(),
                create: row.create.to_string(),
                uses: row.uses.to_string(),
                node: oracle.observe(&NodeSession {
                    probe: &[],
                    cells: sources
                        .iter()
                        .map(|source| NodeCell {
                            source,
                            reject: None,
                        })
                        .collect(),
                }),
            }
        })
        .collect();
    let corpus = GeneratedCorpus {
        node: PINNED_NODE.to_string(),
        seeds: BOUNDED_SEEDS,
        sessions,
        round_trip,
    };
    let mut text = serde_json::to_string_pretty(&corpus).expect("the corpus serializes");
    text.push('\n');
    let path = sessions_directory().join("generated.json");
    std::fs::write(&path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

/// The bounded corpus's sessions of one shard, each regenerated from its
/// seed, held to the checked-in session, and run through lash in both
/// harness modes against Node's checked-in answer.
fn check_shard(shard: u64) {
    let corpus = corpus();
    assert_eq!(
        corpus.seeds, BOUNDED_SEEDS,
        "generated.json holds another bounded corpus; regenerate it with `{REGENERATE}`"
    );
    let mut failures = Vec::new();
    for stored in corpus
        .sessions
        .iter()
        .filter(|stored| stored.seed % SHARDS == shard)
    {
        let session = generate(stored.seed);
        let drifted = session.probe != stored.probe
            || session.cells.len() != stored.cells.len()
            || session
                .cells
                .iter()
                .zip(&stored.cells)
                .any(|(cell, stored)| {
                    cell.source() != stored.source || cell.reject != stored.reject
                });
        if drifted {
            failures.push(format!(
                "seed {}: the generator no longer draws the checked-in session; regenerate generated.json with `{REGENERATE}`",
                stored.seed
            ));
            continue;
        }
        let answers = stored
            .cells
            .iter()
            .map(|cell| cell.node.clone())
            .collect::<Vec<_>>();
        for mode in HarnessMode::ALL {
            if let Some(divergence) = first_divergence(&session, &answers, *mode) {
                failures.push(format!(
                    "seed {} {mode:?} cell {}: {}\n  source: {:?}\n  lash:   {:?}\n  expect: {:?}",
                    stored.seed,
                    divergence.cell,
                    divergence.detail,
                    stored.cells[divergence.cell].source,
                    divergence.lash,
                    divergence.expected
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn generated_sessions_observe_what_node_observes_shard_0() {
    check_shard(0);
}

#[test]
fn generated_sessions_observe_what_node_observes_shard_1() {
    check_shard(1);
}

#[test]
fn generated_sessions_observe_what_node_observes_shard_2() {
    check_shard(2);
}

#[test]
fn generated_sessions_observe_what_node_observes_shard_3() {
    check_shard(3);
}

/// The longer run: fresh seeds (`LASH_GENERATED_SEEDS=START..END`) against
/// the pinned Node live. A divergence is minimized and printed as the
/// session-corpus row that pins it, ready to classify and commit.
#[test]
#[ignore = "asks the pinned Node live; set LASH_GENERATED_SEEDS=START..END"]
#[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the live Node oracle is a test host capability.
fn generated_sessions_against_live_node() {
    let spec = std::env::var("LASH_GENERATED_SEEDS")
        .expect("LASH_GENERATED_SEEDS=START..END names the seeds to run");
    let (start, end) = spec
        .split_once("..")
        .expect("LASH_GENERATED_SEEDS is START..END");
    let start = start.parse::<u64>().expect("a start seed");
    let end = end.parse::<u64>().expect("an end seed");
    let mut oracle = NodeOracle::start();
    let mut diverged = Vec::new();
    for seed in start..end {
        let session = generate(seed);
        let answers = node_answers(&mut oracle, &session);
        for mode in HarnessMode::ALL {
            if let Some(divergence) = first_divergence(&session, &answers, *mode) {
                let row =
                    super::minimize::minimize(&mut oracle, seed, &session, *mode, &divergence);
                println!(
                    "seed {seed} {mode:?} cell {}: {}\n{row}",
                    divergence.cell, divergence.detail
                );
                diverged.push(seed);
                break;
            }
        }
    }
    assert!(
        diverged.is_empty(),
        "generated sessions diverged from Node at seeds {diverged:?}; the minimized corpus row of each is printed above"
    );
}

/// Whether the census accepts the row `kind name`.
fn census_accepts(kind: &str, name: &str) -> bool {
    CENSUS.lines().any(|line| {
        let columns = line.split('\t').collect::<Vec<_>>();
        columns.len() >= 3 && columns[0] == kind && columns[1] == name && columns[2] == "accepted"
    })
}

/// The generator draws only from the dialect's accepted grammar, and the
/// bounded corpus reaches every row it can draw.
#[test]
fn the_generator_draws_only_accepted_grammar() {
    let mut failures = Vec::new();
    for grammar in GRAMMAR {
        match grammar {
            Grammar::Census(kind, name) if !census_accepts(kind, name) => failures.push(format!(
                "the generator draws `{kind} {name}`, which is not an accepted census row"
            )),
            Grammar::Whatwg(class) if !README.contains(&format!("new {class}(")) => {
                failures.push(format!(
                    "the generator draws WHATWG `{class}`, whose signatures the README does not accept"
                ));
            }
            _ => {}
        }
    }
    let mut reached = BTreeSet::new();
    for seed in 0..BOUNDED_SEEDS {
        let session = generate(seed);
        for grammar in &session.grammar {
            if !GRAMMAR.contains(grammar) {
                failures.push(format!(
                    "seed {seed} draws {grammar:?}, which GRAMMAR does not list"
                ));
            }
        }
        reached.extend(session.grammar);
    }
    for grammar in GRAMMAR {
        if !reached.contains(grammar) {
            failures.push(format!("the bounded corpus never draws {grammar:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_generator_is_a_function_of_its_seed() {
    assert_eq!(generate(7), generate(7));
    assert_ne!(generate(7), generate(8));
}

/// Whether the crate README's section `section` names `name`.
fn listed(section: &str, name: &str) -> bool {
    README
        .split(section)
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .unwrap_or_default()
        .contains(&format!("`{name}`"))
}

/// Every shape the generator excludes is an open defect, pinned where it
/// diverges: by a defect cell of the session corpus.
/// When the defect is fixed, its README entry goes and this test names the
/// exclusion to delete.
#[test]
fn every_generator_exclusion_is_a_pinned_open_defect() {
    let corpus: serde_json::Value =
        serde_json::from_str(CORPUS).expect("the session expectations parse");
    let corpus_defects = corpus["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|session| session["cells"].as_array().into_iter().flatten())
        .filter_map(|cell| cell["defect"].as_str())
        .collect::<BTreeSet<_>>();
    let mut failures = Vec::new();
    for (defect, ticket) in OPEN_DEFECT_EXCLUSIONS {
        if !listed("## Open conformance defects", defect) {
            failures.push(format!(
                "the generator excludes `{defect}` ({ticket}), which is not an open defect: delete the exclusion"
            ));
        }
        if !corpus_defects.contains(defect) {
            failures.push(format!(
                "the generator excludes `{defect}`, which no session-corpus defect cell pins"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The snapshot round-trip law over every value type (see [`round_trip`]).
#[test]
fn every_value_type_round_trips_or_is_refused() {
    let corpus = corpus();
    let mut failures = Vec::new();
    let kinds = lashlang::testing::heap_object_kinds();
    for kind in kinds {
        let covered = ROWS.iter().any(|row| row.kind == *kind)
            || NOT_A_DIALECT_VALUE.iter().any(|(name, _)| name == kind);
        if !covered {
            failures.push(format!(
                "heap kind `{kind}` has no round-trip row and no reason no program builds one"
            ));
        }
    }
    let primitives = [
        "number",
        "string",
        "boolean",
        "null",
        "undefined",
        "class instance",
    ];
    let mut names = BTreeSet::new();
    for row in ROWS {
        if !kinds.contains(&row.kind) && !primitives.contains(&row.kind) {
            failures.push(format!("round-trip row `{}` names no value kind", row.name));
        }
        if !names.insert(row.name) {
            failures.push(format!("round-trip row `{}` is named twice", row.name));
        }
        if let Some(deviation) = row.node_deviation
            && !listed("## Deviation register", deviation)
        {
            failures.push(format!(
                "round-trip row `{}` names deviation `{deviation}`, which the README does not register",
                row.name
            ));
        }
        let Some(stored) = corpus
            .round_trip
            .iter()
            .find(|stored| stored.name == row.name)
        else {
            failures.push(format!(
                "round-trip row `{}` has no Node answer in generated.json; regenerate it with `{REGENERATE}`",
                row.name
            ));
            continue;
        };
        if stored.create != row.create || stored.uses != row.uses {
            failures.push(format!(
                "round-trip row `{}` changed since its Node answer; regenerate generated.json with `{REGENERATE}`",
                row.name
            ));
            continue;
        }
        failures.extend(round_trip::check(row, &stored.node));
    }
    if corpus.round_trip.len() != ROWS.len() {
        failures.push(format!(
            "generated.json answers rows the law no longer has; regenerate it with `{REGENERATE}`"
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
