//! The backend's projection providers (ADR 0132 §9; S8 of I0, FIG-5194).
//! Owned by L7p (FIG-5197).
//!
//! The provider trait, its catalog and the plain-data `ResourceRef` live in
//! `lashlang` beside the read types they answer (`ProjectedReadRequest`,
//! `ProjectedReadResponse`), because the VM sits above this crate. The
//! backend carries the built catalog behind [`ProjectionProviders`], and the
//! VM half reads it back as `lashlang::ProjectionCatalog`.

use std::any::Any;

/// A catalog of projection providers, as the backend carries it.
pub trait ProjectionProviders: Send + Sync + 'static {
    /// The projection types it has a provider for.
    fn projection_types(&self) -> Vec<String>;

    /// The catalog itself, for the VM half to read back typed.
    fn as_any(&self) -> &(dyn Any + Send + Sync);
}

/// A backend with no projection providers.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProjectionProviders;

impl ProjectionProviders for NoProjectionProviders {
    fn projection_types(&self) -> Vec<String> {
        Vec::new()
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}
