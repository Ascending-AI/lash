//! What a package build and an install refuse, and what an artifact's
//! identity is.

use std::sync::Arc;

use lash_kernel_doc::{Datum, Type};

use super::{SHOUT, finished, functions, kernel, kernel_with, run, runtime};
use crate::testing::{DATAFRAME, RELEASE, dataframe};
use crate::{
    Declared, Export, HeldPackages, NameRole, Package, PackageRefusal, PackageSet, PackageSources,
    Segment, SetHolds, TypeMember,
};

fn segment(text: &str) -> Segment {
    Segment::new(text).unwrap()
}

/// The fixture with its surface `surface`.
fn with_surface(surface: &str) -> Result<Package, PackageRefusal> {
    let kernel = kernel();
    Package::build(
        "dataframe",
        RELEASE,
        &PackageSources {
            surface,
            ..DATAFRAME
        },
        &[&kernel],
    )
}

/// The fixture with its kernel source `kernel_source`.
fn with_kernel(surface: &str, kernel_source: &str) -> Result<Package, PackageRefusal> {
    let kernel = kernel();
    Package::build(
        "dataframe",
        RELEASE,
        &PackageSources {
            kernel: kernel_source,
            surface,
            ..DATAFRAME
        },
        &[&kernel],
    )
}

fn member<'a>(package: &'a Package, ty: &str, name: &str) -> &'a TypeMember {
    let Some(Export::Type(ty)) = package.exports().get(&segment(ty)) else {
        panic!("`{ty}` is a type");
    };
    &ty.members[&segment(name)]
}

/// EXPORT-ONE: a package has one export per canonical segment, and a type
/// one member per segment; a repeated segment is refused naming both
/// declarations; an error's kernel kind is its path.
#[test]
fn export_one_a_segment_is_declared_once_and_an_errors_kind_is_its_path() {
    let built = dataframe(&kernel()).unwrap();
    let (data_sort, expr_sort) = (
        member(&built, "data_quanta", "sort"),
        member(&built, "expr", "sort"),
    );
    assert_ne!(
        data_sort.member().signature,
        expr_sort.member().signature,
        "each type's `sort` keeps its own slots"
    );
    assert!(data_sort.member().rest && !expr_sort.member().rest);
    assert_eq!(
        built.error_kind(&segment("execution_error")).as_deref(),
        Some("dataframe.execution_error")
    );
    assert_eq!(built.error_kind(&segment("count")), None);

    let twice = format!(
        "{}function run(plan: Any) -> List(Any) = dataframe.count\n",
        DATAFRAME.surface
    );
    let refused = with_surface(&twice).unwrap_err();
    assert_eq!(
        refused,
        PackageRefusal::Repeated {
            scope: "dataframe".into(),
            segment: segment("run"),
            first: Declared::Operation,
            second: Declared::Function,
        }
    );
    assert_eq!(
        refused.to_string(),
        "`run` is declared twice in `dataframe`: an operation and a function"
    );

    let member_twice = DATAFRAME.surface.replace(
        "  property name(self: expr)",
        "  property sort(self: expr) -> Text = dataframe.expr.name\n  property name(self: expr)",
    );
    assert_eq!(
        with_surface(&member_twice).unwrap_err(),
        PackageRefusal::Repeated {
            scope: "dataframe.expr".into(),
            segment: segment("sort"),
            first: Declared::Method,
            second: Declared::Property,
        }
    );
}

/// PKG-SURFACE: a member without a definition, or with another slot list,
/// is refused.
#[test]
fn pkg_surface_a_member_without_its_definition_or_slots_is_refused() {
    let without = format!(
        "{}function first(values: List(Any)) -> Any = dataframe.first\n",
        DATAFRAME.surface
    );
    assert!(matches!(
        with_surface(&without).unwrap_err(),
        PackageRefusal::NoDefinition { member, definition, .. }
            if member.to_string() == "first" && definition.as_str() == "dataframe.first"
    ));

    let other_slots = DATAFRAME.surface.replace(
        "method alias(self: expr, name: Text)",
        "method alias(self: expr, name: Int)",
    );
    assert!(matches!(
        with_surface(&other_slots).unwrap_err(),
        PackageRefusal::OtherSlots { member, member_signature, definition_signature, .. }
            if member.to_string() == "expr.alias"
                && member_signature.params[1].ty == Type::Int
                && definition_signature.params[1].ty == Type::Text
    ));
}

/// PKG-NAMES: a non-canonical path, export or member segment is refused
/// naming it; definition names are not checked against the grammar.
#[test]
fn pkg_names_a_non_canonical_segment_is_refused_naming_it() {
    let kernel = kernel();
    for (path, text) in [("Dataframe", "Dataframe"), ("data.Frame", "Frame")] {
        assert_eq!(
            Package::build(path, RELEASE, &DATAFRAME, &[&kernel]).unwrap_err(),
            PackageRefusal::NotCanonical {
                role: NameRole::Path,
                text: text.into(),
            }
        );
    }
    let export = DATAFRAME
        .surface
        .replace("function col(", "function readCol(");
    assert_eq!(
        with_surface(&export).unwrap_err(),
        PackageRefusal::NotCanonical {
            role: NameRole::Export,
            text: "readCol".into(),
        }
    );
    let member = DATAFRAME
        .surface
        .replace("method flat_map(", "method flatMap(");
    assert_eq!(
        with_surface(&member).unwrap_err(),
        PackageRefusal::NotCanonical {
            role: NameRole::Member,
            text: "flatMap".into(),
        }
    );
    let surface = format!(
        "{}function helper() -> Int = dataframe.helperOne\n",
        DATAFRAME.surface
    );
    let source = format!(
        "{}\nfunction dataframe.helperOne() -> Int\nkernel 1\ncharge 1\nbody {{\n  return 1\n}}\n",
        DATAFRAME.kernel
    );
    with_kernel(&surface, &source).expect("a definition name is not a canonical name");
}

/// PKG-ROOTS: a host package's definition outside its path, or a host
/// path whose first segment a lash package lists as a root, is refused;
/// the reserved roots are exactly the roots lash's packages list.
#[test]
fn pkg_roots_a_host_package_lives_under_its_own_path_off_lash_roots() {
    let outside = format!(
        "{}\nfunction frames.helper() -> Int\nkernel 1\ncharge 1\nbody {{\n  return 1\n}}\n",
        DATAFRAME.kernel
    );
    assert!(matches!(
        with_kernel(DATAFRAME.surface, &outside).unwrap_err(),
        PackageRefusal::OutsideRoots { definition, .. } if definition.as_str() == "frames.helper"
    ));

    let kernel = kernel();
    let built = dataframe(&kernel).unwrap();
    let held = HeldPackages::install([kernel.clone(), built]).unwrap();
    assert_eq!(
        held.reserved_roots(),
        kernel.roots().iter().collect(),
        "the reserved roots are the kernel package's, and no host package's"
    );
    for path in ["text", "list.tools", "brand"] {
        let empty = Package::build(path, "1", &PackageSources::default(), &[&kernel]).unwrap();
        let refused = HeldPackages::install([kernel.clone(), empty]).unwrap_err();
        let root = path.split('.').next().unwrap();
        assert!(
            matches!(
                &refused,
                PackageRefusal::ReservedRoot { package, root: refused_root, owner }
                    if package.to_string() == path
                        && refused_root == root
                        && owner.to_string() == "kernel"
            ),
            "{path}: {refused:?}"
        );
    }
}

/// PKG-DIGEST: an artifact is keyed by (path, digest); unchanged source
/// over a changed dependency is a new digest and installs beside the old
/// one, while its constructors keep their identities.
#[test]
fn pkg_digest_unchanged_source_over_another_kernel_is_another_artifact() {
    let (first, second) = (kernel(), kernel_with(&[SHOUT]));
    assert_ne!(first.digest(), second.digest());
    let over_first = dataframe(&first).unwrap();
    assert_eq!(
        over_first.digest(),
        dataframe(&first).unwrap().digest(),
        "a build is a function of its sources and its requirements"
    );
    let over_second = dataframe(&second).unwrap();
    assert_eq!(over_first.path(), over_second.path());
    assert_eq!(over_first.release(), over_second.release());
    assert_ne!(over_first.key(), over_second.key());

    // A constructor stamps its values with its own identity, which
    // unchanged source keeps over another kernel.
    let registry = runtime(&[&over_first]);
    let col = functions(&registry, &[&over_first], &["dataframe.col"]);
    let Datum::Record(fields) = finished(run(
        &registry,
        &col,
        "let made = invoke dataframe.col(\"a\")\nreturn made",
    )) else {
        panic!("an expr is a record")
    };
    let constructor = over_first.function("dataframe.make_expr").unwrap();
    assert_eq!(
        fields
            .iter()
            .find(|(name, _)| name == "__lash_built")
            .map(|(_, built)| built),
        Some(&Datum::Bytes(lash_kernel_doc::Bytes::new(
            &constructor.as_bytes()[..]
        )))
    );
    assert_eq!(
        over_second.function("dataframe.make_expr"),
        Some(constructor)
    );

    let sets = [
        PackageSet::new([&first, &over_first]).unwrap(),
        PackageSet::new([&second, &over_second]).unwrap(),
    ];
    let own = sets[0].digest();
    let retained = sets[1].digest();
    let held = HeldPackages::new(
        [first, second, over_first.clone(), over_second.clone()].map(Arc::new),
        sets,
        own,
    )
    .unwrap();
    assert!(held.artifacts().contains_key(&over_first.key()));
    assert!(held.artifacts().contains_key(&over_second.key()));
    let path = over_first.path().clone();
    assert_eq!(
        held.view(&own).unwrap().package(&path).unwrap().digest(),
        over_first.digest()
    );
    assert_eq!(
        held.view(&retained)
            .unwrap()
            .package(&path)
            .unwrap()
            .digest(),
        over_second.digest()
    );
}

/// PKG-SET-CLOSED: a package set is constructed only when every
/// artifact's `requires` names the set's artifact of that line.
#[test]
fn pkg_set_closed_a_set_holds_every_artifact_its_members_require() {
    let (first, second) = (kernel(), kernel_with(&[SHOUT]));
    let built = dataframe(&first).unwrap();
    PackageSet::new([&first, &built]).expect("a closed set");
    assert_eq!(
        PackageSet::new([&second, &built]).unwrap_err(),
        PackageRefusal::NotClosed {
            artifact: Box::new(built.key()),
            requires: Box::new(first.key()),
            held: SetHolds::Other(second.digest()),
        }
    );
    assert_eq!(
        PackageSet::new([&built]).unwrap_err(),
        PackageRefusal::NotClosed {
            artifact: Box::new(built.key()),
            requires: Box::new(first.key()),
            held: SetHolds::Nothing,
        }
    );
    assert!(matches!(
        PackageSet::new([&first, &second]).unwrap_err(),
        PackageRefusal::TwoOfLine { .. }
    ));
}

/// §2.1: a sync member whose definition can wait is refused.
#[test]
fn a_sync_member_whose_definition_can_wait_is_refused() {
    let waits = DATAFRAME.surface.replace(
        "  method limit(self: data_quanta, n: Int) -> data_quanta = dataframe.data_quanta.limit",
        "  method limit(self: data_quanta, n: Int) -> data_quanta = dataframe.data_quanta.limit\n  method rows(self: data_quanta) -> List(Any) = dataframe.data_quanta.rows",
    );
    let source = format!(
        "{}\nfunction dataframe.data_quanta.rows(self: data_quanta) -> List(Any)\nkernel 1\ncharge 1\nbody {{\n  let rows = invoke dataframe.data_quanta.collect_rows(self)\n  return rows\n}}\n",
        DATAFRAME.kernel.replacen(
            "use dataframe.data_quanta.then\n",
            "use dataframe.data_quanta.then\nuse dataframe.data_quanta.collect_rows\n",
            1,
        )
    );
    assert!(matches!(
        with_kernel(&waits, &source).unwrap_err(),
        PackageRefusal::SyncMemberWaits { member, .. } if member.to_string() == "data_quanta.rows"
    ));
}

/// PKG-OPS: a definition performing an operation its package does not
/// export is refused.
#[test]
fn pkg_ops_a_body_performs_only_what_its_package_exports() {
    let built = dataframe(&kernel()).unwrap();
    let collect = built
        .code()
        .definition(
            built
                .code()
                .position("dataframe.data_quanta.collect_rows")
                .unwrap(),
        )
        .unwrap();
    let run = lash_kernel_doc::EffectName::new("dataframe.run").unwrap();
    assert_eq!(
        collect.body().unwrap().effects[&run].contract,
        lash_kernel_doc::ContractId::new("dataframe.run/1"),
        "a header's operation reaches the body that performs it"
    );
    assert!(
        built
            .code()
            .definition(built.code().position("dataframe.col").unwrap())
            .unwrap()
            .body()
            .unwrap()
            .effects
            .is_empty(),
        "and no other"
    );

    for (effect, slots) in [
        ("web.fetch", "(url: Any) -> Any"),
        ("dataframe.read_raw", "(url: Any) -> Any"),
        ("dataframe.count", "() -> Any"),
    ] {
        let args = if slots.starts_with("()") { "" } else { "url" };
        let source = format!(
            "{}\nfunction dataframe.fetch(url: Any) -> Any\nkernel 1\ncharge 1\neffect {effect}{slots}\nbody {{\n  let page = perform {effect}({args}) as Any\n  return page\n}}\n",
            DATAFRAME.kernel
        );
        let refused = with_kernel(DATAFRAME.surface, &source).unwrap_err();
        assert!(
            matches!(
                &refused,
                PackageRefusal::UndeclaredOperation { definition, effect: performed, .. }
                    if definition.as_str() == "dataframe.fetch" && performed.as_str() == effect
            ),
            "{effect}: {refused:?}"
        );
    }
}

/// PKG-STABLE (FIG-6987): a host package built over another release of a
/// host package it requires, which changed only definitions it does not
/// call, is another artifact whose every definition, its constructors
/// among them, keeps its identity.
#[test]
fn pkg_stable_a_host_package_keeps_its_identities_over_an_unrelated_host_release() {
    let kernel = kernel();
    let one =
        "function helper.one(value: Text) -> Text\nkernel 1\ncharge 1\nbody {\n  return value\n}\n";
    let two =
        "function helper.two(value: Text) -> Text\nkernel 1\ncharge 1\nbody {\n  return value\n}\n";
    let helper = |kernel_text: &str| {
        Package::build(
            "helper",
            "1",
            &PackageSources {
                kernel: kernel_text,
                ..PackageSources::default()
            },
            &[&kernel],
        )
        .unwrap()
    };
    let (first, second) = (helper(one), helper(&format!("{one}\n{two}")));
    assert_ne!(first.digest(), second.digest());
    let over = |helper: &Package| {
        Package::build("dataframe", RELEASE, &DATAFRAME, &[&kernel, helper]).unwrap()
    };
    let (over_first, over_second) = (over(&first), over(&second));
    assert_ne!(
        over_first.digest(),
        over_second.digest(),
        "another artifact"
    );
    let identities = |package: &Package| {
        let code = package.code();
        (0..code.len())
            .map(|at| (code.name(at).to_owned(), code.ids()[at]))
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    assert!(identities(&over_first).contains_key("dataframe.make_expr"));
    assert_eq!(identities(&over_first), identities(&over_second));
}
