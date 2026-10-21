//! The store-to-runtime loader (FIG-6985, K-MACH-008): a node assembles
//! the library a pinned run executes from the definitions its document
//! was written against, its own where it holds an identity and the
//! store's otherwise, and refuses one run, naming the function, when that
//! code is incomplete, corrupt or needs a native the node lacks.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::FunctionCatalog as _;
use lash_kernel_doc::graph_tokens::encode_body;
use lash_kernel_doc::{
    Document, FunctionDefinition, FunctionId, FunctionName, FunctionRegistry, KernelDataLimits,
    NumberPolicy,
};
use lash_kernel_package::{HeldPackages, Package, PackageSources};

use super::{Definitions, LoadRefusal, assemble, declare, requirements};

fn q(value: i64) -> Package {
    let kernel =
        format!("function q.v() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n");
    Package::build(
        "q",
        &format!("{value}.0.0"),
        &PackageSources {
            kernel: &kernel,
            surface: "function v() -> Int = q.v\n",
            labels: r#"{"members": {"v": "value"}}"#,
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
            labels: r#"{"members": {"f": "f"}}"#,
        },
        &[q],
    )
    .expect("p builds")
}

fn held(lines: &[&[&Package]]) -> HeldPackages {
    let [line] = lines else {
        panic!("a node holds one set")
    };
    HeldPackages::install(line.iter().map(|package| (*package).clone())).expect("the held line")
}

/// A document listing `functions`, with `registry`'s names for them.
fn listing(registry: &FunctionRegistry, functions: &[FunctionId]) -> Document {
    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    for function in functions {
        let name = registry.name_of(function).expect("a held function");
        document
            .manifest
            .functions
            .insert(*function, FunctionName::new(name).expect("a function name"));
    }
    document
}

/// `functions` as `registry` holds them, written to and read back from the
/// store's bytes.
fn stored(
    registry: &FunctionRegistry,
    functions: &[FunctionId],
) -> BTreeMap<FunctionId, Arc<FunctionDefinition>> {
    let definitions = Definitions::default();
    functions
        .iter()
        .map(|function| {
            let bytes = encode_body(
                registry
                    .get(function)
                    .expect("a held function")
                    .definition()
                    .as_ref(),
            );
            let definition = definitions
                .decode(*function, &bytes, KernelDataLimits::standard())
                .expect("the stored definition admits");
            (*function, definition)
        })
        .collect()
}

/// A node holding only R2 runs R1's pinned `p.f` from R1's stored
/// definitions, under the very closure a node holding R1 pins; one that
/// holds everything runs its own registry unchanged; without the stored
/// definitions the run is refused, naming what is missing.
#[test]
fn a_node_runs_definitions_it_lacks_from_the_store_under_the_closure_the_writer_pinned() {
    let (q1, q2) = (q(1), q(2));
    let (p1, p2) = (p(&q1), p(&q2));
    let r1 = [&p1, &q1];
    let r2 = [&p2, &q2];
    let f1 = p1.function("p.f").expect("R1's p.f");
    let v1 = q1.function("q.v").expect("R1's q.v");

    let writer = held(&[&r1]);
    let document = listing(writer.functions(), &[f1, v1]);
    let written = assemble(&document, writer.functions(), &BTreeMap::new())
        .expect("the writer holds its own code");
    assert!(
        written.held,
        "a node holding every function runs its own registry"
    );
    assert!(Arc::ptr_eq(&written.registry, writer.functions()));

    let reader = held(&[&r2]);
    let code = stored(writer.functions(), &[f1, v1]);
    let read = assemble(&document, reader.functions(), &code).expect("R2 runs R1's stored code");
    assert!(!read.held);
    assert_eq!(
        read.closure.reference(),
        written.closure.reference(),
        "the reader pins the closure the writer pinned"
    );
    assert_eq!(read.closure.stored().len(), 2);
    assert!(read.registry.get(&f1).is_some() && read.registry.get(&v1).is_some());
    assert!(
        read.registry
            .get(&p2.function("p.f").expect("R2's p.f"))
            .is_none(),
        "the run's library is its closure alone"
    );

    assert_eq!(
        assemble(&document, reader.functions(), &BTreeMap::new()).map(|_| ()),
        Err(LoadRefusal::Missing { function: f1 }),
    );
}

/// A corrupt or incomplete stored closure is refused at admission, naming
/// the function: bytes of another definition, bytes that do not decode, a
/// callee the store does not keep, and a definition the document does not
/// reach.
#[test]
fn a_corrupt_or_incomplete_stored_closure_is_refused_at_admission() {
    let (q1, q2) = (q(1), q(2));
    let p1 = p(&q1);
    let f1 = p1.function("p.f").expect("R1's p.f");
    let v1 = q1.function("q.v").expect("R1's q.v");
    let v2 = q2.function("q.v").expect("R2's q.v");
    let writer = held(&[&[&p1, &q1]]);
    let reader = held(&[&[&q2]]);
    let document = listing(writer.functions(), &[f1, v1]);
    let definitions = Definitions::default();
    let image = |function: &FunctionId| {
        encode_body(
            writer
                .functions()
                .get(function)
                .expect("a held function")
                .definition()
                .as_ref(),
        )
    };

    assert_eq!(
        definitions
            .decode(f1, &image(&v1), KernelDataLimits::standard())
            .map(|_| ()),
        Err(LoadRefusal::Identity {
            function: f1,
            found: v1
        }),
    );
    assert!(matches!(
        definitions.decode(f1, &[0xff, 0x00, 0x13], KernelDataLimits::standard()),
        Err(LoadRefusal::Undecodable { function, .. }) if function == f1
    ));
    assert_eq!(
        assemble(
            &document,
            reader.functions(),
            &stored(writer.functions(), &[f1])
        )
        .map(|_| ()),
        Err(LoadRefusal::Missing { function: v1 }),
        "p.f's callee is neither held nor stored"
    );
    let mut extra = stored(writer.functions(), &[f1, v1]);
    extra.extend(stored(reader.functions(), &[v2]));
    assert_eq!(
        assemble(&document, reader.functions(), &extra).map(|_| ()),
        Err(LoadRefusal::Unlisted { function: v2 }),
    );
}

/// Declaration mode reads the header of code a node can no longer run: a
/// native-only function whose native this node lacks is declared, with
/// the signature it was written under, where running it is refused.
#[test]
fn declaration_mode_reads_a_header_whose_native_the_node_lacks() {
    let leaf = Package::build(
        "n",
        "1.0.0",
        &PackageSources {
            kernel: "function n.leaf(x: Text) -> Int\nkernel 1\ncharge 1\nnative\n",
            surface: "function leaf(x: Text) -> Int = n.leaf\n",
            labels: r#"{"members": {"leaf": "leaf"}}"#,
        },
        &[],
    )
    .expect("n builds");
    let function = leaf.function("n.leaf").expect("n.leaf");
    let writer = held(&[&[&leaf]]);
    let node = held(&[&[&q(1)]]);
    let document = listing(writer.functions(), &[function]);
    let code = stored(writer.functions(), &[function]);
    let declared = declare([function], 1, node.functions(), &code)
        .expect("a header is read without its native");
    assert_eq!(
        declared
            .definition(&function)
            .map(|definition| &definition.signature),
        Some(
            &writer
                .functions()
                .get(&function)
                .expect("held")
                .definition()
                .signature
        ),
    );
    assert_eq!(
        assemble(&document, node.functions(), &code).map(|_| ()),
        Err(LoadRefusal::NativeMissing { function }),
        "the same code does not run here"
    );
}

/// Code pinned to a kernel version this build does not interpret, or to
/// one other than its document's, is refused at admission, naming the
/// function, in either mode: it is never relinked to run.
#[test]
fn a_definition_of_an_unsupported_kernel_is_refused_at_admission() {
    let q1 = q(1);
    let writer = held(&[&[&q1]]);
    let v1 = q1.function("q.v").expect("q.v");
    let mut future = writer
        .functions()
        .get(&v1)
        .expect("held")
        .definition()
        .as_ref()
        .clone();
    future.kernel = 99;
    let function = future.identity();
    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    document
        .manifest
        .functions
        .insert(function, FunctionName::new("q.v").expect("a name"));
    let code = BTreeMap::from([(function, Arc::new(future))]);
    let node = held(&[&[&q(2)]]);
    let refusal = LoadRefusal::KernelVersion {
        function,
        kernel: 99,
        document: 1,
    };
    assert_eq!(
        assemble(&document, node.functions(), &code).map(|_| ()),
        Err(refusal.clone())
    );
    assert_eq!(
        declare([function], 1, node.functions(), &code).map(|_| ()),
        Err(refusal)
    );
}

/// A run's requirements record is a projection of its closure: it
/// requires the native of each native-only function its code reaches, not
/// of one the node merely holds beside it, and the kernel version its code
/// is written in.
#[test]
fn a_run_requires_the_natives_its_code_reaches_and_its_kernel() {
    let leaf = Package::build(
        "n",
        "1.0.0",
        &PackageSources {
            kernel: "function n.leaf() -> Int\nkernel 1\ncharge 1\nnative\n",
            surface: "function leaf() -> Int = n.leaf\n",
            labels: r#"{"members": {"leaf": "leaf"}}"#,
        },
        &[],
    )
    .expect("n builds");
    let q1 = q(1);
    let node = held(&[&[&leaf, &q1]]);
    let native = leaf.function("n.leaf").expect("n.leaf");
    let v1 = q1.function("q.v").expect("q.v");
    let record = |functions: &[FunctionId]| {
        let document = listing(node.functions(), functions);
        let closure = super::super::closure::derive(
            document.manifest.functions.keys().copied(),
            node.functions(),
            &BTreeMap::new(),
        )
        .expect("the node holds the code");
        requirements(&closure, document.manifest.kernel, std::iter::empty())
    };
    let reaches = record(&[native, v1]);
    assert_eq!(reaches.natives, [native].into());
    assert_eq!(
        reaches.kernels,
        [listing(node.functions(), &[v1]).manifest.kernel].into()
    );
    assert!(
        record(&[v1]).natives.is_empty(),
        "a native the node holds and the code does not reach is no requirement"
    );
}
