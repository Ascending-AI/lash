//! HELP-LABEL and HELP-COMPARE read the caller's selected package set.

use lash_helper_labels::Comparison;
use lash_kernel_check::WorkflowDocument;
use lash_kernel_doc::WorkflowDocumentEntry;
use lash_kernel_doc::parse_document;
use lash_kernel_package::{HeldPackages, Package, PackageSources};
use lash_vocabulary::legacy_names::FunctionName;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::names::MemberPath;
use lash_vocabulary::names::PackagePath;
use lash_vocabulary::names::Segment;

use crate::{ComparisonView, derive};

fn path(name: &str) -> PackagePath {
    PackagePath::new(Segment::new(name).unwrap())
}

fn member(name: &str) -> MemberPath {
    MemberPath {
        export: Segment::new(name).unwrap(),
        member: None,
    }
}

/// The packages of `held`'s own set, the kernel first.
fn own_packages(held: &HeldPackages) -> Vec<Package> {
    let view = held.view();
    let mut packages: Vec<Package> = view
        .packages()
        .map(|package| held.artifacts()[&package.key()].as_ref().clone())
        .collect();
    packages.sort_by_key(|package| package.path().to_string() != "kernel");
    packages
}

fn comparison_package(name: &str, title: &str, kernel: &Package) -> Package {
    let package = Package::build(
        name,
        "1.0",
        &PackageSources {
            kernel: &format!(
                "use eq\nfunction {name}.compare(left: Any, right: Any) -> Bool\n\
                 kernel 1\ncharge 4\nbody {{ return eq(left, right) }}\n"
            ),
            surface: &format!("function compare(left: Any, right: Any) -> Bool = {name}.compare\n"),
            labels: &serde_json::json!({"members": {"compare": title}}).to_string(),
        },
        &[kernel],
    )
    .unwrap();
    let mut display = package.display().clone();
    display
        .members
        .get_mut(&member("compare"))
        .unwrap()
        .comparison = Some(Comparison::Eq);
    package.with_display(display)
}

/// HELP-LABEL: labels belong to (package, member); the same member name in
/// two packages never overwrites either title, and an unknown member has none.
#[test]
fn labels_are_keyed_by_package_and_member_in_the_selected_set() {
    let built = lash_vm_releases::lash_packages().unwrap();
    let built_view = built.view();
    for (name, title) in [
        ("ts.array.map", "TypeScript array map"),
        ("py.str", "Python str"),
    ] {
        let id = built.functions().named(name).next().unwrap();
        assert_eq!(
            built_view.title(&id, &FunctionName::new(name).unwrap()),
            title
        );
    }
    // FIG-6942: display side facts survive a move independently of whether
    // a build installs the definition that uses them.
    let typescript = built_view.package(&path("typescript")).unwrap();
    for (name, title) in [
        (
            "ts.array.callback_slots2",
            "TypeScript array two operand callback",
        ),
        (
            "ts.array.callback_slots3",
            "TypeScript array three operand callback",
        ),
        ("ts.array.filter_slots", "TypeScript array filter"),
        ("ts.array.map_slots", "TypeScript array map"),
        ("ts.array.sort_slots", "TypeScript array sort"),
    ] {
        assert_eq!(
            typescript
                .display()
                .definitions
                .get(&FunctionName::new(name).unwrap())
                .map(|display| display.title.as_str()),
            Some(title),
            "package display lost {name}"
        );
    }
    let removed = FunctionName::new("future.removed_helper").unwrap();
    assert_eq!(
        built_view.title(
            &lash_vocabulary::legacy_names::FunctionId::from_bytes([0; 32]),
            &removed
        ),
        removed.as_str()
    );
    let mut packages = own_packages(&built);
    let kernel = packages[0].clone();
    packages.extend([
        comparison_package("demo", "Compare items", &kernel),
        comparison_package("other", "Compare values", &kernel),
    ]);
    let held = HeldPackages::install(packages).unwrap();
    let selected = held.view();
    for (package, title) in [("demo", "Compare items"), ("other", "Compare values")] {
        assert_eq!(
            selected
                .display(&path(package), &member("compare"))
                .unwrap()
                .title,
            title
        );
        assert!(
            selected
                .display(&path(package), &member("removed"))
                .is_none()
        );
    }
}

/// HELP-COMPARE: the package declares which labelled functions compare.
/// A view of another set must not borrow a declaration from a global table.
#[test]
fn comparisons_come_from_the_supplied_package_set() {
    let built = lash_vm_releases::lash_packages().unwrap();
    let built_view = built.view();
    let mut packages = own_packages(&built);
    let package = comparison_package("demo", "Compare items", &packages[0]);
    packages.push(package);
    let held = HeldPackages::install(packages).unwrap();
    let selected = held.view();
    let name = FunctionName::new("demo.compare").unwrap();
    let id = held
        .functions()
        .iter()
        .find_map(|(id, function)| (function.definition().name == name).then_some(*id))
        .unwrap();
    let mut document = parse_document(&format!(
        "kernel 1\nnumbers float\nuse demo.compare = @{id}\n\
         entry flow(left: Any, right: Any) -> Any\n\
         fn flow(left, right) {{\n\
           let compared = invoke demo.compare(left, right)\n\
           if compared {{ do yield }}\n\
           return null\n\
         }}\nmain {{ finish null }}\n"
    ))
    .unwrap();
    let mut pending = vec![id];
    while let Some(function) = pending.pop() {
        let definition = held.functions().get(&function).unwrap().definition();
        for (dependency, name) in definition
            .body()
            .into_iter()
            .flat_map(|body| &body.functions)
        {
            if document
                .manifest
                .functions
                .insert(*dependency, name.clone())
                .is_none()
            {
                pending.push(*dependency);
            }
        }
    }
    let checked = WorkflowDocument::derive(
        document,
        WorkflowDocumentEntry::Entry {
            function: Name::new("flow"),
        },
        held.functions().as_ref(),
    )
    .unwrap();
    let view = derive(&checked, &selected).unwrap();
    assert!(view.nodes.iter().any(|node| {
        node.header
            .as_ref()
            .is_some_and(|header| matches!(header.comparison, Some(ComparisonView::Eq { .. })))
    }));
    let other = derive(&checked, &built_view).unwrap();
    assert!(other.nodes.iter().all(|node| {
        node.header
            .as_ref()
            .is_none_or(|header| header.comparison.is_none())
    }));
}
