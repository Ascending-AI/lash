//! Laws of namespace composition and resolution: OP-NAME and NS-COMPOSE.

use std::collections::BTreeMap;

use lash_kernel_doc::{EffectName, FunctionName, Signature, Type};

use crate::{
    Export, ExportView, Member, NamespaceRefusal, Namespaces, Offered, Operations, PackagePath,
    QualifiedOperation, Reserving, Resolution, Segment, Spelling, spell_operation,
};

/// Code packages, by path.
struct Packages(BTreeMap<PackagePath, BTreeMap<Segment, Export>>);

impl ExportView for Packages {
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        self.0.get(path)
    }

    fn paths(&self) -> Vec<PackagePath> {
        self.0.keys().cloned().collect()
    }
}

fn operation(text: &str) -> QualifiedOperation {
    QualifiedOperation::from_effect(&EffectName::new(text).expect("an effect name"))
        .expect("an operation")
}

fn signature() -> Signature {
    Signature {
        params: Vec::new(),
        result: Type::Any,
    }
}

fn operations(names: &[&str]) -> Operations {
    names
        .iter()
        .map(|name| (operation(name), Offered::tool(*name, signature())))
        .collect()
}

/// The package `dataframe` exporting the function `col`.
fn dataframe() -> Packages {
    Packages(BTreeMap::from([(
        PackagePath::parse("dataframe").expect("a path"),
        BTreeMap::from([(
            Segment::new("col").expect("a segment"),
            Export::Function(Member {
                definition: FunctionName::new("dataframe.col").expect("a function name"),
                signature: signature(),
                asynchronous: false,
                rest: false,
            }),
        )]),
    )]))
}

/// OP-NAME: an operation has one canonical effect name; each dialect
/// writes it by `spell`, and each spelling resolves, in its own dialect
/// only, to that one operation.
#[test]
fn an_operation_has_one_name_spelled_per_dialect() {
    let send = operation("inbox.send_item");
    assert_eq!(send.effect_name().as_str(), "inbox.send_item");
    let namespaces =
        Namespaces::of_operations(operations(&["inbox.send_item"])).expect("one operation");
    for (spelling, written) in [
        (Spelling::TypeScript, "inbox.sendItem"),
        (Spelling::Python, "inbox.send_item"),
    ] {
        assert_eq!(spell_operation(spelling, &send), written);
        let path = written.split('.').collect::<Vec<_>>();
        assert!(
            matches!(
                namespaces.resolve(&path, spelling),
                Resolution::Operation { operation, .. } if *operation == send
            ),
            "{written}"
        );
    }
    assert!(matches!(
        namespaces.resolve(&["inbox", "send_item"], Spelling::TypeScript),
        Resolution::Unknown
    ));
    let misspelled = namespaces
        .misspelled(&["inbox", "send_item"], Spelling::TypeScript)
        .expect("the canonical spelling names the operation in another dialect");
    assert_eq!(misspelled.repair, "inbox.sendItem");
}

/// NS-COMPOSE: disjoint children merge; a package path equal to another's
/// export, a dialect global, a printer-generated name or `lash` is refused
/// naming both; `tools` is accepted.
#[test]
fn disjoint_packages_merge_and_a_taken_root_is_refused_naming_both() {
    let namespaces = Namespaces::build(
        &dataframe(),
        operations(&[
            "dataframe.run",
            "mail.send",
            "mail.inbox.list",
            "tools.search",
        ]),
    )
    .expect("disjoint children merge");
    for (written, operation) in [
        (&["dataframe", "run"][..], "dataframe.run"),
        (&["mail", "send"][..], "mail.send"),
        (&["mail", "inbox", "list"][..], "mail.inbox.list"),
        (&["tools", "search"][..], "tools.search"),
    ] {
        assert!(
            matches!(
                namespaces.resolve(written, Spelling::Python),
                Resolution::Operation { operation: found, .. } if found.effect_name().as_str() == operation
            ),
            "{operation}"
        );
    }
    assert!(matches!(
        namespaces.resolve(&["dataframe", "col"], Spelling::Python),
        Resolution::Export { .. }
    ));
    assert!(matches!(
        namespaces.resolve(&["mail", "inbox"], Spelling::Python),
        Resolution::Namespace
    ));

    let refused = Namespaces::build(&dataframe(), operations(&["dataframe.col.head"]))
        .expect_err("a package path equal to an export");
    assert_eq!(
        refused,
        NamespaceRefusal::PathIsExport {
            path: "dataframe.col".to_owned(),
            package: "dataframe".to_owned(),
            export: "dataframe.col".to_owned(),
        }
    );
    let refused = Namespaces::of_operations(operations(&["mail.send", "mail.send.now"]))
        .expect_err("a package path equal to an operation");
    assert!(
        matches!(&refused, NamespaceRefusal::PathIsExport { path, export, .. }
            if path == "mail.send" && export == "mail.send"),
        "{refused:?}"
    );

    let reserves = |root: &str| {
        if root == "math" {
            Some("a dialect global".to_owned())
        } else if root.starts_with("helper_") {
            Some("a name the printer generates".to_owned())
        } else {
            None
        }
    };
    let view = Reserving {
        view: &crate::NoPackages,
        reserves: &reserves,
    };
    for (name, by) in [
        ("math.max", "a dialect global"),
        ("helper_sort.run", "a name the printer generates"),
        ("lash.run", "lash's own namespace"),
    ] {
        let refused = Namespaces::build(&view, operations(&[name])).expect_err(name);
        let root = name.split('.').next().unwrap_or_default();
        assert_eq!(
            refused,
            NamespaceRefusal::ReservedRoot {
                path: root.to_owned(),
                root: root.to_owned(),
                by: by.to_owned(),
            },
            "{name}"
        );
    }
    Namespaces::build(&view, operations(&["tools.search"])).expect("`tools` is accepted");
}
