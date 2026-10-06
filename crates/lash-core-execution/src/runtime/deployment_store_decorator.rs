//! The delegating base for [`DeploymentStore`] decorators (ADR 0112 §2).
//!
//! The runtime half of the operation list lives beside the segment traits in
//! `lash-core-store` and generates [`RuntimeStoreDecorator`]. The deployment
//! operations name execution types, so their half of the list lives here,
//! beside [`DeploymentStore`], and generates [`DeploymentStoreDecorator`] and
//! its blanket [`DeploymentStore`] implementation.
use std::num::NonZeroUsize;

use super::DeploymentStore;
use crate::store::{
    ControlIntent, ControlIntentId, MaintenanceResult, ParkFeedCursor, ParkFeedPage,
    RetentionBound, RetentionReport, RunTerminal, RuntimeStoreDecorator, TurnChangeCursor,
    TurnChangePage, TurnPark, TurnParkQuery, TurnParkTarget, UnsettledTurnCounts,
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
            fn artifact_frame_is_retained(&self, frame: &crate::FrameEnvironmentId) -> Result<bool, StoreError>;
            fn count_unsettled_turns(&self) -> Result<UnsettledTurnCounts, StoreError>;
            fn list_turn_parks(&self, query: &TurnParkQuery) -> Result<Vec<TurnPark>, StoreError>;
            fn turns_changed_since(&self, after: TurnChangeCursor, limit: NonZeroUsize) -> Result<TurnChangePage, StoreError>;
            fn turn_park_feed(&self, after: ParkFeedCursor, limit: NonZeroUsize) -> Result<ParkFeedPage<TurnParkTarget>, StoreError>;
            fn compact_turn_park_feed(&self, through: ParkFeedCursor) -> Result<(), StoreError>;
            fn non_terminal_runs_page(&self, after: Option<&crate::engine::RunRef>, limit: NonZeroUsize) -> Result<Vec<crate::engine::OpenRun>, StoreError>;
            fn end_lost_run(&self, target: &crate::engine::RunRef, loss: crate::engine::RunLoss, at_ms: u64) -> Result<Option<RunTerminal>, StoreError>;
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

/// `DeploymentOp` and the deployment half of the scripted store
/// (`lash_core_store::testing::script`): every listed deployment operation,
/// routed through the law's script.
#[cfg(any(test, feature = "testing"))]
macro_rules! emit_scripted_deployment {
    ($(
        $(#[$meta:meta])*
        fn $name:ident(&self $(, $arg:ident: $arg_ty:ty)*) -> $ret:ty;
    )*) => {
        /// A deployment operation a law scripts, named as the operation list
        /// names it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[expect(
            non_camel_case_types,
            reason = "each variant is spelled as the operation it names"
        )]
        pub enum DeploymentOp {
            $($name,)*
        }

        impl DeploymentOp {
            /// Every scriptable deployment operation, in list order.
            pub const ALL: &[DeploymentOp] = &[$(DeploymentOp::$name,)*];

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$name => stringify!($name),)*
                }
            }
        }

        impl From<DeploymentOp> for lash_core_store::testing::Op {
            fn from(op: DeploymentOp) -> Self {
                Self::listed(op.name())
            }
        }

        #[async_trait::async_trait]
        impl<S> DeploymentStoreDecorator for lash_core_store::testing::Scripted<S>
        where
            S: DeploymentStore + ?Sized,
        {
            $(
                $(#[$meta])*
                async fn $name(&self $(, $arg: $arg_ty)*) -> $ret {
                    self.call(DeploymentOp::$name, self.inner().$name($($arg),*)).await
                }
            )*
        }
    };
}

deployment_operations!(emit_deployment_decorator);
#[cfg(any(test, feature = "testing"))]
deployment_operations!(emit_scripted_deployment);

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
    // Architecture lint: the scripted deployment's operations are the listed
    // ones, so a rule on any `DeploymentOp` can fire.
    fn every_deployment_operation_is_scriptable() {
        let scriptable: Vec<&str> = super::DeploymentOp::ALL
            .iter()
            .map(|op| op.name())
            .collect();
        assert_eq!(scriptable, super::DEPLOYMENT_OPERATIONS);

        fn is_a_deployment<T: crate::runtime::DeploymentStore + ?Sized>() {}
        is_a_deployment::<lash_core_store::testing::Scripted<dyn crate::runtime::DeploymentStore>>(
        );
    }
}
