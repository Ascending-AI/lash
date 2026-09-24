//! Every corpus the dialect has, as linkable programs.
//!
//! The corpus laws run over all of them (FIG-3599): the Node differential
//! table, the Test262 slice, the Node session corpus (each cell, linked
//! against the globals its earlier cells bound), the workflow-graph goldens
//! and the codemode-parity cells. A new corpus joins here, or the laws do not
//! see it.

// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeSet;

use lashlang::{LashlangHostEnvironment, TypeExpr};

use super::goldens;
use super::ingest::{data_path, harness_bindings, test_script};
use super::metadata::{self, Phase, TestFlag};

/// One program of one corpus.
#[derive(Clone, Debug)]
pub(crate) struct CorpusProgram {
    /// `<corpus>:<row>`, stable across runs; the allowlists key on it.
    pub(crate) id: String,
    pub(crate) source: String,
    /// The session globals the program links against: the names earlier
    /// cells of its session bound. Empty outside the session corpus.
    pub(crate) globals: BTreeSet<String>,
}

impl CorpusProgram {
    fn new(id: String, source: String) -> Self {
        Self {
            id,
            source,
            globals: BTreeSet::new(),
        }
    }

    /// The host every corpus links against: the harness's catalogue, the
    /// operations the goldens and the parity cells call, and the program's
    /// session globals.
    pub(crate) fn environment(&self) -> LashlangHostEnvironment {
        let mut environment = lashlang::testing::harness::test_environment();
        for (module, type_name, operation) in
            [("tools", "Tools", "lookup"), ("web", "Web", "fetch")]
        {
            environment
                .resources
                .add_module_operation(
                    [module],
                    type_name,
                    operation,
                    operation,
                    TypeExpr::Any,
                    TypeExpr::Any,
                )
                .expect("the corpus catalogue has no conflicting operation");
        }
        lashlang::add_trigger_resource_operations(&mut environment.resources)
            .expect("the corpus catalogue has no conflicting trigger operation");
        environment
            .resources
            .add_trigger_source_constructor(
                ["timer", "Schedule"],
                TypeExpr::Object(vec![lashlang::TypeField {
                    name: "expr".into(),
                    ty: TypeExpr::Str,
                    optional: false,
                }]),
                lashlang::NamedDataType::object(
                    "timer.Tick",
                    vec![lashlang::TypeField {
                        name: "fired_at".into(),
                        ty: TypeExpr::Str,
                        optional: false,
                    }],
                )
                .expect("a valid timer tick type"),
            )
            .expect("the corpus catalogue has one timer trigger source");
        environment.with_globals(self.globals.iter().cloned())
    }
}

/// Every program of every corpus.
pub(crate) fn all() -> Vec<CorpusProgram> {
    let mut programs = differential();
    programs.extend(test262());
    programs.extend(sessions());
    programs.extend(goldens());
    programs.extend(codemode_parity());
    let ids = programs
        .iter()
        .map(|program| program.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids.len(), programs.len(), "corpus ids are unique");
    programs
}

/// The Node differential table's rows a cell admits, as the oracle compiles
/// them: every `expectations/<shard>.tsv` in sorted order, so a new shard
/// joins the corpus without an edit here. A row refused at parse or link has
/// no program.
fn differential() -> Vec<CorpusProgram> {
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/differential/expectations");
    let mut shards = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
        .map(|entry| entry.expect("an expectations entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tsv"))
        .collect::<Vec<_>>();
    shards.sort();
    shards
        .into_iter()
        .flat_map(|path| {
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
                .lines()
                .skip(1)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter_map(|line| {
            let columns = line.split('\t').collect::<Vec<_>>();
            let [lane, index, disposition, expression, ..] = columns.as_slice() else {
                panic!("malformed differential row: {line}")
            };
            let expression: String =
                serde_json::from_str(expression).expect("differential expression JSON");
            let source = match *disposition {
                "accept" | "open-defect" => format!("finish(`${{{expression}}}`);"),
                "runtime-reject" => format!("finish({expression});"),
                // A cell refuses these at parse or link, so they have no
                // admitted artifact; the oracle pins their refusal.
                "reject" | "accept-unlinked" => return None,
                other => panic!("unknown differential disposition `{other}`"),
            };
            Some(CorpusProgram::new(
                format!("differential:{lane}:{index}"),
                source,
            ))
        })
        .collect()
}

/// The Test262 PR sample's passing tests, each as the Script that runs after
/// its harness: the test bridged exactly as the conformance runner bridges
/// it, linked against the global names its harness binds, as a later cell
/// links against an earlier one's. The laws hold over the test's own code;
/// the harness is the runner's, not the corpus's. The sample, not the whole
/// selection, keeps this law's cost in the PR lane; a parse-phase negative
/// test is a rejection, not a program.
fn test262() -> Vec<CorpusProgram> {
    let rows = |file: &str| {
        std::fs::read_to_string(data_path(file))
            .unwrap_or_else(|error| panic!("read {file}: {error}"))
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let passing = rows("outcomes.tsv")
        .into_iter()
        .filter_map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            let [path, class, _qualifier] = fields.as_slice() else {
                panic!("malformed outcomes row: {line}")
            };
            (*class == "pass").then(|| (*path).to_owned())
        })
        .collect::<BTreeSet<_>>();
    rows("sample.tsv")
        .into_iter()
        .filter(|path| passing.contains(path))
        .filter_map(|path| {
            let file = data_path(&path);
            let meta =
                metadata::read_metadata(&file).unwrap_or_else(|error| panic!("{path}: {error}"));
            let source = match &meta.negative {
                Some(negative) if negative.phase != Phase::Runtime => return None,
                Some(_) => test_script(&file, &meta, false),
                None if meta.flags.contains(&TestFlag::Async) => test_script(&file, &meta, false),
                None => test_script(&file, &meta, true),
            };
            Some(CorpusProgram {
                id: format!("test262:{path}"),
                source,
                globals: harness_bindings(&meta),
            })
        })
        .collect()
}

/// Every cell of the Node session corpus, linked against the globals the
/// session's earlier cells bound. A cell the dialect refuses has no program:
/// a `reject` cell, or a defect cell whose stated answer is the refusal.
fn sessions() -> Vec<CorpusProgram> {
    super::sessions::corpus()
        .sessions
        .iter()
        .flat_map(|session| {
            let host = session.host.keys().cloned().collect::<BTreeSet<_>>();
            let mut globals = host.clone();
            let mut programs = Vec::new();
            for (index, cell) in session.cells.iter().enumerate() {
                if !cell.refused() {
                    programs.push(CorpusProgram {
                        id: format!("session:{}:{index}", session.id),
                        source: cell.source.clone(),
                        globals: globals.clone(),
                    });
                }
                globals = session.bound_after(cell);
                globals.extend(host.iter().cloned());
            }
            programs
        })
        .collect()
}

fn goldens() -> Vec<CorpusProgram> {
    let mut programs = goldens::ALL
        .iter()
        .map(|(name, source)| CorpusProgram::new(format!("golden:{name}"), (*source).to_string()))
        .collect::<Vec<_>>();
    programs.extend(
        goldens::CARRIER_LAWS
            .iter()
            .enumerate()
            .map(|(index, source)| {
                CorpusProgram::new(
                    format!("golden:carrier-laws-{index}"),
                    (*source).to_string(),
                )
            }),
    );
    programs
}

fn codemode_parity() -> Vec<CorpusProgram> {
    [
        (
            "turn",
            include_str!("../../../../examples/codemode-parity/turn.ts"),
        ),
        (
            "durable-process",
            include_str!("../../../../examples/codemode-parity/durable-process.ts"),
        ),
    ]
    .into_iter()
    .map(|(name, source)| CorpusProgram::new(format!("codemode-parity:{name}"), source.to_string()))
    .collect()
}
