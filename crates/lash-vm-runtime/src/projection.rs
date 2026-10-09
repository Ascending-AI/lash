//! Projection providers: who answers a run's read through a handle.
//!
//! A projection is a kernel handle, and a read through it is a host read
//! (kernel spec §9): the machine hands the host the handle and a request,
//! both kernel data, and the provider registered for the handle's kind
//! answers with kernel data or with the error the read raises. A provider
//! knows nothing of any language: what a value is worth as a condition, a
//! number or a string is the kernel's to say, once the data is in the run.
//! Reads are never recorded; whatever ran after a save reads again.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Handle};

/// The reads of one handle kind.
#[async_trait::async_trait]
pub trait ProjectionProvider: Send + Sync {
    /// The handle kind this provider answers.
    fn kind(&self) -> &str;

    /// One read of `handle`. `Err` is raised in the guest, which may catch
    /// it.
    async fn read(&self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum>;
}

/// Why a provider could not be registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionRefusal {
    /// Two providers answer one kind.
    #[error("two projection providers for `{kind}`")]
    Duplicate { kind: String },
}

/// The providers registered on a backend, by handle kind.
#[derive(Clone, Default)]
pub struct ProjectionCatalog {
    providers: BTreeMap<String, Arc<dyn ProjectionProvider>>,
}

impl std::fmt::Debug for ProjectionCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_set()
            .entries(self.providers.keys())
            .finish()
    }
}

impl ProjectionCatalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under its kind.
    ///
    /// # Errors
    ///
    /// [`ProjectionRefusal::Duplicate`] when its kind has a provider.
    pub fn register(
        &mut self,
        provider: Arc<dyn ProjectionProvider>,
    ) -> Result<(), ProjectionRefusal> {
        let kind = provider.kind().to_owned();
        if self.providers.contains_key(&kind) {
            return Err(ProjectionRefusal::Duplicate { kind });
        }
        self.providers.insert(kind, provider);
        Ok(())
    }

    /// The catalog a backend carries, read back typed; empty for a backend
    /// built without providers.
    pub fn of_backend(
        providers: Option<
            &dyn lash_core_execution::runtime::actor::projection::ProjectionProviders,
        >,
    ) -> Self {
        providers
            .and_then(|providers| providers.as_any().downcast_ref::<Self>())
            .cloned()
            .unwrap_or_default()
    }

    /// The registered kinds.
    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    /// Answers one read through the provider of the handle's kind. A kind
    /// with no provider raises `no_projection_provider` in the guest: it is
    /// never a placeholder value.
    pub async fn answer(&self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        match self.providers.get(&handle.kind) {
            Some(provider) => provider.read(handle, request).await,
            None => Err(ErrorDatum {
                kind: "no_projection_provider".to_owned(),
                message: format!("no projection provider for `{}`", handle.kind),
                data: Datum::Null,
            }),
        }
    }
}

impl lash_core_execution::runtime::actor::projection::ProjectionProviders for ProjectionCatalog {
    fn projection_types(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rows;

    #[async_trait::async_trait]
    impl ProjectionProvider for Rows {
        fn kind(&self) -> &str {
            "rows"
        }

        async fn read(&self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
            Ok(Datum::List(vec![
                Datum::Text(handle.id.clone()),
                request.clone(),
            ]))
        }
    }

    /// Target 3: a read is answered by the provider registered for the
    /// handle's kind, with kernel data; a kind nobody answers is an error
    /// the guest can catch, and one kind has one provider.
    #[tokio::test]
    async fn a_read_is_answered_by_the_provider_of_the_handles_kind() {
        let mut catalog = ProjectionCatalog::new();
        catalog.register(Arc::new(Rows)).expect("first");
        assert_eq!(
            catalog.register(Arc::new(Rows)),
            Err(ProjectionRefusal::Duplicate {
                kind: "rows".into()
            })
        );
        let handle = |kind: &str| Handle {
            kind: kind.into(),
            id: "r1".into(),
        };
        assert_eq!(
            catalog.answer(&handle("rows"), &Datum::Bool(true)).await,
            Ok(Datum::List(vec![
                Datum::Text("r1".into()),
                Datum::Bool(true)
            ]))
        );
        let refused = catalog
            .answer(&handle("gone"), &Datum::Null)
            .await
            .expect_err("no provider");
        assert_eq!(refused.kind, "no_projection_provider");
    }
}
