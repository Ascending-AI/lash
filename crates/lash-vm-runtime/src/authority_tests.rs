//! R5 (FIG-6985): stored code performs a package's internal operation only
//! when a stored snapshot lists it as that package's own.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::parse_definition;
use lash_vm_broker::kernel::loader::{LoadRefusal, decode};
use lash_vocabulary::data_limits::KernelDataLimits;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::tokens::encode_body;

use super::{AuthorityUnread, InternalAuthority, admit_internal};

/// What the store's snapshots say: the operations packages declare
/// internal, each definition's packages, and each stored set's surface.
#[derive(Default)]
struct Snapshots {
    internal: BTreeMap<EffectName, BTreeSet<String>>,
    members: BTreeMap<FunctionId, BTreeSet<String>>,
    surfaces: BTreeMap<lash_kernel_doc::PackageSetDigest, Vec<u8>>,
}

#[async_trait::async_trait]
impl InternalAuthority for Snapshots {
    async fn internal_operations(
        &self,
        operations: &[EffectName],
    ) -> Result<BTreeMap<EffectName, BTreeSet<String>>, AuthorityUnread> {
        Ok(operations
            .iter()
            .filter_map(|operation| {
                Some((operation.clone(), self.internal.get(operation)?.clone()))
            })
            .collect())
    }

    async fn package_paths(
        &self,
        functions: &[FunctionId],
    ) -> Result<BTreeMap<FunctionId, BTreeSet<String>>, AuthorityUnread> {
        Ok(functions
            .iter()
            .filter_map(|function| Some((*function, self.members.get(function)?.clone())))
            .collect())
    }

    async fn package_surface(
        &self,
        packages: &lash_kernel_doc::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, AuthorityUnread> {
        Ok(self.surfaces.get(packages).cloned())
    }
}

/// `name`, performing `dataframe.run` itself, read back from the bytes the
/// store keeps for it.
fn performer(name: &str) -> FunctionDefinition {
    let written = parse_definition(&format!(
        "function {name}(plan: Any) -> Any\nkernel 1\ncharge 1\n\
         effect dataframe.run(plan: Any) -> Any contract \"dataframe.run/1\"\n\
         body {{\n  let rows = perform dataframe.run(plan) as Any\n  return rows\n}}\n"
    ))
    .expect("a definition");
    decode(
        written.identity(),
        &encode_body(&written),
        KernelDataLimits::standard(),
    )
    .expect("the stored bytes admit")
}

fn stored(definitions: &[&FunctionDefinition]) -> BTreeMap<FunctionId, Arc<FunctionDefinition>> {
    definitions
        .iter()
        .map(|definition| (definition.identity(), Arc::new((*definition).clone())))
        .collect()
}

/// A package's helper that performs its own internal operation runs from
/// the store on a node that never built the package; a definition that
/// merely carries a package-looking name, which no snapshot lists, is
/// refused at admission naming it and the operation, and so is a user
/// function that calls the genuine helper and performs the operation
/// itself: a caller's authority is its own.
#[tokio::test]
async fn only_a_packages_own_stored_code_performs_its_internal_operation() {
    let run = EffectName::new("dataframe.run").expect("an operation");
    let helper = performer("dataframe.helper");
    let forged = performer("dataframe.helper2");
    let user = parse_definition(&format!(
        "function user.go(plan: Any) -> Any\nkernel 1\ncharge 1\n\
         use dataframe.helper = @{}\n\
         effect dataframe.run(plan: Any) -> Any contract \"dataframe.run/1\"\n\
         body {{\n  let a = invoke dataframe.helper(plan)\n  let b = perform dataframe.run(plan) as Any\n  return b\n}}\n",
        helper.identity()
    ))
    .expect("a definition");
    let authority = Snapshots {
        internal: BTreeMap::from([(run.clone(), BTreeSet::from(["dataframe".to_owned()]))]),
        members: BTreeMap::from([(helper.identity(), BTreeSet::from(["dataframe".to_owned()]))]),
        ..Snapshots::default()
    };

    assert_eq!(
        admit_internal(&authority, &stored(&[&helper])).await,
        Ok(Ok(())),
        "the package's own helper performs it"
    );
    assert_eq!(
        admit_internal(&authority, &stored(&[&helper, &forged])).await,
        Ok(Err(LoadRefusal::InternalOperation {
            function: forged.identity(),
            operation: run.clone(),
        })),
    );
    assert_eq!(
        admit_internal(&authority, &stored(&[&helper, &user])).await,
        Ok(Err(LoadRefusal::InternalOperation {
            function: user.identity(),
            operation: run,
        })),
    );
    assert_eq!(
        admit_internal(&Snapshots::default(), &stored(&[&forged])).await,
        Ok(Ok(())),
        "an operation no package declares internal is anyone's to perform"
    );
}

/// A store of no module artifacts: the law below reads none.
struct NoArtifacts;

#[async_trait::async_trait]
impl lash_execution_contracts::module_artifacts::ModuleArtifactStore for NoArtifacts {
    async fn publish_module_artifact(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &str,
        _: &[u8],
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }
    async fn acquire_module_artifact(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &str,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }
    async fn hold_module_artifacts(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &[lash_execution_contracts::module_artifacts::ModuleArtifactHold<'_>],
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }
    async fn end_module_referrer(
        &self,
        _: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
        Ok(())
    }
    async fn get_module_artifact(
        &self,
        _: &str,
    ) -> Result<Option<Vec<u8>>, lash_execution_contracts::module_artifacts::ArtifactStoreError>
    {
        Ok(None)
    }
}

/// Two sets of lash's packages: the writer's, which adds `dataframe`, and
/// the lash-only set of a node that does not hold it.
struct TwoSets {
    lash: Vec<lash_kernel_package::Package>,
    writer: Arc<lash_kernel_package::HeldPackages>,
    node: Arc<lash_kernel_package::HeldPackages>,
}

fn two_sets() -> TwoSets {
    let lash = lash_vm_releases::built().expect("lash's packages");
    let kernel = lash
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package");
    let dataframe = lash_kernel_package::testing::dataframe(kernel).expect("the fixture builds");
    let writer = Arc::new(
        lash_kernel_package::HeldPackages::install(lash.iter().cloned().chain([dataframe]))
            .expect("the writer's packages"),
    );
    let node = Arc::new(
        lash_kernel_package::HeldPackages::install(lash.iter().cloned())
            .expect("the node's packages"),
    );
    assert_ne!(node.digest(), writer.digest(), "the node does not hold it");
    TwoSets { lash, writer, node }
}

/// A kernel engine whose own set is `packages`, over a store that keeps
/// `stored`'s surface.
fn engine_holding(
    packages: &Arc<lash_kernel_package::HeldPackages>,
    stored: &lash_kernel_package::HeldPackages,
) -> crate::KernelProcessEngine {
    let config =
        lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
    crate::process_plugin::KernelProcessPluginFactory::new(
        lash_vm_client::service::Service::new(config.clone()),
        lash_vm_releases::TYPESCRIPT,
        config.run_bounds,
    )
    .with_packages(Arc::clone(packages))
    .engine(Arc::new(NoArtifacts))
    .expect("the engine")
    .with_internal_authority(Arc::new(Snapshots {
        surfaces: BTreeMap::from([(stored.digest(), stored.surface().encode())]),
        ..Snapshots::default()
    }))
}

/// `dataframe.run`, which the writer's set declares, with that declaration.
fn run_declared(
    writer: &lash_kernel_package::HeldPackages,
) -> (
    EffectName,
    lash_execution_contracts::process::OwnedOperation,
) {
    let run = EffectName::new("dataframe.run").expect("an operation");
    let declared = crate::owned_operation(&writer.view(), &run).expect("the fixture owns `run`");
    (run, declared)
}

/// PKG-RUNNER (FIG-6985): a start's runners are judged by the declarations
/// of the set it was linked under, read from the store where the node does
/// not hold it, not by the node's own set. A node holding lash's packages
/// alone starts a definition linked under a set that adds `dataframe`:
/// `dataframe` owns `run` there, under its declared contract. One naming a
/// set the store keeps no surface of is refused.
#[tokio::test]
async fn a_start_is_judged_by_the_declarations_of_the_set_it_was_linked_under() {
    let sets = two_sets();
    let linked = sets.writer.digest();
    let engine = engine_holding(&sets.node, &sets.writer);
    let (run, declared) = run_declared(&sets.writer);
    let document =
        lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { }").expect("a document");
    let start = |packages| {
        serde_json::to_value(crate::KernelProcessInput {
            declared_packages: sets.writer.surface().selection(),
            document: document.identity(),
            functions: BTreeSet::new(),
            entry: lash_vocabulary::legacy_names::Name::new("work"),
            packages,
            args: serde_json::Map::new(),
        })
        .expect("a start")
    };
    let performed = BTreeMap::from([(run.clone(), declared.requirement.clone())]);

    let owned = engine
        .owned_operations(&start(linked), &performed)
        .await
        .expect("the linked set's declarations read");
    assert_eq!(owned.get(&run), Some(&declared));
    assert!(
        engine
            .owned_operations(&start(unstored_digest(&sets.lash)), &performed)
            .await
            .is_err(),
        "a set the store keeps no surface of judges no runner"
    );
}

/// A function value whose entry `run` answers its argument.
fn identity_function() -> lash_kernel_doc::FunctionValue {
    use lash_kernel_doc::Document;
    use lash_kernel_doc::Entry;
    use lash_kernel_doc::Expr;
    use lash_kernel_doc::Function;
    use lash_kernel_doc::NumberPolicy;
    use lash_kernel_doc::StartConvention;
    use lash_kernel_doc::Stmt;
    use lash_vocabulary::legacy_names::Name;
    use lash_vocabulary::types::Param;
    use lash_vocabulary::types::Signature;
    use lash_vocabulary::types::Type;
    let run = Name::new(lash_kernel_doc::FUNCTION_VALUE_ENTRY);
    let mut document = Document::new(NumberPolicy::Float, Vec::new());
    document.functions.insert(
        run.clone(),
        Function {
            params: vec![Name::new("x")],
            body: vec![Stmt::Return {
                value: Expr::Variable(Name::new("x")),
            }],
            start: None,
        },
    );
    document.entries.insert(
        run.clone(),
        Entry {
            signature: Signature {
                params: vec![Param {
                    name: Name::new("x"),
                    ty: Type::Any,
                    optional: false,
                }],
                result: Type::Any,
            },
            convention: StartConvention::Direct,
        },
    );
    lash_kernel_doc::FunctionValue {
        captions: lash_kernel_doc::Captions::empty(document.identity()),
        document,
        entry: run,
    }
}

/// INLINE-LINKED (FIG-6993): a function a cell linked under set A starts
/// inline records A on its start, and a node whose own set is B that takes
/// the start over resolves its owned operations under A, as the starting
/// node does: which node reads a start never changes what it runs under.
#[tokio::test]
async fn an_inline_start_resolves_its_operations_under_its_starters_set_on_any_node() {
    let sets = two_sets();
    let linked = sets.writer.digest();
    let starter = engine_holding(&sets.writer, &sets.writer);
    let taker = engine_holding(&sets.node, &sets.writer);
    let (run, declared) = run_declared(&sets.writer);
    let starts = lash_core::ProcessEngineRegistry::new()
        .with_registration(crate::kernel_process_engine_registration(starter))
        .inline_starts(crate::LASH_VM_ENGINE_KIND)
        .expect("the kernel engine starts functions inline");
    let payload = identity_function()
        .transport_json()
        .expect("the value encodes");
    let inline = starts
        .inspect(&payload, linked)
        .await
        .expect("the function checks under the cell's set");
    let resolved = lash_execution_contracts::process::ResolvedProcessDefinition {
        definition: lash_execution_contracts::process::ProcessDefinition::new(
            inline.draft.id(),
            inline.signature,
        ),
        draft: inline.draft,
    };
    let start = resolved
        .start_payload(&serde_json::Map::new())
        .expect("a start payload");
    assert_eq!(
        crate::KernelProcessInput::from_payload(&start)
            .expect("the kernel's start")
            .packages,
        linked,
        "the start records its starter's set"
    );
    let performed = BTreeMap::from([(run.clone(), declared.requirement.clone())]);
    let owned = taker
        .owned_operations(&start, &performed)
        .await
        .expect("the starter's set's declarations read");
    assert_eq!(
        owned.get(&run),
        Some(&declared),
        "the taking node resolves under the starter's set, not its own"
    );
}

/// A set neither the node nor the store holds.
fn unstored_digest(lash: &[lash_kernel_package::Package]) -> lash_kernel_doc::PackageSetDigest {
    let kernel = lash
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package");
    let other = lash_kernel_package::Package::build(
        "other",
        "1",
        &lash_kernel_package::PackageSources {
            kernel: "function other.f() -> Int\nkernel 1\ncharge 1\nbody {\n  return 1\n}\n",
            surface: "function f() -> Int = other.f\n",
            labels: "",
        },
        &[kernel],
    )
    .expect("other builds");
    lash_kernel_package::HeldPackages::install(lash.iter().cloned().chain([other]))
        .expect("another set")
        .digest()
}
