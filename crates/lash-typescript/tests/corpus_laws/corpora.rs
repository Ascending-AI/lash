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
use super::ingest::{data_path, source_for};
use super::metadata::{self, Phase};

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
/// them. A row refused at parse or link has no program.
fn differential() -> Vec<CorpusProgram> {
    let table = include_str!("../differential/expectations.tsv");
    table
        .lines()
        .skip(1)
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

/// The Test262 slice's selected tests that compile, ingested exactly as the
/// slice runner ingests them. A ratcheted skip and a parse-phase negative
/// test are rejections, not programs.
fn test262() -> Vec<CorpusProgram> {
    let manifest = std::fs::read_to_string(data_path("manifest.tsv")).expect("read the manifest");
    manifest
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            let [path, _area, disposition, _expectation] = fields.as_slice() else {
                panic!("malformed manifest row: {line}")
            };
            if *disposition != "pass" {
                return None;
            }
            let file = data_path(path);
            let meta =
                metadata::read_metadata(&file).unwrap_or_else(|error| panic!("{path}: {error}"));
            let source = match &meta.negative {
                Some(negative) if negative.phase != Phase::Runtime => return None,
                Some(_) => source_for(&file, &meta, false),
                None => source_for(&file, &meta, true),
            };
            Some(CorpusProgram::new(format!("test262:{path}"), source))
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
            let mut globals = BTreeSet::new();
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
