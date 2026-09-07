use std::sync::{Arc, Mutex};

use lash_core::{RuntimePersistence, SessionExecutionLeaseAuthority, StoreError};
use tokio::sync::oneshot;

/// The crash fixture owns the handoff from the dropped guard to its successor.
/// Holding the first release also forces the formerly losing schedule on every run.
struct ReleaseRendezvousStore {
    inner: Arc<dyn RuntimePersistence>,
    release: Mutex<Option<ReleaseHandshake>>,
}

struct ReleaseHandshake {
    permitted: oneshot::Receiver<()>,
    completed: oneshot::Sender<()>,
}

pub(super) struct ReleaseRendezvous {
    permit: oneshot::Sender<()>,
    completed: oneshot::Receiver<()>,
}

impl ReleaseRendezvous {
    pub(super) fn wrap(inner: Arc<dyn RuntimePersistence>) -> (Arc<dyn RuntimePersistence>, Self) {
        let (permit, permitted) = oneshot::channel();
        let (completed, completion) = oneshot::channel();
        (
            Arc::new(ReleaseRendezvousStore {
                inner,
                release: Mutex::new(Some(ReleaseHandshake {
                    permitted,
                    completed,
                })),
            }),
            Self {
                permit,
                completed: completion,
            },
        )
    }

    pub(super) async fn complete(self) {
        self.permit
            .send(())
            .expect("dropped guard retains its release gate");
        self.completed
            .await
            .expect("dropped guard acknowledges backend release");
    }
}

#[async_trait::async_trait]
impl lash_core::store::RuntimePersistenceDecorator for ReleaseRendezvousStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError> {
        let handshake = self.release.lock().expect("release handshake mutex").take();
        if let Some(handshake) = handshake {
            handshake
                .permitted
                .await
                .expect("fixture permits dropped guard release");
            self.inner
                .release_session_execution_lease(completion)
                .await?;
            handshake
                .completed
                .send(())
                .expect("fixture waits for release acknowledgement");
            Ok(())
        } else {
            self.inner.release_session_execution_lease(completion).await
        }
    }
}
