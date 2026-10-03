//! A real deferring tool for the soak's checker coverage.

use std::sync::{Arc, Mutex, OnceLock};

use crate::invariants::ToolObserver;
use lash_core::sync::MutexExt as _;

#[derive(Default)]
pub(super) struct WitnessTool {
    observer: OnceLock<ToolObserver>,
    key: Mutex<Option<lash_core::AwaitEventKey>>,
}

impl WitnessTool {
    pub(super) fn observe(&self, observer: ToolObserver) {
        assert!(
            self.observer.set(observer).is_ok(),
            "observer already installed"
        );
    }

    pub(super) fn take_key(&self) -> Option<lash_core::AwaitEventKey> {
        self.key.lock_recover().take()
    }

    #[expect(
        clippy::expect_used,
        reason = "The soak tool definition contains fixed valid schema literals"
    )]
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:soak_witness",
            "soak_witness",
            "Await the soak host's completion.",
            serde_json::json!({"type":"object", "properties":{}, "additionalProperties":false}),
            serde_json::json!({"type":"object"}),
        )
        .expect("valid soak witness schemas")
        .with_declaration(lash_core::ToolDeclaration::deferring())
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for WitnessTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "soak_witness").then(|| Arc::new(Self::definition().contract()))
    }
    #[expect(
        clippy::expect_used,
        reason = "Driver installs the observer before deploying the tool"
    )]
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let observer = self
            .observer
            .get()
            .expect("the soak installed its observer before deployment");
        let observed = observer.executed(call.context);
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
        };
        observer.registered(observed, &key);
        *self.key.lock_recover() = Some(key);
        lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into()
    }
}
