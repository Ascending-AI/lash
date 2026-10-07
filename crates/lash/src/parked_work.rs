//! The control intents a session's close records, listed for an operator.
//!
//! A parked process is its actor's state (ADR 0132 §11): it is recorded in
//! the substrate's park feed, and an operator redrives it by
//! `ProcessAdmin::redrive`.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lash_core::DeploymentStore;
use lash_core::store::{ControlIntent, ControlIntentId};

use crate::Result;

/// The deployment's control intents.
///
/// Obtained from [`LashCore::parked_work`](crate::LashCore::parked_work).
#[derive(Clone)]
pub struct ParkedWork {
    pub(crate) store_factory: Arc<dyn DeploymentStore>,
}

impl std::fmt::Debug for ParkedWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ParkedWork").finish_non_exhaustive()
    }
}

impl ParkedWork {
    /// A page of the deployment's control intents, oldest first.
    ///
    /// # Errors
    /// When the catalog refuses the read.
    pub async fn intents(&self, query: &ControlIntentQuery) -> Result<ControlIntentPage> {
        let mut intents = self
            .store_factory
            .list_control_intents(query.after, query.limit.saturating_add(1))
            .await?;
        let more = intents.len() > query.limit.get();
        intents.truncate(query.limit.get());
        let next = if more {
            intents.last().map(|intent| intent.id)
        } else {
            None
        };
        Ok(ControlIntentPage { intents, next })
    }
}

/// The filter and page a [`ParkedWork::intents`] read applies.
#[derive(Clone, Debug)]
pub struct ControlIntentQuery {
    pub after: Option<ControlIntentId>,
    pub limit: NonZeroUsize,
}

/// One page of control intents.
#[derive(Clone, Debug)]
pub struct ControlIntentPage {
    pub intents: Vec<ControlIntent>,
    pub next: Option<ControlIntentId>,
}
