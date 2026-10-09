//! Law L12 (FIG-3571): observation is language-agnostic.
//!
//! `Mini` is a test-only front end with no TypeScript anywhere in this crate's
//! dependency graph. It lowers its own statements straight to IR, which records
//! no front end (a module's identity is its IR, FIG-4020), and marks the
//! structure it generates with the language-neutral forms: an iteration whose
//! `bind` destructures each element and whose body is a completion list holding
//! a branch, an attribute assignment and an attribute update, a scope, a
//! collection transform, a lifted process wrapped as a process body, a sleep
//! effect, and private slots for its own temporaries. Its programs get complete
//! maps (L1), identical sites and events across relink, a stored reload through
//! a real store's decoder, and redrive (L2), and working process observation,
//! with nothing in the runtime knowing what a `Mini` program looked like.

use super::*;

/// The second front end's statements.
enum Mini {
    Let(&'static str, lash_vm::Expr),
    /// `each [key, value] in pairs { body }`
    EachPair {
        key: &'static str,
        value: &'static str,
        over: lash_vm::Expr,
        body: Vec<Mini>,
    },
    /// `when condition { then } otherwise { otherwise }`
    When {
        condition: lash_vm::Expr,
        then: Vec<Mini>,
        otherwise: Vec<Mini>,
    },
    /// `object.field := value`
    SetAttr {
        object: &'static str,
        field: &'static str,
        value: lash_vm::Expr,
    },
    /// `object.field += value`
    AddToAttr {
        object: &'static str,
        field: &'static str,
        value: lash_vm::Expr,
    },
    /// `{ body }`: a nested statement scope.
    Scope(Vec<Mini>),
    /// `name = map items with (param) => result`
    MapInto {
        name: &'static str,
        items: lash_vm::Expr,
        param: &'static str,
        result: lash_vm::Expr,
    },
    /// `pause`: a durable sleep effect.
    Pause,
    /// `worker name { body }`: an inline process, lifted by the linker.
    Worker {
        name: &'static str,
        body: Vec<Mini>,
    },
    Show(lash_vm::Expr),
    Done(lash_vm::Expr),
}

/// The front end: statement lists lower to completion lists, and each
/// construct to the IR form or structural role that says what it does.
struct MiniLowerer {
    temporaries: u32,
    private: std::collections::BTreeSet<lash_vm::AstString>,
}

impl MiniLowerer {
    fn program(statements: Vec<Mini>) -> lash_vm::Program {
        let mut lowerer = Self {
            temporaries: 0,
            private: Default::default(),
        };
        let mut program = b::program(lowerer.statements(statements));
        program.private_bindings = std::mem::take(&mut lowerer.private);
        program
    }

    fn statements(&mut self, statements: Vec<Mini>) -> Vec<lash_vm::Expr> {
        statements
            .into_iter()
            .map(|statement| self.statement(statement))
            .collect()
    }

    /// A body closed by its completion value.
    fn completion(&mut self, statements: Vec<Mini>) -> lash_vm::Expr {
        let mut items = self.statements(statements);
        items.push(b::null());
        b::role(lash_vm::StructuralRole::Completion, b::block(items))
    }

    /// A slot of the front end's own, which never reaches the session.
    fn temporary(&mut self, purpose: &str) -> String {
        self.temporaries += 1;
        let name = format!("mini_{purpose}_{}", self.temporaries);
        self.private.insert(name.as_str().into());
        name
    }

    fn attribute(
        &mut self,
        object: &str,
        field: &str,
        value: impl FnOnce(&str) -> lash_vm::Expr,
    ) -> lash_vm::Expr {
        let base = self.temporary("base");
        let result = self.temporary("result");
        b::role(
            lash_vm::StructuralRole::AttributeAssign,
            b::block(vec![
                b::assign(&base, b::var(object)),
                b::assign(&result, value(&base)),
                b::assign_path(&base, vec![b::field_step(field)], b::var(&result)),
                b::var(&result),
            ]),
        )
    }

    fn statement(&mut self, statement: Mini) -> lash_vm::Expr {
        match statement {
            Mini::Let(name, value) => b::assign(name, value),
            Mini::EachPair {
                key,
                value,
                over,
                body,
            } => {
                let element = self.temporary("element");
                let body = self.completion(body);
                b::for_bind(
                    &element,
                    over,
                    b::block(vec![
                        b::assign(key, b::index(b::var(&element), b::num(0.0))),
                        b::assign(value, b::index(b::var(&element), b::num(1.0))),
                    ]),
                    body,
                )
            }
            Mini::When {
                condition,
                then,
                otherwise,
            } => {
                let then = self.completion(then);
                let otherwise = self.completion(otherwise);
                b::if_else(condition, then, otherwise)
            }
            Mini::SetAttr {
                object,
                field,
                value,
            } => self.attribute(object, field, |_| value),
            Mini::AddToAttr {
                object,
                field,
                value,
            } => self.attribute(object, field, |base| {
                b::binary(
                    b::field(b::var(base), field),
                    lash_vm::CoercingBinaryOp::Add,
                    value,
                )
            }),
            Mini::Scope(body) => {
                let body = self.statements(body);
                b::role(lash_vm::StructuralRole::Scope, b::block(body))
            }
            Mini::MapInto {
                name,
                items,
                param,
                result,
            } => {
                let receiver = self.temporary("receiver");
                let callback = self.temporary("callback");
                let driver = self.temporary("driver");
                b::assign(
                    name,
                    b::role(
                        lash_vm::StructuralRole::CollectionTransform {
                            operation: "map".into(),
                        },
                        b::block(vec![
                            b::assign(&receiver, items),
                            b::assign(&callback, b::closure(None, &[param], &[], result)),
                            b::assign(
                                &driver,
                                b::closure(
                                    None,
                                    &[],
                                    &[&receiver, &callback],
                                    lash_vm::Expr::Map {
                                        items: Box::new(b::var(&receiver)),
                                        function: Box::new(b::var(&callback)),
                                    },
                                ),
                            ),
                            b::call(b::var(&driver), Vec::new()),
                        ]),
                    ),
                )
            }
            Mini::Pause => b::sleep_for(b::num(0.0)),
            Mini::Worker { name, body } => {
                let run_body = self.completion(body);
                let failure = self.temporary("failure");
                let wrapper = b::role(
                    lash_vm::StructuralRole::ProcessWrapper,
                    b::try_expr(
                        b::finish(b::call(b::closure(None, &[], &[], run_body), Vec::new())),
                        Some(b::catch(&failure, b::fail(b::var(&failure)))),
                        None,
                    ),
                );
                b::assign(name, b::process_literal(Vec::new(), wrapper))
            }
            Mini::Show(value) => b::print(value),
            Mini::Done(value) => b::finish(value),
        }
    }
}

/// The worker's body: every construct but the lift, so the lifted process is
/// the one that iterates, destructures, branches, writes and updates an
/// attribute, opens a scope, maps a collection and sleeps.
fn worker_body() -> Vec<Mini> {
    vec![
        Mini::Let("tally", b::record(vec![("seen", b::num(0.0))])),
        Mini::EachPair {
            key: "step",
            value: "weight",
            over: b::list(vec![
                b::list(vec![b::num(1.0), b::num(10.0)]),
                b::list(vec![b::num(2.0), b::num(20.0)]),
            ]),
            body: vec![
                Mini::Let(
                    "next",
                    b::binary(b::var("step"), lash_vm::CoercingBinaryOp::Add, b::num(1.0)),
                ),
                Mini::When {
                    condition: b::binary(
                        b::var("next"),
                        lash_vm::CoercingBinaryOp::Greater,
                        b::num(2.0),
                    ),
                    then: vec![Mini::SetAttr {
                        object: "tally",
                        field: "seen",
                        value: b::var("weight"),
                    }],
                    otherwise: vec![Mini::AddToAttr {
                        object: "tally",
                        field: "seen",
                        value: b::var("next"),
                    }],
                },
            ],
        },
        Mini::Scope(vec![Mini::Let("scoped", b::num(1.0))]),
        Mini::MapInto {
            name: "doubled",
            items: b::list(vec![b::num(1.0), b::num(2.0)]),
            param: "item",
            result: b::binary(
                b::var("item"),
                lash_vm::CoercingBinaryOp::Multiply,
                b::num(2.0),
            ),
        },
        Mini::Pause,
        Mini::Done(b::string("worked")),
    ]
}

fn mini_program() -> lash_vm::Program {
    MiniLowerer::program(vec![
        Mini::Let("box", b::record(vec![("value", b::num(0.0))])),
        Mini::EachPair {
            key: "item",
            value: "label",
            over: b::list(vec![
                b::list(vec![b::num(1.0), b::string("one")]),
                b::list(vec![b::num(2.0), b::string("two")]),
            ]),
            body: vec![
                Mini::Show(b::var("label")),
                Mini::When {
                    condition: b::binary(
                        b::var("item"),
                        lash_vm::CoercingBinaryOp::Greater,
                        b::num(1.0),
                    ),
                    then: vec![Mini::SetAttr {
                        object: "box",
                        field: "value",
                        value: b::var("item"),
                    }],
                    otherwise: vec![Mini::AddToAttr {
                        object: "box",
                        field: "value",
                        value: b::num(1.0),
                    }],
                },
            ],
        },
        Mini::Worker {
            name: "worker",
            body: worker_body(),
        },
        Mini::Done(b::var("worker")),
    ])
}

fn mini_environment() -> LashVmHostEnvironment {
    LashVmHostEnvironment::new(lash_vm::LashVmHostCatalog::new())
}

fn mini_module() -> lash_vm::ModuleCompileOutput {
    lash_vm::compile_module(lash_vm::ModuleCompileRequest {
        source: "mini",
        program: mini_program(),
        environment: &mini_environment(),
    })
    .expect("the mini program links")
}

fn lifted_worker(artifact: &lash_vm::ModuleArtifact) -> String {
    let lifted = artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            lash_vm::Declaration::Process(process) if process.origin.is_lifted() => {
                Some(process.name.to_string())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let [name] = lifted.as_slice() else {
        panic!("one lifted worker, got {lifted:?}");
    };
    name.clone()
}

type SiteKey = (
    lash_sansio::WorkflowSiteRef,
    lash_sansio::ExecutionNodeKind,
    String,
);

fn compiled_sites(
    artifact: &lash_vm::ModuleArtifact,
    entry: lash_vm::Entry<'_>,
) -> BTreeSet<SiteKey> {
    let compiled = lash_vm::compile(artifact, entry, None).expect("the mini entry compiles");
    lash_vm::testing::harness::compiled_execution_sites(&compiled)
        .into_iter()
        .map(|site| (site.site.clone(), site.kind, site.label.clone()))
        .collect()
}

/// The document an execution of `entry` in `artifact` names: the module's
/// graph and the entry it runs it from.
fn execution_document(
    artifact: &lash_vm::ModuleArtifact,
    entry: Option<&str>,
) -> WorkflowExecutionDocument {
    WorkflowExecutionDocument::fixture(
        lash_trace::WorkflowDocumentRef {
            source_identity: artifact.source_identity().to_string(),
            module_ref: lash_sansio::ModuleRef::new(&lash_sansio::ContentHash::new("module")),
            entry: match entry {
                None => lash_trace::WorkflowDocumentEntry::Main,
                Some(name) => lash_trace::WorkflowDocumentEntry::Process {
                    process_ref: name.to_string(),
                },
            },
            ir_version: 1,
        },
        lash_vm::workflow_graph_from_artifact(artifact),
        entry.map(str::to_string),
    )
}

/// The execution sites the document states for the body it enters, with
/// their kinds.
fn document_sites(document: &WorkflowExecutionDocument) -> BTreeSet<SiteKey> {
    fn collect(body: &lash_vm::WorkflowSubgraph, sites: &mut BTreeSet<SiteKey>) {
        for node in body.nodes() {
            sites.extend(node.execution_sites.iter().map(|site| {
                (
                    lash_sansio::WorkflowSiteRef::new(node.id.clone(), site.site_path.clone()),
                    site.kind,
                    site.label.clone(),
                )
            }));
            if let lash_vm::WorkflowNodeKind::Container(container) = &node.kind {
                for (_, child) in container.child_subgraphs() {
                    collect(child, sites);
                }
            }
        }
    }
    let mut sites = BTreeSet::new();
    collect(document.body(), &mut sites);
    sites
}

/// Every site of `sites` is a site of the reducer's index of `document`, so
/// an observation at it is never a mismatch.
fn assert_overlay_index_covers(document: &WorkflowExecutionDocument, sites: &BTreeSet<SiteKey>) {
    let index = document.overlay_document();
    assert_eq!(index.reference(), document.reference());
    for (site, _, _) in sites {
        assert!(index.contains(site), "{site} is outside the overlay index");
    }
}

#[test]
fn a_second_front_end_gets_complete_documents_for_main_and_its_lifted_process() {
    let output = mini_module();
    let worker = lifted_worker(&output.artifact);
    let worker_ref = output
        .artifact
        .process_ref(&worker)
        .expect("the lifted worker is exported")
        .clone();

    let main_sites = compiled_sites(&output.artifact, lash_vm::Entry::Main);
    let main_document = execution_document(&output.artifact, None);
    assert!(!main_sites.is_empty());
    assert_eq!(
        main_sites
            .difference(&document_sites(&main_document))
            .collect::<Vec<_>>(),
        Vec::<&SiteKey>::new(),
        "every main site is in the main document with its kind and owner path"
    );
    assert_overlay_index_covers(&main_document, &main_sites);

    let worker_sites = compiled_sites(&output.artifact, lash_vm::Entry::Process(&worker_ref));
    let worker_document = execution_document(&output.artifact, Some(&worker));
    let kinds = worker_sites
        .iter()
        .map(|(_, kind, _)| *kind)
        .collect::<BTreeSet<_>>();
    for kind in [
        lash_sansio::ExecutionNodeKind::Loop,
        lash_sansio::ExecutionNodeKind::Branch,
        lash_sansio::ExecutionNodeKind::Sleep,
        lash_sansio::ExecutionNodeKind::Terminal,
    ] {
        assert!(
            kinds.contains(&kind),
            "the worker's sites cover {kind:?}, got {kinds:?}"
        );
    }
    assert_eq!(
        worker_sites
            .difference(&document_sites(&worker_document))
            .collect::<Vec<_>>(),
        Vec::<&SiteKey>::new(),
        "every lifted-process site is in the process document with its kind and owner path"
    );
    assert_overlay_index_covers(&worker_document, &worker_sites);
    assert!(
        main_sites
            .iter()
            .any(|(site, _, _)| !worker_document.overlay_document().contains(site)),
        "the index holds the entry's body, not the whole module"
    );

    // The runtime and the projection read the front end's structure off the
    // neutral forms alone: a compound attribute update, a destructuring
    // iteration and private temporaries, none of them spelled in TypeScript.
    assert!(
        !output.artifact.ir().private_bindings.is_empty(),
        "the front end marks its temporaries private"
    );
    let graph = lash_vm::workflow_graph_from_artifact(&output.artifact);
    assert!(
        graph.nodes().any(|node| matches!(
            node.kind,
            lash_vm::WorkflowNodeKind::StateUpdate(lash_vm::WorkflowStateWrite::Member {
                update: Some(lash_vm::UpdateOperator::Add),
                ..
            })
        )),
        "the attribute update projects as an update of its target"
    );
    assert!(
        graph.nodes().any(|node| matches!(
            node.kind,
            lash_vm::WorkflowNodeKind::Container(lash_vm::WorkflowContainer::For { .. })
        )),
        "the destructuring iteration projects as a loop"
    );
}

/// Publishes `artifact` to a SQLite store at `path`, then reads it back
/// through a fresh view of a second store opened on the same file, whose
/// empty cache sends the read through the decoder.
async fn stored_reload(
    path: &std::path::Path,
    artifact: &lash_vm::ModuleArtifact,
) -> LashVmArtifacts {
    let publisher =
        lash_sqlite_store::SqliteStore::open(path, lash_sqlite_store::SqliteSynchronous::Normal)
            .await
            .expect("open the publishing store");
    LashVmArtifacts::new(Arc::new(publisher))
        .publish_module_artifact(&crate::lib_tests::host_claim(), artifact)
        .await
        .expect("the mini artifact publishes");
    LashVmArtifacts::new(Arc::new(
        lash_sqlite_store::SqliteStore::open(path, lash_sqlite_store::SqliteSynchronous::Normal)
            .await
            .expect("reopen the store"),
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_front_end_keeps_its_sites_across_relink_and_stored_reload() {
    let first = mini_module();
    let relinked = mini_module();
    assert_eq!(first.module_ref, relinked.module_ref);

    let dir = tempfile::tempdir().expect("store directory");
    let store = stored_reload(&dir.path().join("artifacts.db"), &first.artifact).await;
    let reloaded = store
        .get_module_artifact(&first.module_ref)
        .await
        .expect("the reopened store reads")
        .expect("the published artifact is retained");
    let reloaded = reloaded.as_ref();
    assert_eq!(
        reloaded, &first.artifact,
        "the decoded artifact is the published one"
    );
    assert_eq!(reloaded.source_identity(), first.artifact.source_identity());

    let worker = lifted_worker(&first.artifact);
    assert_eq!(lifted_worker(reloaded), worker);
    let worker_ref = first.artifact.process_ref(&worker).expect("worker").clone();
    assert_eq!(
        compiled_sites(reloaded, lash_vm::Entry::Main),
        compiled_sites(&first.artifact, lash_vm::Entry::Main)
    );
    assert_eq!(
        compiled_sites(reloaded, lash_vm::Entry::Process(&worker_ref)),
        compiled_sites(&first.artifact, lash_vm::Entry::Process(&worker_ref))
    );
    assert_eq!(
        lash_vm::workflow_graph_from_artifact(reloaded),
        lash_vm::workflow_graph_from_artifact(&first.artifact)
    );
}
