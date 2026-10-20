//! RELINK-CLOSURE (K-LIB-009) and CURRENT-RELEASE for carried saved
//! functions: relinked by name to the cell's package set, with an earlier
//! release of the same names still held beside it.

use super::*;
use crate::Library as _;
use crate::{Export, ExportView, NamedLibrary, OperationExport, PackagePath, Segment, Visibility};
use lash_kernel_doc::{
    ContractId, FunctionId, FunctionRegistry, NumberPolicy, parse_definition, parse_document,
};

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
/// `dataframe.collect`, and performs `dataframe.run` itself.
fn keep(library: &NamedLibrary) -> SavedFunction {
    let col = library.resolve("dataframe.col").expect("col");
    let collect = library.resolve("dataframe.collect").expect("collect");
    let mut document = parse_document(&format!(
        "kernel 1\nnumbers float\n{RUN} \"dataframe.run/1\"\nuse dataframe.col = @{col}\nuse dataframe.collect = @{collect}\n\
         fn keep(n) {{ let e = invoke dataframe.col(n) let rows = invoke dataframe.collect(e) let more = perform dataframe.run(e) as List(Any) return rows }}\nmain {{}}"
    ))
    .expect("a document");
    document.manifest.functions = super::relink::reachable(&[col, collect], library);
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
    }
}

/// The library functions `function`'s code holds.
fn held(function: &SavedFunction) -> BTreeSet<FunctionId> {
    let code = function.definition().expect("its code");
    CodeReads::of(&code.params, &code.body).library
}

/// RELINK-CLOSURE: a saved function calling `dataframe.col` relinks to the
/// cell's set's `col`, a new identity, while the earlier release is still
/// held under the same names: its code and its listing name that set's
/// identities only (CURRENT-RELEASE), and installing it into a cell of
/// that set holds.
#[test]
fn a_saved_function_relinks_to_the_cell_set_with_the_earlier_one_retained() {
    let r1 = Release::of("1");
    let r2 = Release::of("2");
    let earlier = library(&r2, &r1);
    let saved = keep(&earlier);
    let cell = library(&r1, &r2);
    let relinked =
        relink_saved(&saved, &cell, &Owns::run("plan: Any", "dataframe.run/1")).expect("relinked");
    let col = cell.resolve("dataframe.col").expect("col");
    assert_ne!(earlier.resolve("dataframe.col"), Some(col));
    assert!(held(&saved).contains(&earlier.resolve("dataframe.col").expect("col")));
    assert!(held(&relinked).contains(&col));
    let set: BTreeSet<FunctionId> = cell.iter().map(|(_, function)| function).collect();
    assert!(
        relinked
            .document
            .manifest
            .functions
            .keys()
            .all(|function| set.contains(function)),
        "{:?}",
        relinked.document.manifest.functions
    );
    assert_eq!(relinked.document.manifest.functions.len(), 3);
    assert_ne!(relinked.document.identity(), saved.document.identity());
    // Relinking again to the same set changes nothing.
    let again = relink_saved(&relinked, &cell, &Owns::run("plan: Any", "dataframe.run/1"))
        .expect("relinked again");
    assert_eq!(again.document.identity(), relinked.document.identity());

    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    let functions = BTreeMap::from([(relinked.name.clone(), relinked)]);
    let roots = BTreeSet::from([Name::new("keep")]);
    let namespaces = crate::Namespaces::of_requirements(
        &BTreeMap::from([(
            EffectName::new("dataframe.run").expect("an effect"),
            Owns::run("plan: Any", "dataframe.run/1").requirement(),
        )]),
        &BTreeMap::new(),
    )
    .expect("operations");
    install(
        &mut document,
        &functions,
        &roots,
        &lash_kernel_doc::FunctionRepresentation::Bare,
        &namespaces,
        &cell,
    )
    .expect("a relinked function installs into a cell of its set");
    // An unrelinked one does not: it lists the earlier set's identities.
    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    let functions = BTreeMap::from([(saved.name.clone(), saved)]);
    assert!(matches!(
        install(
            &mut document,
            &functions,
            &roots,
            &lash_kernel_doc::FunctionRepresentation::Bare,
            &namespaces,
            &cell,
        ),
        Err(Unusable::Relink {
            gap: RelinkGap::Unlinked,
            ..
        })
    ));
}

/// RELINK-CLOSURE: a saved function whose callee the cell's set removed,
/// or defines under another signature, or whose own operation the set
/// declares under another signature, is refused, naming it and the
/// callee.
#[test]
fn a_saved_function_whose_callee_the_set_does_not_serve_is_refused_naming_both() {
    let r1 = Release::of("1");
    let saved = keep(&library(&Release::of("0"), &r1));
    let refused = |release: Release, owns: Owns| {
        relink_saved(&saved, &library(&r1, &release), &owns).expect_err("refused")
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
    let relinked = relink_saved(&saved, &library(&r1, &r2), &owns).expect("relinked");
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
