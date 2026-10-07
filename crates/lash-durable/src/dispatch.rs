//! [`ActorDispatch`]: one node's activation, routing each claimed actor to
//! its kind's activation.

use std::sync::Arc;

use crate::ids::ActorKind;
use crate::runner::{Activation, Exit, Owned};

/// Routes a claimed actor by its [`ActorKind`]: sessions to the session
/// activation (V0, then L3), processes to the process activation (L6).
#[derive(Clone)]
pub struct ActorDispatch {
    /// Runs claimed sessions.
    pub session: Arc<dyn Activation>,
    /// Runs claimed processes.
    pub process: Arc<dyn Activation>,
}

#[async_trait::async_trait]
impl Activation for ActorDispatch {
    async fn activate(&self, owned: Owned) -> Exit {
        match owned.actor().kind() {
            ActorKind::Session => self.session.activate(owned).await,
            ActorKind::Process => self.process.activate(owned).await,
        }
    }
}
