//! The example workflows: TypeScript sources imported through lash's
//! public workflow import exactly as authored. No document is written by hand.

use lash::LashCore;
use lash::workflow::document::{Document, FunctionRegistry, Name};
use lash::workflow::{WorkflowDocument, WorkflowDocumentEntry, WorkflowEnvironment};
use serde::{Deserialize, Serialize};

use crate::runtime::RunError;

/// Which kind of example a workflow is in the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogGroup {
    /// An app-like workflow over the mock tools.
    App,
    /// A small program showing how lash lowers one control-flow form.
    ControlFlow,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowCatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub group: CatalogGroup,
    /// How many statements and blocks lash's view of the entry has: the
    /// size of the document the example lowers to.
    pub statements: usize,
    pub scopes: usize,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SelectWorkflowRequest {
    pub id: String,
}

/// An example workflow: authored TypeScript, imported as written, and the
/// entry a run of it starts.
struct Example {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    group: CatalogGroup,
    entry: &'static str,
    source: &'static str,
}

/// The workflow the example opens with.
const DEFAULT_WORKFLOW: &str = "onboarding";

const EXAMPLES: &[Example] = &[
    Example {
        id: "deep-research",
        name: "Deep research brief",
        description: "Search mocked sources, map their evidence into a cited research brief, and display the result.",
        entry: "deep_research",
        group: CatalogGroup::App,
        source: include_str!("../catalog/deep_research.ts"),
    },
    Example {
        id: "weekly-kpis",
        name: "Weekly business KPIs",
        description: "Fetch two mocked reporting periods, compare revenue and activation metrics, and display the report.",
        entry: "weekly_kpis",
        group: CatalogGroup::App,
        source: include_str!("../catalog/weekly_kpis.ts"),
    },
    Example {
        id: "blank",
        name: "Blank workflow",
        description: "An empty starter you build up with edits.",
        entry: "blank",
        group: CatalogGroup::App,
        source: include_str!("../catalog/blank.ts"),
    },
    Example {
        id: "onboarding",
        name: "Onboarding",
        description: "An onboarding flow with a host approval call, a loop and mixed display updates.",
        entry: "onboarding",
        group: CatalogGroup::App,
        source: include_str!("../catalog/onboarding.ts"),
    },
    Example {
        id: "summarize-emails",
        name: "Summarize my top 5 emails",
        description: "List five recent emails, summarize each one, and show the digest.",
        entry: "summarize_top_emails",
        group: CatalogGroup::App,
        source: include_str!("../catalog/summarize_top_emails.ts"),
    },
    Example {
        id: "research-nvidia-stock",
        name: "Research NVIDIA stock",
        description: "Research NVIDIA's stock outlook and show a concise mocked briefing.",
        entry: "research_nvidia_stock",
        group: CatalogGroup::App,
        source: include_str!("../catalog/research_nvidia_stock.ts"),
    },
    Example {
        id: "team-standup-digest",
        name: "Team standup digest",
        description: "Collect Slack and GitHub activity, then present a daily team brief.",
        entry: "team_standup_digest",
        group: CatalogGroup::App,
        source: include_str!("../catalog/team_standup_digest.ts"),
    },
    Example {
        id: "traffic-lights",
        name: "Traffic Lights",
        description: "A visual red, amber, and green light sequence repeated twice.",
        entry: "traffic_lights",
        group: CatalogGroup::App,
        source: include_str!("../catalog/traffic_lights.ts"),
    },
    Example {
        id: "branching-approval",
        name: "Branching Approval",
        description: "An if-heavy approval flow with a visible host approval call and distinct outcomes.",
        entry: "branching_approval",
        group: CatalogGroup::App,
        source: include_str!("../catalog/branching_approval.ts"),
    },
    Example {
        id: "counter-loop",
        name: "Counter Loop",
        description: "A while loop followed by a for loop, with progress updates.",
        entry: "counter_loop",
        group: CatalogGroup::App,
        source: include_str!("../catalog/counter_loop.ts"),
    },
    Example {
        id: "flat",
        name: "Inbox review (straight line)",
        description: "Show a message, list three mock emails and set a status, with no control flow.",
        entry: "demo",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/flat.ts"),
    },
    Example {
        id: "branch",
        name: "Conditional notification",
        description: "An if/else around two display messages.",
        entry: "demo",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/branch.ts"),
    },
    Example {
        id: "digest",
        name: "Digest (untyped for-of)",
        description: "A for-of over an untyped list with an if/else and sleeps, counting what it saw.",
        entry: "digest",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/digest.ts"),
    },
    Example {
        id: "typed",
        name: "Digest (typed Topic[])",
        description: "The digest loop with a declared `Topic[]` parameter type.",
        entry: "digest",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/typed.ts"),
    },
    Example {
        id: "inbox",
        name: "Inbox loop",
        description: "A for-of that shows each item's title, with a sleep per item.",
        entry: "digest",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/inbox.ts"),
    },
    Example {
        id: "countdown",
        name: "While with break",
        description: "A `while (true)` with a counter that breaks after four ticks.",
        entry: "countdown",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/countdown.ts"),
    },
    Example {
        id: "nested",
        name: "Nested loops",
        description: "A for-of over lists inside a for-of over mock emails.",
        entry: "file_emails",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/nested.ts"),
    },
    Example {
        id: "retry",
        name: "Try/catch in a loop",
        description: "A web search per limit inside try/catch; the out-of-range limit fails and is caught.",
        entry: "resilient_search",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/retry.ts"),
    },
    Example {
        id: "helper",
        name: "Helper function in a loop",
        description: "The loop body calls a helper that summarizes and shows each mock email.",
        entry: "summarize_inbox",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/helper.ts"),
    },
    Example {
        id: "unread",
        name: "Filter and map",
        description: "`.filter` the unread mock emails, `.map` them to subjects, then list each one.",
        entry: "unread_subjects",
        group: CatalogGroup::ControlFlow,
        source: include_str!("../catalog/unread.ts"),
    },
];

/// An example ready to open: its listing, the document lash imported its
/// source as, and that source.
pub(crate) struct CatalogWorkflow {
    pub(crate) listing: WorkflowCatalogEntry,
    pub(crate) document: Document,
    pub(crate) entry: Name,
    source: &'static str,
}

impl CatalogWorkflow {
    /// What opening this example saves the next version as.
    pub(crate) fn origin(&self) -> crate::Origin {
        crate::Origin::Imported(self.source.to_owned())
    }
}

/// Every example, prepared once: built-ins parsed, sources imported.
pub(crate) struct Catalog {
    workflows: Vec<CatalogWorkflow>,
}

impl Catalog {
    /// Imports every example against `environment`. An example lash does
    /// not import is an error: the catalog lists only examples that load.
    pub(crate) async fn load(
        core: &LashCore,
        environment: &WorkflowEnvironment,
    ) -> Result<Self, RunError> {
        let mut workflows = Vec::new();
        for example in EXAMPLES {
            let document = crate::authoring_http::import(core, example.source)
                .await
                .map_err(|refusal| {
                    RunError::Invalid(format!(
                        "example `{}`: {} {}",
                        example.id, refusal.body.error.message, refusal.body.error.details
                    ))
                })?;
            let entry = Name::new(example.entry);
            let view = entry_view(&document, &entry, environment.functions()).map_err(|error| {
                RunError::Invalid(format!("example `{}` has no view: {error}", example.id))
            })?;
            workflows.push(CatalogWorkflow {
                listing: WorkflowCatalogEntry {
                    id: example.id.to_owned(),
                    name: example.name.to_owned(),
                    description: example.description.to_owned(),
                    group: example.group,
                    statements: view.statements.len(),
                    scopes: view.scopes.len(),
                },
                document,
                entry,
                source: example.source,
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

    pub(crate) fn get(&self, id: &str) -> Option<&CatalogWorkflow> {
        self.workflows
            .iter()
            .find(|workflow| workflow.listing.id == id)
    }

    /// The workflow the example opens with.
    pub(crate) fn default_workflow(&self) -> Option<&CatalogWorkflow> {
        self.get(DEFAULT_WORKFLOW)
    }
}

/// Lash's view of entry `entry` of `document`.
pub(crate) fn entry_view(
    document: &Document,
    entry: &Name,
    functions: &FunctionRegistry,
) -> Result<lash::workflow::view::WorkflowView, String> {
    let derived = WorkflowDocument::derive(
        document.clone(),
        WorkflowDocumentEntry::Entry {
            function: entry.clone(),
        },
        functions,
    )
    .map_err(|refusal| refusal.to_string())?;
    lash::workflow::view::derive(&derived).map_err(|error| error.to_string())
}

#[cfg(test)]
mod shape;

#[cfg(test)]
mod tests {
    //! FIG-6414: run coverage and replayed paths are reads of the document
    //! alone. The oracles below are the view-based folds verbatim; the law
    //! is that the document read answers identically for every example.

    use std::collections::BTreeMap;
    use std::sync::Arc;

    use lash::workflow::document::{Site, SpawnIdentity, TaskIdentity, Unit};
    use lash::workflow::view::{
        PathSite, PathStep, RanCounts, ReplayPath, RunObservation, ScopeCoverage,
        StatementCoverage, TaskPath, WorkflowCoverage, WorkflowView,
    };

    use super::{Catalog, EXAMPLES, entry_view};

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
        WorkflowCoverage {
            observation: counts.observation,
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
            .build()
            .expect("the durable backend");
        let host = crate::workflow_core(backend).expect("workflow core");
        let core = host.core().clone();
        let environment = crate::runtime::workflow_environment(&core)
            .await
            .expect("the workflow environment");
        let catalog = Catalog::load(&core, &environment)
            .await
            .expect("the catalog loads");
        assert_eq!(
            catalog.workflows.len(),
            EXAMPLES.len(),
            "the catalog lists every example"
        );

        let mut saw_statements = false;
        let mut saw_empty_scope = false;
        for workflow in &catalog.workflows {
            let id = &workflow.listing.id;
            let unit = Unit::Function(workflow.entry.clone());
            let view = entry_view(&workflow.document, &workflow.entry, environment.functions())
                .expect("the example's view");
            let n = workflow.document.site_ordinals().len() as u32;

            // Every ordinal the document does not count stays ignored: n and
            // n + 1 reach past the document's sites, and the duplicate second
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
                        task: TaskIdentity::Main,
                        starts: (0..n).rev().chain(0..n).collect(),
                    },
                    TaskPath {
                        task: TaskIdentity::Spawned(SpawnIdentity {
                            parent: Arc::new(TaskIdentity::Main),
                            site: Site::new(unit.clone(), [0]),
                            occurrence: 0,
                        }),
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
