//! Sessions on the cell's package set (FIG-6926): each cell runs on the
//! set the gate answers, its carried saved functions relinked to that set
//! by name (RELINK-CLOSURE, K-LIB-009), so everything one cell runs comes
//! from one set (CURRENT-RELEASE), and a package value built by another
//! release of its package raises `package.stale_value` naming both
//! (K-SES-002 carries it). The node holds every release, the earlier ones
//! retained beside its own. A round's prompt and its cell take one gate
//! answer, and a parked cell's session stays stamped with its set across a
//! reclaim (FIG-6986).

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{FunctionId, PackageSetDigest};
use lash_kernel_package::{HeldPackages, Package, PackageSet, PackageSources, testing};

use super::{CellTools, SESSION, TURN, finish_of, open_host};
use crate::CellDialect;
use crate::plugin::runtime_state::RlmRuntimeState;
use crate::testing::{DurableHost, cell_context, cell_services, run_cell};

/// A gate a law moves between surfaces, as a fleet's would: `held`'s sets,
/// or a stored surface it does not hold.
struct Switch(Arc<HeldPackages>, Mutex<crate::CellPackages>);

impl Switch {
    fn new(held: &Arc<HeldPackages>, set: PackageSetDigest) -> Self {
        Self(Arc::clone(held), Mutex::new(held_cell(held, set)))
    }

    /// Answers `held`'s set `set` from now on.
    fn answer(&self, set: PackageSetDigest) {
        self.answer_with(held_cell(&self.0, set));
    }

    fn answer_with(&self, cell: crate::CellPackages) {
        *self.1.lock().expect("the gate") = cell;
    }
}

fn held_cell(held: &HeldPackages, set: PackageSetDigest) -> crate::CellPackages {
    crate::CellPackages::held(held, &set).expect("a held set")
}

#[async_trait::async_trait]
impl crate::plugin::PackageGate for Switch {
    async fn writable(
        &self,
    ) -> Result<crate::CellPackages, lash_core::RuntimeEffectControllerError> {
        Ok(self.1.lock().expect("the gate").clone())
    }
}

/// lash's own packages, as this build holds them.
fn lash() -> Vec<Arc<Package>> {
    lash_vm_runtime::lash_packages()
        .expect("lash's packages")
        .artifacts()
        .values()
        .cloned()
        .collect()
}

fn kernel(lash: &[Arc<Package>]) -> &Package {
    lash.iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package")
}

/// The `dataframe` fixture at `release`, its kernel and surface sources
/// each edited by its pair of `(from, to)` replacements.
fn dataframe(
    lash: &[Arc<Package>],
    release: &str,
    kernel_edits: &[(&str, &str)],
    surface_edits: &[(&str, &str)],
) -> Package {
    let edit = |source: &str, edits: &[(&str, &str)]| {
        edits.iter().fold(source.to_owned(), |text, (from, to)| {
            assert!(text.contains(from), "the fixture holds `{from}`");
            text.replace(from, to)
        })
    };
    let kernel_source = edit(testing::DATAFRAME.kernel, kernel_edits);
    let surface = edit(testing::DATAFRAME.surface, surface_edits);
    Package::build(
        "dataframe",
        release,
        &PackageSources {
            kernel: &kernel_source,
            surface: &surface,
            labels: testing::DATAFRAME.labels,
        },
        &[kernel(lash)],
    )
    .expect("the release builds")
}

/// R1: the fixture as written.
fn r1(lash: &[Arc<Package>]) -> Package {
    testing::dataframe(kernel(lash)).expect("the fixture builds")
}

/// R2 keeping every value type's shape: `col` charges more.
fn r2_kept(lash: &[Arc<Package>]) -> Package {
    dataframe(
        lash,
        "0.4.0",
        &[(
            "function dataframe.col(name: Text) -> expr\nkernel 1\ncharge 4",
            "function dataframe.col(name: Text) -> expr\nkernel 1\ncharge 5",
        )],
        &[],
    )
}

/// R2 whose `data_quanta` holds one more field.
fn r2_reshaped(lash: &[Arc<Package>]) -> Package {
    dataframe(
        lash,
        "0.4.0",
        &[(
            "return {source: source, plan: plan, __lash_brand",
            "return {source: source, plan: plan, rev: 1, __lash_brand",
        )],
        &[(
            "type data_quanta = Record{source: Text,",
            "type data_quanta = Record{rev: Int, source: Text,",
        )],
    )
}

/// R2 without `width`.
fn r2_removed(lash: &[Arc<Package>]) -> Package {
    dataframe(
        lash,
        "0.4.0",
        &[(
            "function dataframe.width(row: List(Any)) -> Int\nkernel 1\ncharge 2\nnative\nbody {\n  return list.len(row)\n}",
            "",
        )],
        &[(
            "function width(row: List(Any)) -> Int = dataframe.width\n",
            "",
        )],
    )
}

/// R2 whose `width` takes another parameter.
fn r2_resigned(lash: &[Arc<Package>]) -> Package {
    dataframe(
        lash,
        "0.4.0",
        &[(
            "function dataframe.width(row: List(Any)) -> Int",
            "function dataframe.width(row: List(Any), extra: Int) -> Int",
        )],
        &[(
            "function width(row: List(Any)) -> Int",
            "function width(row: List(Any), extra: Int) -> Int",
        )],
    )
}

/// R2 whose `col` charges more, with a new member `same`.
fn r2_grown(lash: &[Arc<Package>]) -> Package {
    dataframe(
        lash,
        "0.4.0",
        &[(
            "function dataframe.col(name: Text) -> expr\nkernel 1\ncharge 4",
            "function dataframe.same(n: Int) -> Int\nkernel 1\ncharge 1\nbody {\n  return n\n}\n\nfunction dataframe.col(name: Text) -> expr\nkernel 1\ncharge 5",
        )],
        &[(
            "function width(row: List(Any)) -> Int = dataframe.width\n",
            "function width(row: List(Any)) -> Int = dataframe.width\nfunction same(n: Int) -> Int = dataframe.same\n",
        )],
    )
}

/// The surface `installer` holds as `set`, as a node holding `node`'s
/// packages reads it from the store: the snapshot's bytes and its
/// artifacts' labels, over `node`'s registry with the definitions it
/// lacks.
fn stored_cell(
    node: &HeldPackages,
    installer: &HeldPackages,
    set: PackageSetDigest,
) -> crate::CellPackages {
    let installed = installer.surface(&set).expect("the installer's surface");
    let surface = lash_kernel_package::SurfaceSnapshot::decode(set, &installed.encode())
        .expect("the stored bytes")
        .with_labels(|artifact| {
            installed
                .packages()
                .find(|package| package.digest() == *artifact)
                .map(|package| package.labels().clone())
        });
    let stored: std::collections::BTreeMap<_, _> = surface
        .functions()
        .into_iter()
        .filter(|function| node.functions().get(function).is_none())
        .map(|function| {
            let registered = installer.functions().get(&function).expect("installed");
            (function, Arc::clone(registered.definition()))
        })
        .collect();
    let catalog = node.catalog_with(&stored).expect("the node's catalog");
    crate::CellPackages::stored(Arc::new(surface), catalog, stored)
}

/// `q@value`, whose `f` answers `value`.
fn q(lash: &[Arc<Package>], value: &str) -> Package {
    Package::build(
        "q",
        value,
        &PackageSources {
            kernel: &format!(
                "function q.f() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n"
            ),
            surface: "function f() -> Int = q.f\n",
            labels: "",
        },
        &[kernel(lash)],
    )
    .expect("q builds")
}

/// A node holding lash's packages with each of `releases` as a set of its
/// own, the last its own set, the others retained; with each set's
/// digest.
fn hold(
    lash: &[Arc<Package>],
    releases: Vec<Vec<Package>>,
) -> (Arc<HeldPackages>, Vec<PackageSetDigest>) {
    let mut artifacts: Vec<Arc<Package>> = lash.to_vec();
    let mut sets = Vec::new();
    for release in releases {
        let members: Vec<Arc<Package>> = lash
            .iter()
            .cloned()
            .chain(release.into_iter().map(Arc::new))
            .collect();
        sets.push(PackageSet::new(members.iter().map(AsRef::as_ref)).expect("a closed set"));
        artifacts.extend(members);
    }
    let digests: Vec<PackageSetDigest> = sets.iter().map(PackageSet::digest).collect();
    let own = *digests.last().expect("a set");
    let held = HeldPackages::new(artifacts, sets, own).expect("the node's packages");
    (Arc::new(held), digests)
}

/// The services of a node holding `held`, its cells written under the set
/// `gate` answers.
fn node(
    dialect: &CellDialect,
    held: &Arc<HeldPackages>,
    gate: &Arc<Switch>,
) -> super::super::CellServices {
    let mut services = cell_services(
        dialect,
        lash_vm_client::service::Service::default().with_packages(Arc::clone(held)),
    );
    services.kernel = super::super::KernelCarry::holding(Some(Arc::clone(held)), None);
    services.packages = Some(Arc::clone(gate) as Arc<dyn crate::plugin::PackageGate>);
    services
}

/// A session of `held`'s node on `host`, its cells written under the set
/// `gate` answers.
struct Session {
    dialect: CellDialect,
    host: DurableHost,
    state: super::super::CodeModeExecutionState,
    services: super::super::CellServices,
    gate: Arc<Switch>,
    tools: Arc<dyn lash_core::ToolProvider>,
    held: Arc<HeldPackages>,
    cells: usize,
}

impl Session {
    async fn open(held: &Arc<HeldPackages>, first: PackageSetDigest) -> Self {
        Self::open_in(CellDialect::typescript(), held, first).await
    }

    async fn open_in(
        dialect: CellDialect,
        held: &Arc<HeldPackages>,
        first: PackageSetDigest,
    ) -> Self {
        let gate = Arc::new(Switch::new(held, first));
        let services = node(&dialect, held, &gate);
        Self {
            state: crate::testing::session_state(&dialect),
            dialect,
            host: open_host().await,
            services,
            gate,
            tools: Arc::new(CellTools::default()),
            held: Arc::clone(held),
            cells: 0,
        }
    }

    /// The session's next cells served by a node holding `held`: one that
    /// no longer holds the release earlier cells ran on.
    fn on(&mut self, held: &Arc<HeldPackages>) {
        self.services = node(&self.dialect, held, &self.gate);
        self.held = Arc::clone(held);
    }

    /// The session with its cells offered `tools`'s operations.
    fn serving(mut self, tools: Arc<dyn lash_core::ToolProvider>) -> Self {
        self.tools = tools;
        self
    }

    async fn cell_under(&mut self, set: PackageSetDigest, code: &str) -> lash_core::ExecResponse {
        let cell = held_cell(&self.held, set);
        self.cell_with(cell, code).await
    }

    /// A cell the gate writes under `cell`.
    async fn cell_with(
        &mut self,
        cell: crate::CellPackages,
        code: &str,
    ) -> lash_core::ExecResponse {
        self.gate.answer_with(cell);
        let key: &'static str = Box::leak(format!("exec-code:{}", self.cells).into_boxed_str());
        self.cells += 1;
        run_cell(
            &mut self.state,
            cell_context(&self.host, SESSION, TURN, key, self.tools.clone()),
            &self.services,
            code,
        )
        .await
    }

    /// The library functions the saved function `name` lists.
    fn listed(&self, name: &str) -> BTreeSet<FunctionId> {
        self.state.lowering_functions()[&lash_kernel_doc::Name::new(name)]
            .document
            .manifest
            .functions
            .keys()
            .copied()
            .collect()
    }

    /// Every definition of `set`'s packages.
    fn set(&self, set: PackageSetDigest) -> BTreeSet<FunctionId> {
        self.held
            .view(&set)
            .expect("a held set")
            .definitions()
            .map(|(_, function)| function)
            .collect()
    }

    fn function(&self, set: PackageSetDigest, name: &str) -> FunctionId {
        self.held
            .view(&set)
            .expect("a held set")
            .definitions()
            .find(|(defined, _)| *defined == name)
            .map(|(_, function)| function)
            .expect("the set defines it")
    }

    /// The saved functions' tree, as the session's root names it.
    fn functions_root(&self) -> lash_kernel_state::paged::KeyedRoot {
        lash_kernel_state::kernel_internals::session_functions(
            &self.state.session().expect("a carried session"),
        )
    }
}

#[track_caller]
fn ok(response: &lash_core::ExecResponse) {
    assert!(response.error().is_none(), "{:?}", response.error());
}

#[track_caller]
fn failed(response: &lash_core::ExecResponse) -> String {
    response.error().expect("the cell fails").message.clone()
}

/// RELINK-CLOSURE and CURRENT-RELEASE: a saved function calling
/// `dataframe.col`, and a binding of `col` itself (TypeScript's
/// `const { col } = dataframe`), both saved under R1, run in a cell of R2
/// against R2's `col`, a new identity, while the node still holds R1 under
/// the same names; every function they list is R2's set's. A cell of the
/// set the session is linked to leaves every saved record as it was. Each
/// dialect, in its own spelling.
#[tokio::test(flavor = "multi_thread")]
async fn saved_functions_relink_to_the_cell_set_and_a_linked_set_rewrites_nothing() {
    let lash = lash();
    let (held, sets) = hold(&lash, vec![vec![r1(&lash)], vec![r2_kept(&lash)]]);
    for (dialect, saved, called, finish) in [
        (
            CellDialect::typescript(),
            "function keep(n) { return dataframe.col(n).name; }\nconst { col } = dataframe;",
            "const got = [keep(\"a\"), col(\"b\").name];",
            "await control.finish(got);",
        ),
        (
            CellDialect::python(),
            "def keep(n):\n    return dataframe.col(n).name\ncol = dataframe.col",
            "got = [keep(\"a\"), col(\"b\").name]",
            "await control.finish(got)",
        ),
    ] {
        let mut session = Session::open_in(dialect, &held, sets[0]).await;
        ok(&session.cell_under(sets[0], saved).await);
        let col_r1 = session.function(sets[0], "dataframe.col");
        let col_r2 = session.function(sets[1], "dataframe.col");
        assert_ne!(col_r1, col_r2);
        assert!(session.listed("keep").contains(&col_r1));
        let before = session.functions_root();

        ok(&session.cell_under(sets[1], called).await);
        for name in ["keep", "col"] {
            let listed = session.listed(name);
            assert!(listed.contains(&col_r2), "`{name}` reaches R2's col");
            assert!(
                listed.is_subset(&session.set(sets[1])),
                "`{name}` reaches R2's set alone"
            );
        }
        let relinked = session.functions_root();
        assert_ne!(before, relinked, "relinking re-saved the records");

        // The set the session is linked to: nothing relinks or re-saves.
        let finished = session.cell_under(sets[1], finish).await;
        assert_eq!(finish_of(&finished), serde_json::json!(["a", "b"]));
        assert_eq!(session.functions_root(), relinked);
    }
}

/// RELINK-CLOSURE: a saved function whose callee the cell's set removed,
/// or declares under another signature, is dropped at the cell's start,
/// named with the callee, and is no binding of a later cell. The node of
/// the next cell holds that set alone, as a production node does: the
/// callee's old signature is the one the function was saved against. One
/// a binding's data held, saved under a generated name, is named by its
/// path there (`plan.map`), and the binding goes with it.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_the_cell_set_does_not_serve_is_dropped_naming_it_and_the_callee() {
    let lash = lash();
    for r2 in [r2_removed(&lash), r2_resigned(&lash)] {
        let (earlier, first) = hold(&lash, vec![vec![r1(&lash)]]);
        let (later, second) = hold(&lash, vec![vec![r2]]);
        let mut session = Session::open(&earlier, first[0]).await;
        ok(&session
            .cell_under(
                first[0],
                "function w(row) { return dataframe.width(row); }\nconst plan = { map: (row) => dataframe.width(row) };",
            )
            .await);
        session.on(&later);
        ok(&session.cell_under(second[0], "const other = 1;").await);
        let not_carried = session.state.not_carried();
        let why = not_carried[&lash_kernel_doc::Name::new("w")].clone();
        assert!(
            why.contains("`w`") && why.contains("dataframe.width"),
            "{why}"
        );
        let why = not_carried[&lash_kernel_doc::Name::new("plan")].clone();
        assert!(
            why.contains("`plan.map`") && why.contains("dataframe.width"),
            "{why}"
        );
        assert!(
            session
                .state
                .lowering_functions()
                .keys()
                .all(|name| name.saved_index().is_none()),
            "the generated function is dropped"
        );
        assert!(
            !session
                .state
                .lowering_functions()
                .contains_key(&lash_kernel_doc::Name::new("w"))
        );
        let message = failed(
            &session
                .cell_under(second[0], "await control.finish(w([1]));")
                .await,
        );
        assert!(message.contains("`w`"), "{message}");
        let message = failed(
            &session
                .cell_under(second[0], "await control.finish(plan.map([1]));")
                .await,
        );
        assert!(message.contains("plan"), "{message}");
    }
}

/// K-SES-002: a `DataQuanta` a lambda-free pipeline built under R1 is a
/// binding later cells read. Under an R2 that kept its shape it
/// dispatches; under one that changed it, its first member call raises
/// `package.stale_value`, naming both releases. After the gate falls back
/// from R2 to R1 (C3), a value R2 built raises the same neutral message.
#[tokio::test(flavor = "multi_thread")]
async fn a_package_value_dispatches_while_its_shape_holds_and_is_stale_once_it_changed() {
    let lash = lash();
    let built = "const q = dataframe.DataQuanta.readText(\"rows\").limit(2);";
    let use_it = "const r = q.limit(1);\nawait control.finish(1);";

    let (held, sets) = hold(&lash, vec![vec![r1(&lash)], vec![r2_kept(&lash)]]);
    let mut session = Session::open(&held, sets[0]).await;
    ok(&session.cell_under(sets[0], built).await);
    assert_eq!(
        finish_of(&session.cell_under(sets[1], use_it).await),
        serde_json::json!(1)
    );

    let (held, sets) = hold(&lash, vec![vec![r1(&lash)], vec![r2_reshaped(&lash)]]);
    let mut session = Session::open(&held, sets[0]).await;
    ok(&session.cell_under(sets[0], built).await);
    let message = failed(&session.cell_under(sets[1], use_it).await);
    assert!(
        message.contains("package.stale_value") && message.contains("(0.3.0; current 0.4.0)"),
        "{message}"
    );

    // Back from R2 to R1: a value R2 built is stale the other way.
    let mut session = Session::open(&held, sets[1]).await;
    ok(&session.cell_under(sets[1], built).await);
    let message = failed(&session.cell_under(sets[0], use_it).await);
    assert!(
        message.contains("package.stale_value") && message.contains("(0.4.0; current 0.3.0)"),
        "{message}"
    );
}

/// S1-F1: on a node that built R2 while a live node holds only R1, the
/// gate answers R1, and both the cell's own code and the saved function
/// relinked into it reach R1's identities alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_the_gate_writes_under_an_older_set_reaches_that_set_alone() {
    let lash = lash();
    let (held, sets) = hold(&lash, vec![vec![r1(&lash)], vec![r2_kept(&lash)]]);
    assert_eq!(held.own(), sets[1]);
    let mut session = Session::open(&held, sets[1]).await;
    ok(&session
        .cell_under(
            sets[1],
            "function keep(n) { return dataframe.col(n).name; }",
        )
        .await);
    assert!(session.listed("keep").is_subset(&session.set(sets[1])));
    ok(&session
        .cell_under(
            sets[0],
            "function fresh(n) { return dataframe.col(n).name; }\nawait control.finish([keep(\"a\"), fresh(\"b\")]);",
        )
        .await);
    for name in ["keep", "fresh"] {
        let listed = session.listed(name);
        assert!(listed.contains(&session.function(sets[0], "dataframe.col")));
        assert!(
            listed.is_subset(&session.set(sets[0])),
            "`{name}` reaches R1's set alone"
        );
    }
}

/// CURRENT-RELEASE: with `p@1` built over `q@1`, and `q@2` installed (so
/// `p` rebuilt over it), a fresh cell and a relinked saved function reach
/// `q` directly and through `p`, and run `q@2` both ways.
#[tokio::test(flavor = "multi_thread")]
async fn a_dependency_closed_set_runs_one_release_of_each_line_directly_and_through_dependents() {
    let lash = lash();
    let p = |q: &Package| {
        Package::build(
            "p",
            "1",
            &PackageSources {
                kernel: "use q.f\n\nfunction p.g() -> Int\nkernel 1\ncharge 1\nbody {\n  let v = invoke q.f()\n  return v\n}\n",
                surface: "function g() -> Int = p.g\n",
                labels: "",
            },
            &[kernel(&lash), q],
        )
        .expect("p builds")
    };
    let (q1, q2) = (q(&lash, "1"), q(&lash, "2"));
    let (p1, p2) = (p(&q1), p(&q2));
    let (held, sets) = hold(&lash, vec![vec![q1, p1], vec![q2, p2]]);
    let mut session = Session::open(&held, sets[0]).await;
    ok(&session
        .cell_under(sets[0], "function both() { return [q.f(), p.g()]; }")
        .await);
    let ran = session
        .cell_under(sets[1], "await control.finish([...both(), q.f(), p.g()]);")
        .await;
    assert_eq!(finish_of(&ran), serde_json::json!([2, 2, 2, 2]));
    assert!(session.listed("both").is_subset(&session.set(sets[1])));
}

/// The prompt lists the code of the packages of the cell's set, in each
/// dialect's spelling, under that set's releases.
#[test]
fn the_prompt_shows_the_cell_set_packages_in_each_dialect_spelling() {
    let lash = lash();
    let (held, sets) = hold(&lash, vec![vec![r1(&lash)], vec![r2_kept(&lash)]]);
    let surface = |set: PackageSetDigest, spelling| {
        crate::package_surface::render(&held.view(&set).expect("a held set"), spelling)
    };
    let typescript = surface(sets[0], lash_kernel_dialect::Spelling::TypeScript);
    for shown in [
        "#### `dataframe` (0.3.0)",
        "`dataframe.col(name: Text) -> dataframe.Expr`",
        "`dataframe.DataQuanta.readText(text: Text) -> dataframe.DataQuanta`",
        "`dataframe.DataQuanta#reduceByKey(",
        "`dataframe.Expr#name: Text`",
        "`dataframe.ExecutionError`: an error",
    ] {
        assert!(typescript.contains(shown), "{shown} in:\n{typescript}");
    }
    let python = surface(sets[0], lash_kernel_dialect::Spelling::Python);
    for shown in [
        "`dataframe.DataQuanta.read_text(text: Text) -> dataframe.DataQuanta`",
        "`dataframe.DataQuanta#reduce_by_key(",
    ] {
        assert!(python.contains(shown), "{shown} in:\n{python}");
    }
    assert!(
        surface(sets[1], lash_kernel_dialect::Spelling::TypeScript)
            .contains("#### `dataframe` (0.4.0)")
    );
    assert!(
        !typescript.contains("`run("),
        "operations are the tools' declarations"
    );
}

/// A session's runtime state on `held`'s node, its rounds written under
/// the sets `gate` answers.
fn runtime_state(
    held: &Arc<HeldPackages>,
    gate: Arc<dyn crate::plugin::PackageGate>,
) -> RlmRuntimeState {
    let mut services = crate::dialect::test_dialect_services();
    services.workers = lash_vm_client::service::Service::default().with_packages(Arc::clone(held));
    services.kernel = super::super::KernelCarry::holding(Some(Arc::clone(held)), None);
    services.packages = Some(gate);
    RlmRuntimeState::new(Arc::new(crate::dialect::SessionDialect::new(
        CellDialect::typescript(),
        services,
    )))
    .expect("the session's state")
}

/// P3: the gate is asked once per round. When the fleet's answer moves
/// between a round's prompt and its cell, the cell is written under the
/// set the prompt showed; the next round's prompt shows the set the gate
/// answers then, and its cell runs that set.
#[tokio::test(flavor = "multi_thread")]
async fn a_round_cell_is_written_under_the_set_its_prompt_showed() {
    let lash = lash();
    let (held, sets) = hold(&lash, vec![vec![q(&lash, "1")], vec![q(&lash, "2")]]);
    let gate = Arc::new(Switch::new(&held, sets[0]));
    let state = runtime_state(&held, Arc::clone(&gate) as _);
    let host = open_host().await;
    let render = crate::testing::recorded_test_render();
    for (round, (shown, moved_to, value)) in [(sets[0], sets[1], 1), (sets[1], sets[0], 2)]
        .into_iter()
        .enumerate()
    {
        gate.answer(shown);
        let facts = state
            .prompt_facts(Some(&render), None, true, None)
            .await
            .expect("the round's prompt");
        let surface = facts.packages.expect("the prompt shows the set's packages");
        assert!(
            surface.contains(&format!("#### `q` ({value})")),
            "{surface}"
        );
        // The fleet moves before the round's cell runs.
        gate.answer(moved_to);
        let key: &'static str = Box::leak(format!("exec-code:{round}").into_boxed_str());
        let ran = Box::pin(state.execute_code(
            cell_context(&host, SESSION, TURN, key, Arc::new(CellTools::default())),
            lash_core::ExecRequest {
                code: "await control.finish(q.f());".to_owned(),
            },
        ))
        .await
        .expect("the round's cell");
        assert_eq!(
            finish_of(&ran),
            serde_json::json!(value),
            "the cell runs the set its prompt showed"
        );
        state
            .settle_code_execution(lash_core::plugin::CodeExecutionOutcome::Accepted)
            .await
            .expect("the cell settles");
        crate::testing::commit_phase(&state, &host).await;
    }
}

/// A gate with no set to answer yet.
struct Unanswered;

#[async_trait::async_trait]
impl crate::plugin::PackageGate for Unanswered {
    async fn writable(
        &self,
    ) -> Result<crate::CellPackages, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RunDefinitionUnavailable,
            "no package set is held by every live node serving these sessions",
        )
        .retryable_uncommitted_derivation())
    }
}

/// P3: a round whose gate cannot answer has no prompt. The gate's typed
/// refusal is the prompt's error, never a prompt without its package
/// section, and the turn defers the call as a live fault.
#[tokio::test(flavor = "multi_thread")]
async fn a_round_whose_gate_cannot_answer_has_no_prompt_and_defers_typed() {
    let lash = lash();
    let (held, _) = hold(&lash, vec![vec![r1(&lash)]]);
    let state = runtime_state(&held, Arc::new(Unanswered));
    let render = crate::testing::recorded_test_render();
    let Err(error) = state.prompt_facts(Some(&render), None, true, None).await else {
        panic!("a prompt rendered while the gate had no answer");
    };
    assert!(
        matches!(
            &error,
            lash_core::SessionError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(refused)
            ) if refused.code == lash_core::RuntimeErrorCode::RunDefinitionUnavailable
        ),
        "{error:?}"
    );
    assert!(
        matches!(
            lash_core::ExecutionEnvironmentSyncError::of_session_error(
                lash_core::sansio::ExecutionEnvironmentSyncFailureKind::ProtocolFacts,
                error,
            ),
            lash_core::ExecutionEnvironmentSyncError::Live(_)
        ),
        "the turn defers the call"
    );
}

/// The cell a reclaim law parks: each call it makes is held, and a call a
/// node died in is caught as interrupted.
const PARKING_CELL: &str = "let first = \"unset\";\ntry {\n  first = await echo.say({ text: \"held\" });\n} catch (error) {\n  first = \"interrupted\";\n}\nlet second = \"unset\";\ntry {\n  second = await echo.say({ text: \"held\" });\n} catch (error) {\n  second = \"interrupted\";\n}";

/// Runs [`PARKING_CELL`] in `state` on `host`'s node until its next held
/// call starts, the cell parked on it.
async fn park_at_the_held_call(
    state: &mut super::super::CodeModeExecutionState,
    host: &DurableHost,
    services: &super::super::CellServices,
    tools: &Arc<CellTools>,
) {
    let cell = run_cell(
        state,
        cell_context(host, SESSION, TURN, "exec-code:1", tools.clone()),
        services,
        PARKING_CELL,
    );
    tokio::select! {
        response = cell => panic!("the cell ended with its call held: {:?}", response.result),
        () = tools.held_started.notified() => {}
    }
}

/// CLAIM-HOLDS across a reclaim: a cell written under the set the gate
/// answers stamps its session with that set when it parks, so no node
/// lacking the set claims the session while the cell stands parked. On
/// the node that reclaims it, the turn's working state is restored and
/// committed, which clears a stamp the restored state cannot know; the
/// resumed cell parks again under the set its envelope records, and the
/// session is stamped with it again. Once the cell ends, the turn's
/// capture after it clears the stamp.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_cell_keeps_its_session_stamped_with_its_set_across_a_reclaim() {
    let held = lash_vm_runtime::lash_packages().expect("lash's packages");
    let own = held.own();
    let mut host = DurableHost::open_holding(
        lash_core::AdmittedScope::turn(
            lash_core::SessionId::from(SESSION),
            lash_core::TurnId::from(TURN),
        ),
        own,
    )
    .await;
    let mut services = cell_services(
        &CellDialect::typescript(),
        lash_vm_client::service::Service::default(),
    );
    services.packages = Some(Arc::new(Switch::new(&held, own)) as _);
    let tools = Arc::new(CellTools::default());

    // A first cell leaves the turn a working state to restore.
    let mut state = crate::testing::session_state(&CellDialect::typescript());
    ok(&run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const before = 1;",
    )
    .await);
    let (capture, _) = crate::testing::commit_working(&mut state, &host).await;
    let lash_core_store::execution_state::ExecutionGraphCapture::Replace(working) = capture else {
        panic!("the first cell leaves a working state");
    };
    let restored = || {
        let mut state = crate::testing::session_state(&CellDialect::typescript());
        state
            .restore_execution_graph(
                &working,
                lash_core_store::execution_state::ExecutionGraphPurpose::Working,
            )
            .expect("the working state restores");
        state
    };

    tools.hold.store(true, Ordering::SeqCst);
    park_at_the_held_call(&mut state, &host, &services, &tools).await;
    assert!(
        !host.kill_and_resume_past_a_lacking_node().await,
        "a node lacking the cell's set claimed its session at the cell's first park"
    );
    let mut state = restored();
    crate::testing::commit_working(&mut state, &host).await;
    park_at_the_held_call(&mut state, &host, &services, &tools).await;
    assert!(
        !host.kill_and_resume_past_a_lacking_node().await,
        "a node lacking the cell's set claimed its session after the cell parked again"
    );

    tools.hold.store(false, Ordering::SeqCst);
    let mut state = restored();
    ok(&run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        PARKING_CELL,
    )
    .await);
    crate::testing::commit_working(&mut state, &host).await;
    assert!(
        host.kill_and_resume_past_a_lacking_node().await,
        "a session between cells pins no code"
    );
}

/// `dataframe.run`, served by a fake that keeps each plan it receives and
/// answers one row per step.
#[derive(Default)]
struct Runs {
    received: Mutex<Vec<serde_json::Value>>,
}

fn run_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:dataframe_run",
        "dataframe.run",
        "Run a dataframe plan",
        // The plan is the input, its callback a function slot.
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "step": { "type": "string" },
                    "exprs": { "type": "array" },
                    "callback": { "x-lash": { "kind": "function" } },
                    "n": { "type": "integer" }
                },
                "required": ["step", "exprs"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({ "type": "array" }),
    )
    .expect("valid declared tool schemas")
    .declared_contract(lash_core::ContractId::new("dataframe.run/1"))
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Runs {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash_core::ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
    {
        Ok(vec![run_tool().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        let tool = run_tool();
        (name == tool.manifest.tool() || name == tool.id().as_str())
            .then(|| Arc::new(tool.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let steps = call.args.as_array().map_or(0, Vec::len);
        self.received
            .lock()
            .expect("received plans")
            .push(call.args.clone());
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(
            serde_json::json!(vec!["row"; steps]),
        ))
    }
}

/// PKG-CARRY: a package value holding only user closures carries and
/// crosses. The `DataQuanta` a cell's `flat_map` builds holds the user's
/// lambda as given (§2.4); it carries to the next cell, its lambda saved
/// under a generated name, and its `collect_rows` there crosses the plan
/// into `dataframe.run`, the lambda a function value. Each dialect, in its
/// own spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_package_value_holding_a_user_lambda_carries_and_crosses_in_the_next_cell() {
    let lash = lash();
    let (held, sets) = hold(&lash, vec![vec![r1(&lash)]]);
    for (dialect, built, collected) in [
        (
            CellDialect::typescript(),
            "const q = dataframe.DataQuanta.readText(\"a b\").flatMap((line: string) => line.split(\" \"), { output: \"utf8\" });",
            "const rows = await q.collectRows();\nawait control.finish(rows);",
        ),
        (
            CellDialect::python(),
            "q = dataframe.DataQuanta.read_text(\"a b\").flat_map(lambda line: line.split(), output=\"utf8\")",
            "rows = await q.collect_rows()\nawait control.finish(rows)",
        ),
    ] {
        let runs = Arc::new(Runs::default());
        let mut session = Session::open_in(dialect, &held, sets[0])
            .await
            .serving(Arc::clone(&runs) as Arc<dyn lash_core::ToolProvider>);
        let made = session.cell_under(sets[0], built).await;
        ok(&made);
        assert!(made.bindings.not_carried.is_empty(), "{:?}", made.bindings);
        assert!(
            session
                .state
                .lowering_functions()
                .keys()
                .any(|name| name.saved_index().is_some()),
            "the lambda is saved under a generated name"
        );
        assert_eq!(
            finish_of(&session.cell_under(sets[0], collected).await),
            serde_json::json!(["row"])
        );
        let received = runs.received.lock().expect("received plans").clone();
        let callback = &received[0][0]["callback"];
        assert!(
            callback.get(lash_kernel_doc::FUNCTION_VALUE_TAG).is_some(),
            "the lambda crossed as a function value: {callback}"
        );
    }
}

/// P3 / R3: a displayed snapshot stays authoritative even on a node whose
/// worker cannot serve it; asking the gate again could disclose newly
/// public names.
#[tokio::test]
async fn an_unavailable_shown_snapshot_is_refused_without_a_second_gate_answer() {
    struct Counts(std::sync::atomic::AtomicUsize, crate::CellPackages);
    #[async_trait::async_trait]
    impl crate::plugin::PackageGate for Counts {
        async fn writable(
            &self,
        ) -> Result<crate::CellPackages, lash_core::RuntimeEffectControllerError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(self.1.clone())
        }
    }
    let held = lash_vm_runtime::lash_packages().unwrap();
    let own = crate::CellPackages::held(&held, &held.own()).expect("lash's own set");
    let lash = lash();
    let (other, sets) = hold(&lash, vec![vec![r1(&lash)]]);
    // The worker holds lash's packages alone.
    let unavailable = sets[0];
    let gate = Arc::new(Counts(std::sync::atomic::AtomicUsize::new(0), own));
    let mut services = cell_services(
        &CellDialect::typescript(),
        lash_vm_client::service::Service::default(),
    );
    services.packages = Some(gate.clone());
    let mut state = crate::testing::session_state(&CellDialect::typescript());
    state.shows_packages(held_cell(&other, unavailable));
    let host = open_host().await;
    let response = run_cell(
        &mut state,
        cell_context(
            &host,
            SESSION,
            TURN,
            "exec-code:0",
            Arc::new(CellTools::default()),
        ),
        &services,
        "const safe = 1;",
    )
    .await;
    assert!(
        matches!(response.error().map(|failure| &failure.kind), Some(lash_core::CellFailureKind::PackagesNotHeld { packages }) if *packages == unavailable),
        "{:?}",
        response.result
    );
    assert_eq!(
        gate.0.load(Ordering::SeqCst),
        0,
        "the displayed snapshot is the round's only answer"
    );
}

/// SNAPSHOT-SERVING (FIG-6991): a node holding R1 alone writes cells
/// under B, a set it does not hold, from B's stored surface and the
/// definitions the store holds for it. The prompt shows B's packages under
/// B's release, its new member `same` included; a function saved under R1
/// relinks to B's `col`; and a cell naming `same`, which no document used
/// before, lowers against B's names, where under R1 it is no name. Running
/// B's code on the node is the loader's (FIG-6985).
#[tokio::test(flavor = "multi_thread")]
async fn a_node_writes_cells_under_a_stored_surface_it_does_not_hold() {
    use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
    use lash_vm_client::service::{Request, Response};

    let lash = lash();
    let (a, a_sets) = hold(&lash, vec![vec![r1(&lash)]]);
    let (b, b_sets) = hold(&lash, vec![vec![r2_grown(&lash)]]);
    let r1 = a_sets[0];
    let stored = stored_cell(&a, &b, b_sets[0]);
    assert!(
        !stored.stored_definitions().is_empty(),
        "the node lacks B's code"
    );
    let shown =
        crate::package_surface::render(&stored.view(), lash_kernel_dialect::Spelling::TypeScript);
    for line in [
        "#### `dataframe` (0.4.0)",
        "`dataframe.same(n: Int) -> Int`",
    ] {
        assert!(shown.contains(line), "{line} in:\n{shown}");
    }
    let b_view = b.view(&b_sets[0]).expect("B");
    let b_function = |name: &str| {
        b_view
            .definitions()
            .find(|(defined, _)| *defined == name)
            .map(|(_, function)| function)
            .expect("B defines it")
    };
    let b_set: BTreeSet<FunctionId> = b_view.definitions().map(|(_, function)| function).collect();

    let mut session = Session::open(&a, r1).await;
    ok(&session
        .cell_under(r1, "function keep(n) { return dataframe.col(n).name; }")
        .await);
    session.state.relink(&stored).await.expect("relinked");
    let keep = session.listed("keep");
    assert!(
        keep.contains(&b_function("dataframe.col")),
        "`keep` reaches B's col"
    );
    assert!(keep.is_subset(&b_set), "`keep` reaches B's set alone");

    let workers = session
        .services
        .workers
        .begin_execution(std::time::Duration::ZERO);
    let lower = |packages: lash_vm_client::service::SurfaceRef| Request::Lower {
        dialect: session.state.dialect().to_owned(),
        source: "function fresh(n) { return dataframe.same(n); }".to_owned(),
        operations: Default::default(),
        bindings: session.state.names(),
        functions: session.state.lowering_functions(),
        packages,
    };
    let under_r1 = workers
        .request_accounted(lower(lash_vm_client::service::SurfaceRef::held(r1)))
        .await;
    assert!(
        !matches!(under_r1, Ok(Response::Lowered { .. })),
        "under R1, `same` is no name: {under_r1:?}"
    );
    let Ok(Response::Lowered { document, .. }) = workers
        .request_accounted(lower(crate::plugin::package_wire(&stored)))
        .await
    else {
        panic!("the cell lowers under B's stored surface");
    };
    let document = lash_kernel_doc::IdentifiedDocument::decode(
        &document,
        lash_kernel_doc::KernelDataLimits::standard(),
    )
    .expect("a document");
    let listed: BTreeSet<FunctionId> = document.manifest.functions.keys().copied().collect();
    assert!(listed.contains(&b_function("dataframe.same")), "B's `same`");
    assert!(listed.is_subset(&b_set), "the cell reaches B's set alone");
}

/// NEVER-USED-MEMBER (FIG-6985): a node holding R1 alone runs B's new
/// member `same`, which no document ever reached, in a cell written under
/// B's stored surface, from the definitions B's install stored under its
/// pin and nothing else: the cell's declarations are read from the store
/// and checked, and its run executes B's body. Before the loader read
/// them, the cell was refused for want of a declaration the node held.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_runs_a_member_no_document_reached_from_the_code_its_install_stored() {
    let lash = lash();
    let (a, a_sets) = hold(&lash, vec![vec![r1(&lash)]]);
    let (b, b_sets) = hold(&lash, vec![vec![r2_grown(&lash)]]);
    let stored = stored_cell(&a, &b, b_sets[0]);
    let mut session = Session::open(&a, a_sets[0]).await;
    // What B's install writes (FIG-6991): the code the node lacks, under
    // the set's pin.
    let images: Vec<(FunctionId, Arc<[u8]>)> = stored
        .stored_definitions()
        .iter()
        .map(|(function, definition)| {
            (
                *function,
                Arc::from(lash_kernel_doc::graph_tokens::encode_body(
                    definition.as_ref(),
                )),
            )
        })
        .collect();
    lash_vm_runtime::KernelDocuments::new(session.host.context().backend().module_artifacts())
        .hold_definitions(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &images,
        )
        .await
        .expect("B's install stores its code");
    let ran = session
        .cell_with(stored, "await control.finish(dataframe.same(7));")
        .await;
    assert_eq!(finish_of(&ran), serde_json::json!(7));
}
