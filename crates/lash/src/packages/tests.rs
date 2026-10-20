//! FLEET-SET and RETAIN-SURVEY (FIG-6918) over two lines of host packages:
//! `p`, whose body calls `q.v`, built over `q@1` and again over `q@2`.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::durable_port::{FormatSet, HeldArtifact, NodeCapability, PackageSetDigest};
use lash_kernel_package::{HeldPackages, Package, PackageSet, PackageSources};
use lash_vm_runtime::{RetainDependent, RetainSurvey};

use super::{NodeHold, fleet_package_set, held_artifact, members, serving};

fn q(value: i64) -> Package {
    let kernel =
        format!("function q.v() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n");
    Package::build(
        "q",
        &format!("{value}.0.0"),
        &PackageSources {
            kernel: &kernel,
            surface: "function v() -> Int = q.v\n",
            labels: "v\tvalue\n",
        },
        &[],
    )
    .expect("q builds")
}

fn p(q: &Package) -> Package {
    Package::build(
        "p",
        "1.0.0",
        &PackageSources {
            kernel: "use q.v\n\nfunction p.f() -> Int\nkernel 1\ncharge 2\nbody {\n  let value = invoke q.v()\n  return value\n}\n",
            surface: "function f() -> Int = p.f\n",
            labels: "f\tf\n",
        },
        &[q],
    )
    .expect("p builds")
}

/// `p@1` over `q@1`, and the same source of `p` rebuilt over `q@2`.
struct Lines {
    q1: Package,
    p1: Package,
    q2: Package,
    p2: Package,
}

fn lines() -> Lines {
    let q1 = q(1);
    let p1 = p(&q1);
    let q2 = q(2);
    let p2 = p(&q2);
    Lines { q1, p1, q2, p2 }
}

fn set(packages: &[&Package]) -> PackageSet {
    PackageSet::new(packages.iter().copied()).expect("a closed set")
}

/// A node holding both lines, the newer its own set.
fn both(lines: &Lines) -> Arc<HeldPackages> {
    let old = set(&[&lines.p1, &lines.q1]);
    let new = set(&[&lines.p2, &lines.q2]);
    let own = new.digest();
    Arc::new(
        HeldPackages::new(
            [&lines.q1, &lines.p1, &lines.q2, &lines.p2]
                .into_iter()
                .map(|package| Arc::new(package.clone())),
            [old, new],
            own,
        )
        .expect("both lines"),
    )
}

fn artifacts(packages: &[&Package]) -> BTreeSet<HeldArtifact> {
    packages
        .iter()
        .map(|package| held_artifact(&package.key()))
        .collect()
}

fn node(decodes: &[&FormatSet], held: BTreeSet<HeldArtifact>) -> NodeCapability {
    NodeCapability {
        decodes: decodes.iter().map(|set| (*set).clone()).collect(),
        held,
    }
}

fn stored(sets: &[(PackageSetDigest, BTreeSet<HeldArtifact>, u64)]) -> super::PackageCatalog {
    super::PackageCatalog {
        artifacts: Vec::new(),
        sets: sets
            .iter()
            .map(
                |(digest, members, sequence)| lash_core::durable_port::StoredSet {
                    digest: *digest,
                    members: members.clone(),
                    sequence: *sequence,
                },
            )
            .collect(),
    }
}

/// FLEET-SET: rebuilding `p` over `q@2` makes a new artifact, and a cell
/// is written under `{p', q@2}` or `{p@1, q@1}`, never a mix: own set
/// first, then the other sets the node holds whole, newest first by the
/// store's install sequence. While a live serving node lacks the own set,
/// the gate falls back to the older set; a node serving only carried
/// sessions counts; once none lacks it, the own set is written.
#[test]
fn a_cell_is_written_under_one_whole_set_every_serving_node_holds() {
    let lines = lines();
    assert_ne!(
        lines.p1.digest(),
        lines.p2.digest(),
        "p rebuilt over q@2 is a new artifact"
    );
    let packages = both(&lines);
    let old = set(&[&lines.p1, &lines.q1]).digest();
    let new = packages.own();
    let hold = NodeHold::new(Arc::clone(&packages));
    *hold.advertised.write().unwrap() =
        Some(artifacts(&[&lines.q1, &lines.p1, &lines.q2, &lines.p2]));
    // A third, older retained set the store installed first: tried last.
    let catalog = stored(&[
        (old, artifacts(&[&lines.p1, &lines.q1]), 2),
        (new, artifacts(&[&lines.p2, &lines.q2]), 3),
    ]);
    let candidates = hold.candidates(&catalog);
    assert_eq!(
        candidates
            .iter()
            .map(|(digest, _)| *digest)
            .collect::<Vec<_>>(),
        vec![new, old],
        "own first, then newest first"
    );
    for (_, members) in &candidates {
        let lines: BTreeSet<&str> = members.iter().map(|member| member.path.as_str()).collect();
        assert_eq!(
            lines.len(),
            members.len(),
            "a candidate is one artifact a line"
        );
    }

    let session = FormatSet::new("session:law");
    let carried = FormatSet::new("session:law-previous");
    let this = node(&[&session], hold.held());
    let older = node(&[&session], artifacts(&[&lines.p1, &lines.q1]));
    let live = vec![this.clone(), older];
    assert_eq!(
        fleet_package_set(&candidates, &serving(&live, &session, &[])),
        Some(old),
        "a live node lacking the own set holds the cell to the older set"
    );
    let carrying = node(&[&carried], artifacts(&[&lines.p1, &lines.q1]));
    let live = vec![this.clone(), carrying.clone()];
    assert_eq!(
        fleet_package_set(
            &candidates,
            &serving(&live, &session, std::slice::from_ref(&carried))
        ),
        Some(old),
        "a node serving only carried sessions co-serves the cell"
    );
    assert_eq!(
        fleet_package_set(&candidates, &serving(&live, &session, &[])),
        Some(new),
        "a node serving no session of this build does not hold the gate back"
    );
    let mixed = node(&[&session], artifacts(&[&lines.p1, &lines.q2]));
    assert_eq!(
        fleet_package_set(&candidates, &serving(&[this.clone(), mixed], &session, &[])),
        None,
        "a node holding a mix holds no candidate whole: nothing is written"
    );
    assert_eq!(
        fleet_package_set(&candidates, &serving(&[this], &session, &[])),
        Some(new)
    );
}

/// RETAIN-SURVEY: an artifact outside the own set is retained while a
/// parked root, an unfinished process (started or not), a saved function
/// or a published definition lists one of its identities, reached through
/// the bodies of what it lists; with none, it is dropped, and a set naming
/// it stops being a candidate with no set row edited. An identity no held
/// artifact declares strands its dependent.
#[test]
fn an_artifact_is_retained_exactly_while_a_dependent_lists_it() {
    let lines = lines();
    let packages = both(&lines);
    let old = set(&[&lines.p1, &lines.q1]);
    let old_f = lines.p1.function("p.f").expect("p@1's f");
    let session = lash_core::SessionId::parse("retain-law").expect("a session id");
    let dependents = [
        RetainDependent::ParkedCell {
            session: session.clone(),
            cell: "exec-code:0".into(),
        },
        RetainDependent::Process {
            process: lash_core::ProcessId::fixture("unstarted"),
            started: false,
        },
        RetainDependent::SavedFunction {
            session: session.clone(),
            name: "advance".into(),
        },
        RetainDependent::PublishedDefinition {
            document: lash_kernel_doc::DocumentId::from_bytes([3; 32]),
        },
    ];
    for dependent in dependents {
        let survey = RetainSurvey::of(&packages, [(dependent.clone(), BTreeSet::from([old_f]))]);
        assert_eq!(
            survey.retained.keys().cloned().collect::<BTreeSet<_>>(),
            old.keys().collect(),
            "{dependent:?} retains p@1 and, through its body, q@1"
        );
        assert!(
            survey
                .retained
                .values()
                .all(|by| by == &vec![dependent.clone()])
        );
        assert!(survey.stranded.is_empty());
        assert_eq!(
            survey.held(&packages),
            packages.artifacts().keys().cloned().collect()
        );
    }

    let dropped = RetainSurvey::of(&packages, []);
    assert!(dropped.retained.is_empty());
    let hold = NodeHold::new(Arc::clone(&packages));
    *hold.advertised.write().unwrap() =
        Some(dropped.held(&packages).iter().map(held_artifact).collect());
    let catalog = stored(&[
        (old.digest(), members(&old), 1),
        (packages.own(), artifacts(&[&lines.p2, &lines.q2]), 2),
    ]);
    assert_eq!(
        hold.candidates(&catalog)
            .iter()
            .map(|(digest, _)| *digest)
            .collect::<Vec<_>>(),
        vec![packages.own()],
        "a set naming a dropped artifact is no candidate"
    );

    let unknown = lash_kernel_doc::FunctionId::from_bytes([5; 32]);
    let stranded = RetainSurvey::of(
        &packages,
        [(
            RetainDependent::Process {
                process: lash_core::ProcessId::fixture("started"),
                started: true,
            },
            BTreeSet::from([unknown]),
        )],
    );
    assert_eq!(stranded.stranded.len(), 1);
    assert_eq!(stranded.stranded[0].function, unknown);
}
