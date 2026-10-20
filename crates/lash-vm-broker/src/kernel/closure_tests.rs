//! K-MACH-008 (FIG-6918): a parked run resumes by function identity. A run
//! whose document lists release R1's `p.f` resumes on a worker holding R1
//! beside R2, and a worker holding R2 alone refuses it, naming the function.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_package::{HeldPackages, Package, PackageSet, PackageSources};

use super::{ClosureRefusal, Natives, derive};

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

fn held(lines: &[&[&Package]]) -> HeldPackages {
    let sets: Vec<PackageSet> = lines
        .iter()
        .map(|line| PackageSet::new(line.iter().copied()).expect("a closed set"))
        .collect();
    let own = sets.last().expect("an own set").digest();
    HeldPackages::new(
        lines
            .iter()
            .flat_map(|line| line.iter().map(|package| Arc::new((*package).clone()))),
        sets,
        own,
    )
    .expect("the held lines")
}

#[test]
fn a_parked_run_resumes_by_identity_where_its_release_is_held_and_names_what_is_not() {
    let (q1, q2) = (q(1), q(2));
    let (p1, p2) = (p(&q1), p(&q2));
    let r1 = [&p1, &q1];
    let r2 = [&p2, &q2];
    let f1 = p1.function("p.f").expect("R1's p.f");
    let v1 = q1.function("q.v").expect("R1's q.v");

    let both = held(&[&r1, &r2]);
    let closure =
        derive([f1], both.functions(), Natives::Required).expect("R1+R2 resumes R1's run");
    assert_eq!(
        closure
            .body()
            .functions()
            .iter()
            .map(|function| function.function)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([f1, v1]),
        "the closure is what R1's p.f reaches, by identity, and nothing of R2"
    );

    let newer = held(&[&r2]);
    assert_eq!(
        derive([f1], newer.functions(), Natives::Required).map(|_| ()),
        Err(ClosureRefusal::Missing { function: f1 }),
        "R2 alone refuses the resume, naming R1's function"
    );
}
