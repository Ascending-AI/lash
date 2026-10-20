//! Sessions on the cell's package set (FIG-6926): each cell runs on the
//! set the gate answers, its carried saved functions relinked to that set
//! by name (RELINK-CLOSURE, K-LIB-009), so everything one cell runs comes
//! from one set (CURRENT-RELEASE), and a package value built by another
//! release of its package raises `package.stale_value` naming both
//! (K-SES-002 carries it). The node holds every release, the earlier ones
//! retained beside its own.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{FunctionId, PackageSetDigest};
use lash_kernel_package::{HeldPackages, Package, PackageSet, PackageSources, testing};

use super::{CellTools, SESSION, TURN, finish_of, open_host};
use crate::CellDialect;
use crate::testing::{DurableHost, cell_context, cell_services, run_cell};

/// A gate a law moves between sets, as a fleet's would.
struct Switch(Mutex<PackageSetDigest>);

#[async_trait::async_trait]
impl crate::plugin::PackageGate for Switch {
    async fn writable(
        &self,
    ) -> Result<crate::CellPackages, lash_core::RuntimeEffectControllerError> {
        Ok(crate::CellPackages(*self.0.lock().expect("the gate")))
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

/// A session of `held`'s node on `host`, its cells written under the set
/// `gate` answers.
struct Session {
    host: DurableHost,
    state: super::super::CodeModeExecutionState,
    services: super::super::CellServices,
    gate: Arc<Switch>,
    tools: Arc<CellTools>,
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
        let gate = Arc::new(Switch(Mutex::new(first)));
        let mut services = cell_services(
            &dialect,
            lash_vm_client::service::Service::default().with_packages(Arc::clone(held)),
        );
        services.kernel = super::super::KernelCarry::holding(Some(Arc::clone(held)), None);
        services.packages = Some(Arc::clone(&gate) as Arc<dyn crate::plugin::PackageGate>);
        Self {
            host: open_host().await,
            state: crate::testing::session_state(&dialect),
            services,
            gate,
            tools: Arc::new(CellTools::default()),
            held: Arc::clone(held),
            cells: 0,
        }
    }

    async fn cell_under(&mut self, set: PackageSetDigest, code: &str) -> lash_core::ExecResponse {
        *self.gate.0.lock().expect("the gate") = set;
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
/// named with the callee, and is no binding of a later cell.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_function_the_cell_set_does_not_serve_is_dropped_naming_it_and_the_callee() {
    let lash = lash();
    for r2 in [r2_removed(&lash), r2_resigned(&lash)] {
        let (held, sets) = hold(&lash, vec![vec![r1(&lash)], vec![r2]]);
        let mut session = Session::open(&held, sets[0]).await;
        ok(&session
            .cell_under(sets[0], "function w(row) { return dataframe.width(row); }")
            .await);
        ok(&session.cell_under(sets[1], "const other = 1;").await);
        let why = session.state.not_carried()[&lash_kernel_doc::Name::new("w")].clone();
        assert!(
            why.contains("`w`") && why.contains("dataframe.width"),
            "{why}"
        );
        assert!(
            !session
                .state
                .lowering_functions()
                .contains_key(&lash_kernel_doc::Name::new("w"))
        );
        let message = failed(
            &session
                .cell_under(sets[1], "await control.finish(w([1]));")
                .await,
        );
        assert!(message.contains("`w`"), "{message}");
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
    let q = |value: &str| {
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
            &[kernel(&lash)],
        )
        .expect("q builds")
    };
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
    let (q1, q2) = (q("1"), q("2"));
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
