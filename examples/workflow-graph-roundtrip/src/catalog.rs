//! The shared Design 8 corpus is the picker inventory. Sources are literal
//! includes, so the example and its tests ship exactly the committed corpus.
use crate::{Dialect, runtime::RunError};
use lash::LashCore;
use lash::schema::Name;
use lash::workflow::WorkflowEnvironment;
use lash::workflow::document::{Captions, Document, SourceMap};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogGroup {
    Module,
    Cell,
    Refusal,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub group: CatalogGroup,
    pub entries: Vec<String>,
    pub statements: usize,
    pub scopes: usize,
    pub python: Option<LoweringSize>,
    pub refusal: Option<String>,
    pub typescript_source: Option<&'static str>,
    pub python_source: Option<&'static str>,
    pub typescript_witness: Option<&'static str>,
    pub python_witness: Option<&'static str>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoweringSize {
    pub statements: usize,
    pub scopes: usize,
}
#[derive(Clone, Debug, Deserialize)]
pub struct SelectWorkflowRequest {
    pub id: String,
    #[serde(default)]
    pub dialect: Dialect,
    pub entry: Option<Name>,
}
#[derive(Deserialize)]
struct Case {
    name: String,
    mode: String,
    dialects: Vec<String>,
    #[serde(default)]
    entries: Vec<String>,
    refusal: Option<String>,
}
use workflow_corpus::SOURCES;

pub(crate) struct CatalogWorkflow {
    pub(crate) listing: WorkflowCatalogEntry,
    typescript: Option<ImportedTwin>,
    python: Option<ImportedTwin>,
}
pub(crate) struct ImportedTwin {
    packages: crate::runtime::Namespace,
    pub(crate) document: Document,
    pub(crate) entry: Name,
    dialect: Dialect,
    source: &'static str,
    map: Option<SourceMap>,
    captions: Captions,
}
impl ImportedTwin {
    pub(crate) fn origin(&self) -> crate::Origin {
        crate::Origin::Imported {
            packages: Box::new(self.packages.clone()),
            dialect: self.dialect,
            text: self.source.into(),
            map: self.map.clone(),
            captions: self.captions.clone(),
        }
    }
}
pub(crate) struct Catalog {
    workflows: Vec<CatalogWorkflow>,
}
impl Catalog {
    pub(crate) async fn load(
        core: &LashCore,
        pin: &lash::process::HostArtifactPin,
        _environment: &WorkflowEnvironment,
    ) -> Result<Self, RunError> {
        let cases: Vec<Case> = serde_json::from_str(workflow_corpus::CATALOG_JSON)?;
        let mut workflows = Vec::new();
        for case in cases {
            let sources = SOURCES
                .iter()
                .find(|row| row.name == case.name)
                .ok_or_else(|| RunError::Invalid(format!("missing corpus source {}", case.name)))?;
            let declared = |extension: &str| case.dialects.iter().any(|name| name == extension);
            if case.dialects.len()
                != usize::from(sources.typescript.is_some()) + usize::from(sources.python.is_some())
                || declared("ts") != sources.typescript.is_some()
                || declared("py") != sources.python.is_some()
            {
                return Err(RunError::Invalid(format!(
                    "{}: corpus source dialects differ from the manifest",
                    case.name
                )));
            }
            let group = if case.mode == "cell" {
                CatalogGroup::Cell
            } else if case.refusal.is_some() {
                CatalogGroup::Refusal
            } else {
                CatalogGroup::Module
            };
            let mut twins = Vec::new();
            for (dialect, source) in [
                (Dialect::TypeScript, sources.typescript),
                (Dialect::Python, sources.python),
            ] {
                let Some(source) = source else {
                    twins.push(None);
                    continue;
                };
                if group == CatalogGroup::Cell {
                    twins.push(None);
                    continue;
                }
                let imported = crate::authoring_http::import(core, pin, dialect, source).await;
                if let Some(code) = &case.refusal {
                    let error = imported.err().ok_or_else(|| {
                        RunError::Invalid(format!("{} must refuse {code}", case.name))
                    })?;
                    let diagnostic: lash::dialect::Diagnostic =
                        serde_json::from_value(error.body.error.details["diagnostics"][0].clone())
                            .map_err(|error| RunError::Invalid(error.to_string()))?;
                    if diagnostic.code() != code {
                        return Err(RunError::Invalid(format!(
                            "{}: unexpected refusal {:?}",
                            case.name, error.body
                        )));
                    }
                    twins.push(None);
                } else {
                    let (document, map, captions, packages) = imported.map_err(|error| {
                        RunError::Invalid(format!("{} ({dialect:?}): {:?}", case.name, error.body))
                    })?;
                    let entries: Vec<_> = document
                        .exports
                        .iter()
                        .filter(|(_, export)| {
                            matches!(export, lash::workflow::document::Export::Function(_))
                        })
                        .map(|(name, _)| name.to_string())
                        .collect();
                    if entries != case.entries {
                        return Err(RunError::Invalid(format!(
                            "{}: entry contract changed",
                            case.name
                        )));
                    }
                    twins.push(Some(ImportedTwin {
                        packages,
                        document,
                        entry: Name::new(&entries[0]),
                        dialect,
                        source,
                        map,
                        captions,
                    }));
                }
            }
            let mut twins = twins.into_iter();
            let typescript = twins.next().flatten();
            let python = twins.next().flatten();
            let size = async |twin: &ImportedTwin| {
                entry_view(
                    core,
                    pin,
                    &twin.document,
                    &twin.entry,
                    &twin.packages.request,
                )
                .await
                .map(|view| LoweringSize {
                    statements: view.statements.len(),
                    scopes: view.scopes.len(),
                })
                .map_err(RunError::Invalid)
            };
            let ts_size = match typescript.as_ref() {
                Some(twin) => size(twin).await?,
                None => LoweringSize {
                    statements: 0,
                    scopes: 0,
                },
            };
            let py_size = match python.as_ref() {
                Some(twin) => Some(size(twin).await?),
                None => None,
            };
            workflows.push(CatalogWorkflow {
                listing: WorkflowCatalogEntry {
                    id: case.name.clone(),
                    name: case.name.replace('-', " "),
                    description: case
                        .refusal
                        .clone()
                        .unwrap_or_else(|| format!("{} corpus case", case.mode)),
                    group,
                    entries: case.entries,
                    statements: ts_size.statements,
                    scopes: ts_size.scopes,
                    python: py_size,
                    refusal: case.refusal,
                    typescript_source: sources.typescript,
                    python_source: sources.python,
                    typescript_witness: sources.typescript_witness,
                    python_witness: sources.python_witness,
                },
                typescript,
                python,
            });
        }
        Ok(Self { workflows })
    }
    pub(crate) fn entries(&self) -> Vec<WorkflowCatalogEntry> {
        self.workflows
            .iter()
            .map(|workflow| workflow.listing.clone())
            .collect()
    }
    pub(crate) fn get(&self, id: &str, dialect: Dialect) -> Option<Result<&ImportedTwin, &str>> {
        let workflow = self
            .workflows
            .iter()
            .find(|workflow| workflow.listing.id == id)?;
        let source = match dialect {
            Dialect::TypeScript => workflow.listing.typescript_source,
            Dialect::Python => workflow.listing.python_source,
        };
        if source.is_none() {
            return Some(Err(
                "This corpus case has no source in the selected dialect",
            ));
        }
        Some(
            match dialect {
                Dialect::TypeScript => &workflow.typescript,
                Dialect::Python => &workflow.python,
            }
            .as_ref()
            .ok_or(workflow.listing.refusal.as_deref().unwrap_or(
                "Cell cases run in sessions; inspect their crossing witness in the catalog",
            )),
        )
    }
    pub(crate) fn default_workflow(&self) -> Option<&ImportedTwin> {
        self.get("straight-tool", Dialect::TypeScript)
            .and_then(Result::ok)
    }
}
pub(crate) async fn entry_view(
    core: &LashCore,
    pin: &lash::process::HostArtifactPin,
    document: &Document,
    entry: &Name,
    environment: &lash::vm::WorkflowEnvironmentRequest,
) -> Result<lash::workflow::view::WorkflowView, String> {
    let start = core
        .host_artifacts()
        .publish_workflow(
            pin,
            &lash::workflow::document::DocumentBytes::from(document.clone()),
            &lash::package::StartSelector::Export(entry.clone()),
            environment,
            None,
            None,
        )
        .await
        .map_err(|error| error.to_string())?;
    let read = core
        .host_artifacts()
        .execution_document(&start)
        .await
        .map_err(|error| error.to_string())?;
    let lash::workflow::WorkflowDocumentRead::Read(checked) = read else {
        return Err(format!("{read:?}"));
    };
    let publications = environment
        .snapshot
        .advertisements
        .iter()
        .map(|accepted| accepted.publication.clone())
        .collect::<Vec<_>>();
    let context = core
        .host_artifacts()
        .workflow_view_context(&checked, &publications)
        .await
        .map_err(|error| error.to_string())?;
    lash::workflow::view::derive(&checked, &context).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    //! FIG-6414: run coverage and replayed paths are reads of the document
    //! alone. The oracles below are the view-based folds verbatim; the law
    //! is that the document read answers identically for every example.

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use lash::process::RunObservation;
    use lash::workflow::document::{Site, Unit};
    use lash::workflow::view::{
        PathSite, PathStep, RanCounts, ReplayPath, ScopeCoverage, StatementCoverage, TaskPath,
        WorkflowCoverage, WorkflowView,
    };

    use super::{Catalog, SOURCES, entry_view};

    fn first_statements(view: &WorkflowView) -> BTreeMap<Site, u32> {
        let mut first = BTreeMap::new();
        for statement in &view.statements {
            first
                .entry(statement.block.clone())
                .or_insert(statement.ordinal);
        }
        first
    }

    /// The view-based `view::coverage` this host ran before the document read.
    fn view_coverage(view: &WorkflowView, counts: &RanCounts) -> WorkflowCoverage {
        let mut ran = BTreeMap::<u32, u64>::new();
        for (ordinal, count) in &counts.ran {
            let total = ran.entry(*ordinal).or_default();
            *total = total.saturating_add(*count);
        }
        let count = |ordinal| {
            (counts.observation == Some(RunObservation::Counts))
                .then(|| ran.get(&ordinal).copied().unwrap_or(0))
        };
        let first = first_statements(view);
        let mut actions: Vec<_> = view
            .dataflow
            .actions
            .iter()
            .map(|action| StatementCoverage {
                site: action.site.clone(),
                runs: count(view.action_node(&action.site.position()).unwrap().ordinal),
            })
            .collect();
        actions.sort_by_key(|action| action.site.clone());
        let mut conditions: Vec<_> = view
            .nodes
            .iter()
            .filter_map(|node| {
                node.condition_ordinal.map(|ordinal| StatementCoverage {
                    site: node.site.clone(),
                    runs: count(ordinal),
                })
            })
            .collect();
        conditions.sort_by_key(|condition| condition.site.clone());
        WorkflowCoverage {
            observation: counts.observation,
            actions,
            conditions,
            statements: view
                .statements
                .iter()
                .map(|statement| StatementCoverage {
                    site: statement.site.clone(),
                    runs: count(statement.ordinal),
                })
                .collect(),
            scopes: view
                .scopes
                .iter()
                .map(|scope| ScopeCoverage {
                    block: scope.block.clone(),
                    entries: count(first.get(&scope.block).copied().unwrap_or(scope.ordinal)),
                })
                .collect(),
        }
    }

    /// The view-based `view::path` this host ran before the document read.
    fn view_path(view: &WorkflowView, replay: &ReplayPath) -> Vec<PathStep> {
        let mut sites = BTreeMap::<u32, Vec<PathSite>>::new();
        let first = first_statements(view);
        for scope in &view.scopes {
            sites
                .entry(first.get(&scope.block).copied().unwrap_or(scope.ordinal))
                .or_default()
                .push(PathSite::Scope {
                    block: scope.block.clone(),
                });
        }
        for statement in &view.statements {
            sites
                .entry(statement.ordinal)
                .or_default()
                .push(PathSite::Statement {
                    site: statement.site.clone(),
                });
        }
        let mut steps = Vec::new();
        for task in &replay.tasks {
            for ordinal in &task.starts {
                for site in sites.get(ordinal).into_iter().flatten() {
                    steps.push(PathStep {
                        task: task.task.clone(),
                        site: site.clone(),
                    });
                }
            }
        }
        steps
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn document_coverage_and_paths_equal_the_derived_views_fold_for_every_example() {
        let stores = lash::sqlite::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory stores");
        let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
            .config(lash::durable::DurableSettings::standard())
            .build()
            .expect("the durable backend");
        let host = crate::workflow_core(backend).expect("workflow core");
        let core = host.core().clone();
        let pin = lash::process::HostArtifactPin::mint();
        let environment = crate::runtime::workflow_environment(&core, &pin)
            .await
            .expect("the workflow environment");
        let catalog = Catalog::load(&core, &pin, &environment)
            .await
            .expect("the catalog loads");
        assert_eq!(
            catalog.workflows.len(),
            SOURCES.len(),
            "the catalog lists every example"
        );

        let mut saw_statements = false;
        let mut saw_empty_scope = false;
        for example in &catalog.workflows {
            let id = &example.listing.id;
            let Some(workflow) = &example.typescript else {
                continue;
            };
            let unit = match &workflow.document.exports[&workflow.entry] {
                lash::workflow::document::Export::Function(export) => {
                    Unit::Library(export.function)
                }
                _ => panic!("function Export"),
            };
            let view = entry_view(
                &core,
                &pin,
                &workflow.document,
                &workflow.entry,
                &workflow.packages.request,
            )
            .await
            .expect("the example's view");
            let n = (workflow.document.site_ordinals().len()
                + workflow.document.action_ordinals().len()
                + workflow.document.condition_ordinals().len()) as u32;

            // Every ordinal the document does not count stays ignored: n and
            // n + 1 reach past statement, action and condition sites. The duplicate second
            // entry of each even ordinal exercises summing.
            let ran: Vec<(u32, u64)> = (0..n + 2)
                .filter(|ordinal| ordinal % 3 != 0)
                .map(|ordinal| (ordinal, 2 * u64::from(ordinal) + 1))
                .chain(
                    (0..n + 2)
                        .filter(|ordinal| ordinal % 2 == 0)
                        .map(|ordinal| (ordinal, 1)),
                )
                .collect();
            for (observation, ran) in [
                (Some(RunObservation::Counts), ran.clone()),
                (Some(RunObservation::Off), ran.clone()),
                (None, ran),
                (Some(RunObservation::Counts), Vec::new()),
            ] {
                let counts = RanCounts { observation, ran };
                assert_eq!(
                    lash::workflow::view::coverage(&workflow.document, &unit, &counts),
                    view_coverage(&view, &counts),
                    "{id}: coverage of the document equals the view's fold"
                );
            }

            let replay = ReplayPath {
                tasks: vec![
                    TaskPath {
                        task: lash::workflow::document::testkit::root_task(),
                        starts: (0..n).rev().chain(0..n).collect(),
                    },
                    TaskPath {
                        task: lash::workflow::document::testkit::spawned_task(
                            lash::workflow::document::testkit::root_task(),
                            Site::new(unit.clone(), [0]),
                            0,
                        ),
                        starts: (0..n + 1).collect(),
                    },
                ],
            };
            assert_eq!(
                lash::workflow::view::path(&workflow.document, &unit, &replay),
                view_path(&view, &replay),
                "{id}: the document's path equals the view's fold"
            );

            let coverage = lash::workflow::view::coverage(
                &workflow.document,
                &unit,
                &RanCounts {
                    observation: Some(RunObservation::Counts),
                    ran: Vec::new(),
                },
            );
            saw_statements |= !coverage.statements.is_empty() && !coverage.scopes.is_empty();
            saw_empty_scope |= view
                .scopes
                .iter()
                .any(|scope| view.statements.iter().all(|s| s.block != scope.block));
        }
        assert!(
            saw_statements,
            "some example's coverage has statements and scopes"
        );
        assert!(
            saw_empty_scope,
            "some example has an empty scope, so both entry-ordinal rules ran"
        );
    }
}
