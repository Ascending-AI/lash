//! R5 (FIG-6985): stored code performs a package's internal operation only
//! when a stored snapshot lists it as that package's own.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::graph_tokens::encode_body;
use lash_kernel_doc::{
    EffectName, FunctionDefinition, FunctionId, KernelDataLimits, parse_definition,
};
use lash_vm_broker::kernel::loader::{LoadRefusal, decode};

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

/// PKG-RUNNER (FIG-6985): a start's runners are judged by the declarations
/// of the package set it was linked under, read from the store where the
/// node does not hold it, not by the node's own set. A node holding lash's
/// packages alone starts a definition linked under a set that adds
/// `dataframe`: `dataframe` owns `run` there, under its declared contract.
/// A start naming no set is linked under the node's own, where no package
/// owns it, and one naming a set the store keeps no surface of is refused.
#[tokio::test]
async fn a_start_is_judged_by_the_declarations_of_the_set_it_was_linked_under() {
    let lash = lash_vm_releases::built().expect("lash's packages");
    let kernel = lash
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package");
    let dataframe = lash_kernel_package::testing::dataframe(kernel).expect("the fixture builds");
    let writer =
        lash_kernel_package::HeldPackages::install(lash.iter().cloned().chain([dataframe]))
            .expect("the writer's packages");
    let linked = writer.own();
    let node = Arc::new(
        lash_kernel_package::HeldPackages::install(lash.iter().cloned())
            .expect("the node's packages"),
    );
    assert!(node.surface(&linked).is_none(), "the node does not hold it");
    let run = EffectName::new("dataframe.run").expect("an operation");
    let declared = crate::owned_operation(&writer.view(&linked).expect("the writer's set"), &run)
        .expect("the fixture owns `run`");
    let config =
        lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
    let engine = crate::process_plugin::KernelProcessPluginFactory::new(
        lash_vm_client::service::Service::new(config.clone()),
        lash_vm_releases::TYPESCRIPT,
        config.run_bounds,
    )
    .with_packages(node)
    .engine(Arc::new(NoArtifacts))
    .expect("the engine")
    .with_internal_authority(Arc::new(Snapshots {
        surfaces: BTreeMap::from([(
            linked,
            writer.surface(&linked).expect("its surface").encode(),
        )]),
        ..Snapshots::default()
    }));
    let document =
        lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { }").expect("a document");
    let start = |packages| {
        serde_json::to_value(crate::KernelProcessInput {
            document: document.identity(),
            functions: BTreeSet::new(),
            entry: lash_kernel_doc::Name::new("work"),
            packages,
            args: serde_json::Map::new(),
        })
        .expect("a start")
    };
    let performed = BTreeMap::from([(run.clone(), declared.requirement.clone())]);

    let owned = engine
        .owned_operations(&start(Some(linked)), &performed)
        .await
        .expect("the linked set's declarations read");
    assert_eq!(owned.get(&run), Some(&declared));
    assert!(
        engine
            .owned_operations(&start(None), &performed)
            .await
            .expect("the node's own set")
            .is_empty(),
        "no package of the node's own set owns `run`"
    );
    assert!(
        engine
            .owned_operations(&start(Some(unstored_digest(&lash))), &performed)
            .await
            .is_err(),
        "a set the store keeps no surface of judges no runner"
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
        .own()
}
