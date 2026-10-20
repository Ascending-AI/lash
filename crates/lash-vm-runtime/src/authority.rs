//! Internal-operation authority (FIG-6985, R5): stored code performs an
//! operation a package declares internal only when it is that package's
//! own code.
//!
//! A package may declare an operation internal: only its own bodies
//! perform it. Code a node runs from the store carries no proof of whose
//! it is beyond its name, which anyone can write, so authority is
//! membership: a definition performs package P's internal operation only
//! when some stored snapshot lists it as one of P's definitions. Only a
//! trusted install writes snapshots, and none is ever deleted, so the
//! authority holds across rollback.
//!
//! The check reads each definition's own direct performs
//! (`FunctionBody::effects`): a function it calls answers for its own,
//! and a callback inherits nothing from its caller. The node's own
//! registrations are its trusted build and are not checked again.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_kernel_doc::{EffectName, FunctionDefinition, FunctionId};
use lash_vm_broker::kernel::loader::LoadRefusal;

/// Where the store's answers about package membership come from: the
/// operations packages declare internal, and the packages each definition
/// is a member of in some stored snapshot.
#[async_trait::async_trait]
pub trait InternalAuthority: Send + Sync {
    /// The paths of the packages that declare each of `operations`
    /// internal (`DurableStore::internal_operations`); an operation no
    /// package declares internal is absent.
    ///
    /// # Errors
    ///
    /// The store's failure to answer.
    async fn internal_operations(
        &self,
        operations: &[EffectName],
    ) -> Result<BTreeMap<EffectName, BTreeSet<String>>, AuthorityUnread>;

    /// The path of every package a stored snapshot lists each of
    /// `functions` as a definition of (`DurableStore::package_paths`); a
    /// function no snapshot lists is absent.
    ///
    /// # Errors
    ///
    /// The store's failure to answer.
    async fn package_paths(
        &self,
        functions: &[FunctionId],
    ) -> Result<BTreeMap<FunctionId, BTreeSet<String>>, AuthorityUnread>;

    /// The surface snapshot bytes stored for the set `packages`
    /// (`DurableStore::package_surface`): what its packages declare of the
    /// operations they own, which judges a start linked under it
    /// (PKG-RUNNER). `None` for a set the store does not keep.
    ///
    /// # Errors
    ///
    /// The store's failure to answer.
    async fn package_surface(
        &self,
        packages: &lash_kernel_doc::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, AuthorityUnread>;
}

/// The store did not answer an authority read: the run is retried, never
/// admitted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("internal-operation authority is not read: {0}")]
pub struct AuthorityUnread(pub String);

/// The authority of a store that holds no snapshot: no package declares
/// an operation internal, so nothing is refused, and no surface is kept.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoInternalOperations;

#[async_trait::async_trait]
impl InternalAuthority for NoInternalOperations {
    async fn internal_operations(
        &self,
        _operations: &[EffectName],
    ) -> Result<BTreeMap<EffectName, BTreeSet<String>>, AuthorityUnread> {
        Ok(BTreeMap::new())
    }

    async fn package_paths(
        &self,
        _functions: &[FunctionId],
    ) -> Result<BTreeMap<FunctionId, BTreeSet<String>>, AuthorityUnread> {
        Ok(BTreeMap::new())
    }

    async fn package_surface(
        &self,
        _packages: &lash_kernel_doc::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, AuthorityUnread> {
        Ok(None)
    }
}

/// The authority a store's installs grant: its membership index and the
/// operations its stored packages declare internal, which only an install
/// writes (FIG-6991). The store set's durable store is taken on first
/// use, as the backend takes it.
pub struct StoredAuthority {
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    durable: std::sync::OnceLock<Arc<dyn lash_durable::DurableStore>>,
}

impl StoredAuthority {
    /// The authority of `stores`' durable store.
    #[must_use]
    pub fn new(stores: Arc<dyn lash_execution_contracts::backend::StoreSet>) -> Self {
        Self {
            stores,
            durable: std::sync::OnceLock::new(),
        }
    }

    fn durable(&self) -> &Arc<dyn lash_durable::DurableStore> {
        self.durable.get_or_init(|| self.stores.durable_store())
    }
}

#[async_trait::async_trait]
impl InternalAuthority for StoredAuthority {
    async fn internal_operations(
        &self,
        operations: &[EffectName],
    ) -> Result<BTreeMap<EffectName, BTreeSet<String>>, AuthorityUnread> {
        self.durable()
            .internal_operations(operations)
            .await
            .map_err(|error| AuthorityUnread(error.to_string()))
    }

    async fn package_paths(
        &self,
        functions: &[FunctionId],
    ) -> Result<BTreeMap<FunctionId, BTreeSet<String>>, AuthorityUnread> {
        self.durable()
            .package_paths(functions)
            .await
            .map_err(|error| AuthorityUnread(error.to_string()))
    }

    async fn package_surface(
        &self,
        packages: &lash_kernel_doc::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, AuthorityUnread> {
        self.durable()
            .package_surface(packages)
            .await
            .map_err(|error| AuthorityUnread(error.to_string()))
    }
}

/// Admits `stored`, the definitions a run reads from the store, under
/// `authority`: each operation a definition performs itself that a
/// package declares internal must be performed by a member of a package
/// that declares it.
///
/// # Errors
///
/// [`AuthorityUnread`]; the inner [`LoadRefusal::InternalOperation`] names
/// the first definition, by identity, that performs another package's
/// internal operation, and the operation.
pub async fn admit_internal(
    authority: &dyn InternalAuthority,
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
) -> Result<Result<(), LoadRefusal>, AuthorityUnread> {
    let performs = |definition: &FunctionDefinition| -> BTreeSet<EffectName> {
        definition
            .body()
            .map(|body| body.effects.keys().cloned().collect())
            .unwrap_or_default()
    };
    let performed: BTreeSet<EffectName> = stored
        .values()
        .flat_map(|definition| performs(definition))
        .collect();
    if performed.is_empty() {
        return Ok(Ok(()));
    }
    let performed: Vec<EffectName> = performed.into_iter().collect();
    let owners = authority.internal_operations(&performed).await?;
    let performers: Vec<FunctionId> = stored
        .iter()
        .filter(|(_, definition)| {
            definition.body().is_some_and(|body| {
                body.effects
                    .keys()
                    .any(|effect| owners.contains_key(effect))
            })
        })
        .map(|(function, _)| *function)
        .collect();
    if performers.is_empty() {
        return Ok(Ok(()));
    }
    let members = authority.package_paths(&performers).await?;
    for function in performers {
        let paths = members.get(&function);
        for operation in performs(&stored[&function]) {
            let Some(owners) = owners.get(&operation) else {
                continue;
            };
            if !paths.is_some_and(|paths| !paths.is_disjoint(owners)) {
                return Ok(Err(LoadRefusal::InternalOperation {
                    function,
                    operation,
                }));
            }
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
#[path = "authority_tests.rs"]
mod tests;
