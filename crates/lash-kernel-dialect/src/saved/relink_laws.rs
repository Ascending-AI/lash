//! RELINK-CLOSURE (K-LIB-009) for carried saved functions: relinked by
//! name to the cell's package set.

use super::*;
use crate::Export;
use crate::ExportView;
use crate::Library as _;
use crate::NamedLibrary;
use crate::OperationExport;
use crate::Visibility;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::parse_definition;
use lash_kernel_doc::parse_document;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::names::PackagePath;
use lash_vocabulary::names::Segment;
use lash_vocabulary::operations::ContractId;

const RUN: &str = "effect dataframe.run(plan: Any) -> List(Any) contract";

/// One release of `dataframe`: `make` returning `value`, and `col` calling
/// it, under `col`'s parameters `params`, and `collect`, whose body
/// performs `dataframe.run` under `contract`. `None` leaves `col` out.
struct Release {
    value: &'static str,
    col: Option<&'static str>,
    contract: &'static str,
}

impl Release {
    const fn of(value: &'static str) -> Self {
        Self {
            value,
            col: Some("name: Text"),
            contract: "dataframe.run/1",
        }
    }

    /// Registers the release in `registry`: its functions' identities.
    fn register(&self, registry: &mut FunctionRegistry) -> BTreeSet<FunctionId> {
        // A definition two releases share is one function.
        let mut define = |text: String| {
            let definition = parse_definition(&text).expect("a definition");
            let function = definition.identity();
            if registry.get(&function).is_none() {
                registry.register(definition, None).expect("registered");
            }
            function
        };
        let make = define(format!(
            "function dataframe.make() -> Any\nkernel 1\ncharge 1\nbody {{ return {} }}",
            self.value
        ));
        let collect = define(format!(
            "function dataframe.collect(plan: Any) -> List(Any)\nkernel 1\ncharge 1\n{RUN} \"{}\"\nbody {{ let rows = perform dataframe.run(plan) as List(Any) return rows }}",
            self.contract
        ));
        let mut ids = BTreeSet::from([make, collect]);
        if let Some(params) = self.col {
            ids.insert(define(format!(
                "function dataframe.col({params}) -> Any\nkernel 1\ncharge 1\nuse dataframe.make = @{make}\nbody {{ let made = invoke dataframe.make() return made }}"
            )));
        }
        ids
    }
}

/// The package set of `release`, with `retained` held beside it: its names
/// over every function either defines.
fn library(retained: &Release, release: &Release) -> NamedLibrary {
    let mut registry = FunctionRegistry::new();
    retained.register(&mut registry);
    let set = release.register(&mut registry);
    NamedLibrary::resolving(&registry, |function| set.contains(function)).expect("one set")
}

/// The package set of `release` alone, as a node that no longer holds the
/// release a function was saved against holds it.
fn alone(release: &Release) -> NamedLibrary {
    let mut registry = FunctionRegistry::new();
    let set = release.register(&mut registry);
    NamedLibrary::resolving(&registry, |function| set.contains(function)).expect("one set")
}

/// The `dataframe` package's exports in a set: its `run` operation under
/// `contract`, with `params` as its parameters.
struct Owns(BTreeMap<Segment, Export>);

impl Owns {
    fn run(params: &str, contract: &str) -> Self {
        let signature = parse_definition(&format!(
            "function dataframe.run({params}) -> List(Any)\nkernel 1\ncharge 1\nbody {{ let rows = [] return rows }}"
        ))
        .expect("a signature")
        .signature;
        Self(BTreeMap::from([(
            Segment::new("run").expect("a segment"),
            Export::Operation(OperationExport {
                requirement: OperationRequirement {
                    signature,
                    contract: ContractId::new(contract),
                },
                intents: BTreeSet::new(),
                visibility: Visibility::Public,
            }),
        )]))
    }
}

impl ExportView for Owns {
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        (path.to_string() == "dataframe").then_some(&self.0)
    }

    fn paths(&self) -> Vec<PackagePath> {
        vec![PackagePath::parse("dataframe").expect("a path")]
    }
}

/// `keep`, saved under `library`'s set: it calls `dataframe.col` and
/// `dataframe.collect`, held against that set's signatures of them, and
/// performs `dataframe.run` itself.
fn keep(library: &NamedLibrary) -> SavedFunction {
    let col = library.resolve("dataframe.col").expect("col");
    let collect = library.resolve("dataframe.collect").expect("collect");
    let mut document = parse_document(&format!(
        "kernel 1\nnumbers float\n{RUN} \"dataframe.run/1\"\nuse dataframe.col = @{col}\nuse dataframe.collect = @{collect}\n\
         fn keep(n) {{ let e = invoke dataframe.col(n) let rows = invoke dataframe.collect(e) let more = perform dataframe.run(e) as List(Any) return rows }}\nmain {{}}"
    ))
    .expect("a document");
    document.manifest.functions = super::relink::reachable(&[col, collect], library);
    let catalog: &dyn lash_kernel_doc::FunctionCatalog = library;
    let against = [col, collect]
        .into_iter()
        .map(|function| {
            let header = catalog.header(&function).expect("a held function");
            (function, header.signature.clone())
        })
        .collect();
    SavedFunction {
        name: Name::new("keep"),
        document,
        captures: BTreeMap::new(),
        closure: PageRef {
            id: PageId([1; 32]),
            grant: PageGrant::NONE,
        },
        helper_owners: BTreeSet::new(),
        references: BTreeSet::new(),
        written: None,
        token: None,
        held: None,
        against,
    }
}

/// RELINK-CLOSURE: a saved function whose callee the cell's set removed,
/// or defines under another signature, or whose own operation the set
/// declares under another signature, is refused, naming it and the
/// callee. The node holds the cell's set alone, as a production node does:
/// the callee's old signature is the one the function was saved against,
/// and a callee whose old signature is not known is refused too. One saved
/// under a generated name is named by the path a binding held it at.
#[test]
fn a_saved_function_whose_callee_the_set_does_not_serve_is_refused_naming_both() {
    let r1 = Release::of("1");
    let saved = keep(&library(&Release::of("0"), &r1));
    let refused = |release: Release, owns: Owns| {
        relink_saved(&saved, &alone(&release), &owns).expect_err("refused")
    };
    let run = || Owns::run("plan: Any", "dataframe.run/1");
    for (release, owns, callee, gap) in [
        (
            Release {
                col: None,
                ..Release::of("2")
            },
            run(),
            "dataframe.col",
            RelinkGap::Removed,
        ),
        (
            Release {
                col: Some("name: Text, alias: Text"),
                ..Release::of("2")
            },
            run(),
            "dataframe.col",
            RelinkGap::Signature,
        ),
        (
            Release::of("2"),
            Owns::run("plan: Any, limit: Int", "dataframe.run/1"),
            "dataframe.run",
            RelinkGap::Signature,
        ),
    ] {
        let error = refused(release, owns);
        assert_eq!(
            error,
            Unusable::Relink {
                function: Name::new("keep"),
                callee: callee.to_owned(),
                gap,
            }
        );
        let message = error.to_string();
        assert!(
            message.contains("`keep`") && message.contains(callee),
            "{message}"
        );
    }
    let unknown = SavedFunction {
        against: BTreeMap::new(),
        ..saved.clone()
    };
    assert_eq!(
        relink_saved(&unknown, &alone(&Release::of("2")), &run())
            .expect_err("an old signature no one holds serves no call"),
        Unusable::Relink {
            function: Name::new("keep"),
            callee: "dataframe.col".to_owned(),
            gap: RelinkGap::Signature,
        }
    );
    // One saved under a generated name is named by where a binding held it.
    let held = SavedFunction {
        held: Some(HeldAt {
            binding: Name::new("plan"),
            path: "plan.map".to_owned(),
        }),
        ..saved.clone()
    };
    let removed = Release {
        col: None,
        ..Release::of("2")
    };
    assert!(
        relink_saved(&held, &alone(&removed), &run())
            .expect_err("refused")
            .to_string()
            .contains("`plan.map`")
    );
}

/// A changed contract under an unchanged signature is taken: the relinked
/// function performs `dataframe.run`, itself and through the set's body,
/// under the set's requirement.
#[test]
fn a_saved_function_whose_operation_changed_contract_alone_runs_under_the_new_requirement() {
    let r1 = Release::of("1");
    let saved = keep(&library(&Release::of("0"), &r1));
    let r2 = Release {
        contract: "dataframe.run/2",
        ..Release::of("2")
    };
    let owns = Owns::run("plan: Any", "dataframe.run/2");
    let relinked = relink_saved(&saved, &alone(&r2), &owns).expect("relinked");
    let run = EffectName::new("dataframe.run").expect("an effect");
    assert_eq!(
        saved.document.manifest.effects[&run].contract,
        ContractId::new("dataframe.run/1")
    );
    assert_eq!(
        relinked.document.manifest.effects,
        BTreeMap::from([(run, owns.requirement())])
    );
}

impl Owns {
    fn requirement(&self) -> OperationRequirement {
        match self.0.values().next() {
            Some(Export::Operation(export)) => export.requirement.clone(),
            _ => unreachable!("the fixture exports `run`"),
        }
    }
}
