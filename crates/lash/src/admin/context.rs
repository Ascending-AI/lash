//! Shared runtime access for session capabilities. Domain operations live in their owners.

use crate::support::{EmbedError, LashRuntime, Result, RuntimeHandle, SessionError};

#[derive(Clone)]
pub(crate) struct AdminContext {
    pub(crate) target: crate::send::SendTarget,
    pub(crate) runtime: RuntimeHandle,
}

impl AdminContext {
    /// The body is the canonical `lock → call → publish_from` stamp shared by nearly every
    /// mutating control method; publish happens unconditionally once the closure returns.
    pub(crate) async fn with_writer<F, T>(&self, f: F) -> T
    where
        F: AsyncFnOnce(&mut LashRuntime) -> T,
    {
        let writer = self.runtime.writer();
        let mut runtime = writer.lock().await;
        let value = f(&mut runtime).await;
        self.runtime.publish_from(&runtime).await;
        value
    }

    /// The session's store, which every durable admin read answers from.
    pub(crate) fn head_store(&self) -> Result<lash_core::store::SessionStore> {
        self.runtime.observe().queue_store.clone().ok_or_else(|| {
            EmbedError::Session(SessionError::Protocol(
                "a durable admin read needs a store-backed session".to_string(),
            ))
        })
    }
}
