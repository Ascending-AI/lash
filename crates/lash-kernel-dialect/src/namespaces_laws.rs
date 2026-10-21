//! Laws of namespace composition and resolution: OP-NAME, NS-COMPOSE and
//! PKG-NAMESPACE.

use std::collections::BTreeMap;

use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionName;
use lash_vocabulary::types::Signature;
use lash_vocabulary::types::Type;

use crate::Export;
use crate::ExportView;
use crate::Member;
use crate::NamespaceRefusal;
use crate::Namespaces;
use crate::Offered;
use crate::Operations;
use crate::Reserving;
use crate::Resolution;
use crate::Spelling;
use crate::TypeMember;
use crate::ValueType;
use lash_vocabulary::names::PackagePath;
use lash_vocabulary::names::QualifiedOperation;
use lash_vocabulary::names::Segment;

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
        .map(|name| {
            (
                operation(name),
                Offered::tool(
                    *name,
                    lash_vocabulary::operations::OperationRequirement::new(
                        signature(),
                        lash_vocabulary::operations::ContractId::new("1"),
                    ),
                ),
            )
        })
        .collect()
}

/// The package `dataframe` exporting the function `col`.
fn dataframe() -> Packages {
    Packages(BTreeMap::from([(
        PackagePath::parse("dataframe").expect("a path"),
        BTreeMap::from([
            (
                Segment::new("run").unwrap(),
                Export::Operation(crate::OperationExport {
                    requirement: lash_vocabulary::operations::OperationRequirement::new(
                        signature(),
                        lash_vocabulary::operations::ContractId::new("1"),
                    ),
                    intents: Default::default(),
                    visibility: crate::Visibility::Public,
                }),
            ),
            (
                Segment::new("col").expect("a segment"),
                Export::Function(Member {
                    definition: FunctionName::new("dataframe.col").expect("a function name"),
                    signature: signature(),
                    asynchronous: false,
                    rest: false,
                }),
            ),
        ]),
    )]))
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

/// NS-COMPOSE (FIG-6988): one path, one owner. An offered operation at a
/// segment a code package exports as anything but an operation is refused
/// naming both, so a catalog tool never shadows package code. An operation
/// at the package's own operation export composes under the package's
/// declared requirement, and under another contract is refused.
#[test]
fn an_operation_at_a_code_export_is_refused_naming_both() {
    let mut packages = dataframe();
    let exports = packages
        .0
        .get_mut(&PackagePath::parse("dataframe").expect("a path"))
        .expect("the package");
    exports.insert(
        Segment::new("execution_error").expect("a segment"),
        Export::Error,
    );
    exports.insert(
        Segment::new("run").expect("a segment"),
        Export::Operation(crate::OperationExport {
            requirement: lash_vocabulary::operations::OperationRequirement::new(
                signature(),
                lash_vocabulary::operations::ContractId::new("1"),
            ),
            intents: Default::default(),
            visibility: crate::Visibility::Public,
        }),
    );
    for export in ["dataframe.col", "dataframe.execution_error"] {
        let refused = Namespaces::build(&packages, operations(&["dataframe.run", export]))
            .expect_err("an operation at a code export");
        assert_eq!(
            refused,
            NamespaceRefusal::OperationIsExport {
                operation: export.to_owned(),
                package: "dataframe".to_owned(),
                export: export.to_owned(),
            },
            "{export}"
        );
        assert!(refused.to_string().contains(export), "{refused}");
    }
    // A runner's tool states its own signature, `(input: Json)`; the
    // operation is offered under the package's declared requirement, the
    // one the package's bodies state.
    let run = operation("dataframe.run");
    let offered_under = |contract: &str| {
        Operations::from([(
            run.clone(),
            Offered::tool(
                "dataframe.run",
                lash_vocabulary::operations::OperationRequirement::new(
                    Signature {
                        params: vec![lash_vocabulary::types::Param {
                            name: lash_vocabulary::legacy_names::Name::new("input"),
                            ty: Type::Json,
                            optional: false,
                        }],
                        result: Type::Any,
                    },
                    lash_vocabulary::operations::ContractId::new(contract),
                ),
            ),
        )])
    };
    let namespaces = Namespaces::build(&packages, offered_under("1"))
        .expect("the package's own operation composes");
    assert!(matches!(
        namespaces.resolve(&["dataframe", "run"], Spelling::Python),
        Resolution::Operation { operation, offered }
            if *operation == run
                && offered.requirement
                    == lash_vocabulary::operations::OperationRequirement::new(
                        signature(),
                        lash_vocabulary::operations::ContractId::new("1"),
                    )
    ));
    assert_eq!(
        Namespaces::build(&packages, offered_under("2")).expect_err("another contract"),
        NamespaceRefusal::OperationContractDiffers {
            operation: "dataframe.run".to_owned(),
            package: "dataframe".to_owned(),
            offered: "2".to_owned(),
            declared: "1".to_owned(),
        }
    );
    assert!(matches!(
        namespaces.resolve(&["dataframe", "col"], Spelling::Python),
        Resolution::Export { .. }
    ));
}

/// PKG-NAMESPACE (FIG-6920): only the package path is spelled as path
/// segments. A value type after it resolves by its type-name spelling, and
/// only by it, and a static under it by its member spelling, in each
/// dialect.
#[test]
fn a_value_type_and_its_static_resolve_by_their_own_spellings() {
    let read_text = Member {
        definition: FunctionName::new("dataframe.data_quanta.read_text").expect("a function name"),
        signature: signature(),
        asynchronous: false,
        rest: false,
    };
    let view = Packages(BTreeMap::from([(
        PackagePath::parse("dataframe").expect("a path"),
        BTreeMap::from([(
            Segment::new("data_quanta").expect("a segment"),
            Export::Type(ValueType {
                representation: Type::Any,
                members: BTreeMap::from([(
                    Segment::new("read_text").expect("a segment"),
                    TypeMember::Static(read_text.clone()),
                )]),
            }),
        )]),
    )]));
    let namespaces = Namespaces::build(&view, Operations::new()).expect("one package");
    for (spelling, ty, member) in [
        (Spelling::TypeScript, "DataQuanta", "readText"),
        (Spelling::Python, "DataQuanta", "read_text"),
    ] {
        assert!(
            matches!(
                namespaces.resolve(&["dataframe", ty], spelling),
                Resolution::Export { segment, member: None, .. } if segment.as_str() == "data_quanta"
            ),
            "{ty}"
        );
        assert!(
            matches!(
                namespaces.resolve(&["dataframe", ty, member], spelling),
                Resolution::Export { member: Some((_, TypeMember::Static(found))), .. } if *found == read_text
            ),
            "{ty}.{member}"
        );
    }
    // A type is written as a type name, and a package path as segments.
    for written in [
        &["dataframe", "dataQuanta"][..],
        &["Dataframe", "DataQuanta"],
    ] {
        assert!(
            matches!(
                namespaces.resolve(written, Spelling::TypeScript),
                Resolution::Unknown
            ),
            "{written:?}"
        );
    }
}

/// CLOSED-NAMESPACE: offers cannot invent package members, including under
/// an installed nested namespace, while external operations remain valid.
#[test]
fn a_package_surface_refuses_undeclared_operation_offers() {
    for (path, expected) in [
        ("dataframe.run", crate::PackagePathClass::Public),
        ("dataframe.col", crate::PackagePathClass::Public),
        ("dataframe.nosuch", crate::PackagePathClass::Unknown),
        ("dataframe.sub.nosuch", crate::PackagePathClass::Unknown),
        ("dataframe_other.run", crate::PackagePathClass::Outside),
    ] {
        let segments = path
            .split('.')
            .map(Segment::new)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            crate::classify_package_path(&dataframe(), &segments),
            expected,
            "{path}"
        );
    }
    for path in ["dataframe.nosuch", "dataframe.nested.nosuch"] {
        assert!(
            matches!(Namespaces::build(&dataframe(), operations(&[path])),
            Err(NamespaceRefusal::UndeclaredOperation { operation, .. }) if operation == path)
        );
    }
    let namespaces = Namespaces::build(
        &dataframe(),
        operations(&["dataframe.run", "outside.lookup"]),
    )
    .unwrap();
    assert!(matches!(
        namespaces.resolve(&["dataframe", "run"], Spelling::Python),
        Resolution::Operation { .. }
    ));
    assert!(matches!(
        namespaces.resolve(&["outside", "lookup"], Spelling::Python),
        Resolution::Operation { .. }
    ));
}
