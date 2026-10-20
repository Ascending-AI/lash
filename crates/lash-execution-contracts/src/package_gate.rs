//! Which package surface a cell, and a workflow import, is written under
//! (FIG-6918, FIG-6991).
//!
//! Every package set a node installs is stored with its surface snapshot,
//! what running its code requires and all its definitions. The gate
//! answers the surface each cell is written under, and the answer is the
//! cell's one key (P3): the prompt shows that surface, the worker lowers
//! against its names, the cell's saved functions relink to it, and its
//! declared operations judge the cell's runners. A node built with another
//! set reads the surface from its stored snapshot and the definitions the
//! store holds for it.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry, PackageSetDigest};
use lash_kernel_package::{HeldPackages, PackageView, SurfaceSnapshot};

/// The package surface a cell is written under: the gate's answer, over
/// a catalog that holds every definition it names.
#[derive(Clone)]
pub struct CellPackages {
    surface: Arc<SurfaceSnapshot>,
    catalog: Arc<FunctionRegistry>,
    /// `None` for the node's own set; else what it read from the store.
    stored: Option<Arc<Stored>>,
}

/// A stored surface not the node's own: the definitions it names that the
/// node lacks, and both as a worker is sent them.
struct Stored {
    definitions: BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    surface: Vec<u8>,
    images: Vec<Vec<u8>>,
}

impl CellPackages {
    /// The node's own set, over its registry.
    #[must_use]
    pub fn held(held: &HeldPackages) -> Self {
        Self {
            surface: Arc::clone(held.surface()),
            catalog: Arc::clone(held.functions()),
            stored: None,
        }
    }

    /// A stored surface not the node's own, over `catalog`, the node's
    /// registry with `stored` registered over it
    /// ([`HeldPackages::catalog_with`]).
    #[must_use]
    pub fn stored(
        surface: Arc<SurfaceSnapshot>,
        catalog: Arc<FunctionRegistry>,
        stored: BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    ) -> Self {
        let images = stored
            .values()
            .map(|definition| lash_kernel_doc::graph_tokens::encode_body(definition.as_ref()))
            .collect();
        Self {
            stored: Some(Arc::new(Stored {
                surface: surface.encode(),
                images,
                definitions: stored,
            })),
            surface,
            catalog,
        }
    }

    /// The set's digest.
    #[must_use]
    pub fn digest(&self) -> PackageSetDigest {
        self.surface.digest()
    }

    /// The surface every reader of the cell's packages takes.
    #[must_use]
    pub fn view(&self) -> PackageView<'_> {
        PackageView::new(&self.surface, &self.catalog)
    }

    pub fn surface(&self) -> &Arc<SurfaceSnapshot> {
        &self.surface
    }

    /// The registry a document written under the set is checked against:
    /// the node's own, with a stored surface's definitions over it.
    #[must_use]
    pub fn catalog(&self) -> &Arc<FunctionRegistry> {
        &self.catalog
    }

    /// The definitions the surface names that the node does not hold.
    pub fn stored_definitions(&self) -> &BTreeMap<FunctionId, Arc<FunctionDefinition>> {
        static NONE: BTreeMap<FunctionId, Arc<FunctionDefinition>> = BTreeMap::new();
        self.stored
            .as_ref()
            .map_or(&NONE, |stored| &stored.definitions)
    }

    /// For a stored surface, its encoded snapshot and the token bodies of
    /// the definitions the node lacks: what a worker lowering under it is
    /// sent. `None` for the node's own set, which a worker is sent by
    /// digest.
    pub fn stored_wire(&self) -> Option<(&[u8], &[Vec<u8>])> {
        self.stored
            .as_ref()
            .map(|stored| (stored.surface.as_slice(), stored.images.as_slice()))
    }
}

impl PartialEq for CellPackages {
    fn eq(&self, other: &Self) -> bool {
        self.digest() == other.digest()
    }
}

impl Eq for CellPackages {}

impl std::fmt::Debug for CellPackages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CellPackages")
            .field("digest", &self.digest())
            .field("stored", &self.stored_definitions().len())
            .finish()
    }
}

/// The package surface a cell lowered now is written under.
#[async_trait::async_trait]
pub trait PackageGate: Send + Sync {
    /// The surface a cell lowered now is written under. A typed retryable
    /// error defers lowering when the fleet has none to answer, or when the
    /// live survey or the stored surface is unavailable.
    async fn writable(&self) -> Result<CellPackages, crate::RuntimeEffectControllerError>;
}
