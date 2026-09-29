//! The delegating base for [`DeploymentStore`] decorators (ADR 0112 §2).
//!
//! The runtime half of the operation list lives beside the segment traits in
//! `lash-core-store` and generates [`RuntimeStoreDecorator`]. The deployment
//! operations name execution types, so their half of the list lives here,
//! beside [`DeploymentStore`], and generates [`DeploymentStoreDecorator`] and
//! its blanket [`DeploymentStore`] implementation.
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::DeploymentStore;
use crate::store::{
    ControlIntent, ControlIntentId, MaintenanceResult, ParkFeedCursor, ParkFeedPage,
    RetentionBound, RetentionReport, RootTerminal, RuntimeStoreDecorator, TurnPark, TurnParkQuery,
    TurnParkTarget, UnsettledTurnCounts,
};
use crate::{ExecutionScope, StoreError};

/// Every asynchronous [`DeploymentStore`] operation: the single place each
/// deployment signature is written. `bind_effect_host`, the one synchronous
/// operation, is forwarded by hand below.
///
/// The `decorator_surface_covers_every_deployment_method` lint fails when
/// this list and the trait disagree.
macro_rules! deployment_operations {
    ($emit:ident) => {
        $emit! {
            fn count_unsettled_turns(&self) -> Result<UnsettledTurnCounts, StoreError>;
            fn list_turn_parks(&self, query: &TurnParkQuery) -> Result<Vec<TurnPark>, StoreError>;
            fn turn_park_feed(&self, after: ParkFeedCursor, limit: NonZeroUsize) -> Result<ParkFeedPage<TurnParkTarget>, StoreError>;
            fn compact_turn_park_feed(&self, through: ParkFeedCursor) -> Result<(), StoreError>;
            fn non_terminal_roots_page(&self, after: Option<&crate::engine::RootRef>, limit: NonZeroUsize) -> Result<Vec<crate::engine::RootRef>, StoreError>;
            fn end_lost_root(&self, target: &crate::engine::RootRef, at_ms: u64) -> Result<Option<RootTerminal>, StoreError>;
            fn list_control_intents(&self, after: Option<ControlIntentId>, limit: NonZeroUsize) -> Result<Vec<ControlIntent>, StoreError>;
            fn retire_turn_cancel_closure_scope(&self, scope: &ExecutionScope) -> Result<(), StoreError>;
            fn reclaim_retained_evidence(&self, bound: RetentionBound) -> MaintenanceResult<RetentionReport>;
        }
    };
}

macro_rules! emit_deployment_decorator {
    ($(
        $(#[$meta:meta])*
        fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
    )*) => {
        /// Delegating base for [`DeploymentStore`] decorators.
        ///
        /// A deployment decorator is a [`RuntimeStoreDecorator`] whose inner
        /// store is a deployment. It overrides only the operations it
        /// intercepts: every runtime operation forwards through
        /// [`RuntimeStoreDecorator`], every deployment operation through the
        /// defaults here, the deployment's control-intent ledger through
        /// [`RuntimeStoreDecorator`]'s control-intent hooks, and its
        /// attachment root set wholesale.
        ///
        /// A decorator must not implement [`DeploymentStore`] directly; doing
        /// so would overlap the blanket implementation below.
        #[async_trait::async_trait]
        pub trait DeploymentStoreDecorator:
            RuntimeStoreDecorator<Inner: DeploymentStore>
        {
            /// Forwards [`DeploymentStore::bind_effect_host`].
            fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>) {
                self.inner().bind_effect_host(effect_host);
            }

            $(
                $(#[$meta])*
                async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                    self.inner().$name($($arg),*).await
                }
            )*
        }

        #[async_trait::async_trait]
        impl<T> DeploymentStore for T
        where
            T: DeploymentStoreDecorator + ?Sized,
        {
            fn bind_effect_host(&self, effect_host: &Arc<dyn crate::EffectHost>) {
                DeploymentStoreDecorator::bind_effect_host(self, effect_host);
            }

            $(
                $(#[$meta])*
                async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                    DeploymentStoreDecorator::$name(self $(, $arg)*).await
                }
            )*
        }

        /// The listed operations, for the surface lint.
        #[cfg(test)]
        const DEPLOYMENT_OPERATIONS: &[&str] = &[$(stringify!($name)),*];
    };
}

deployment_operations!(emit_deployment_decorator);

#[cfg(test)]
mod tests {
    #[test]
    // Architecture lint: lexical drift guard between `DeploymentStore` and
    // the deployment operation list, not a behavior proof.
    fn decorator_surface_covers_every_deployment_method() {
        let source = include_str!("vocabulary.rs");
        let start = source
            .find("pub trait DeploymentStore:")
            .expect("`pub trait DeploymentStore` is declared in vocabulary.rs");
        let body = &source[start..];
        let end = body
            .find("\n}\n")
            .expect("the `DeploymentStore` body closes at column zero");
        let lines: Vec<&str> = body[..end].lines().collect();
        let mut declared = std::collections::BTreeSet::new();
        for (index, line) in lines.iter().enumerate() {
            let Some(rest) = line.strip_prefix("    ") else {
                continue;
            };
            let rest = rest.strip_prefix("async ").unwrap_or(rest);
            let Some(rest) = rest.strip_prefix("fn ") else {
                continue;
            };
            let name = rest.split('(').next().unwrap_or_default().to_string();
            let has_default = lines[index..]
                .iter()
                .map(|line| line.trim_end())
                .find(|line| line.ends_with(';') || line.ends_with('{'))
                .is_some_and(|line| line.ends_with('{'));
            assert!(
                !has_default,
                "`DeploymentStore::{name}` has a default; every deployment operation is required"
            );
            declared.insert(name);
        }
        let mut listed: std::collections::BTreeSet<String> = super::DEPLOYMENT_OPERATIONS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        listed.insert("bind_effect_host".to_string());
        assert_eq!(
            declared, listed,
            "`deployment_operations!` must list exactly the `DeploymentStore` operations"
        );
    }
    #[test]
    // A deployment decorator intercepts the control-intent ledger through its
    // hooks; implementing `ControlIntentStore` directly would overlap the
    // blanket implementation (E0119).
    fn a_deployment_decorator_intercepts_control_intents_through_its_hooks() {
        use crate::StoreError;
        use crate::runtime::DeploymentStore;
        use crate::store::{
            ClaimToken, ControlIntentId, ControlIntentStore, IntentSettle, RuntimeStoreDecorator,
        };

        struct AcknowledgeHook(std::sync::Arc<dyn DeploymentStore>);

        #[async_trait::async_trait]
        impl RuntimeStoreDecorator for AcknowledgeHook {
            type Inner = dyn DeploymentStore;

            fn inner(&self) -> &Self::Inner {
                self.0.as_ref()
            }

            async fn acknowledge_intent(
                &self,
                id: ControlIntentId,
                claim: &ClaimToken,
                at_ms: u64,
            ) -> Result<IntentSettle, StoreError> {
                self.inner().acknowledge_intent(id, claim, at_ms).await
            }
        }

        impl super::DeploymentStoreDecorator for AcknowledgeHook {}

        fn is_a_deployment<T: DeploymentStore + ControlIntentStore + ?Sized>() {}
        is_a_deployment::<AcknowledgeHook>();
    }
}
