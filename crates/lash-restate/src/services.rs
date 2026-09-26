#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by Lash durable waits"
)]

//! The Restate services lash itself serves, and the one place that binds them.
//!
//! [`LashService`] names every service lash-restate addresses by name: the
//! ingress calls, the handler-side clients, and the error messages that name a
//! service nothing binds all spell it through this enum. [`bind_lash_services`]
//! is the only code that binds them, and it binds every variant, so a
//! deployment serves all of lash's services or none of them. A host reaches it
//! through [`RestateEngine::endpoint_builder`](crate::RestateEngine::endpoint_builder)
//! and binds only its own services on the builder it gets back.

use std::sync::Arc;

use restate_sdk::context::RunRetryPolicy;
use restate_sdk::endpoint::{Builder, HandlerOptions, ServiceOptions};
use restate_sdk::service::IntoServiceDefinition as _;

use crate::RestateEffectHost;
use crate::durable_wait::{
    LashDurableWaitIndex as _, LashDurableWaitIndexImpl, LashDurableWaitWorkflow as _,
    LashDurableWaitWorkflowImpl,
};
use crate::effect_group::{EffectGroupDispatch, EffectGroupIndex, EffectGroupPayload};
use crate::ingress::RestateIngressClient;
use crate::process::{LashProcessWorkflow as _, LashProcessWorkflowImpl, RestateProcessRunner};
use crate::process_attach::{LashProcessAttach as _, LashProcessAttachImpl};

/// Declares [`LashService`] and its complete [`LASH_SERVICES`] together, so
/// a variant cannot exist without being listed.
macro_rules! lash_services {
    ($($(#[$doc:meta])* $variant:ident => $name:literal,)+) => {
        /// A Restate service lash itself serves.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) enum LashService {
            $($(#[$doc])* $variant,)+
        }

        /// Every service lash serves, each once.
        pub(crate) const LASH_SERVICES: &[LashService] = &[$(LashService::$variant,)+];

        impl LashService {
            /// The service's Restate name: what an ingress call addresses and
            /// what the endpoint's discovery document reports.
            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)+
                }
            }
        }
    };
}

lash_services! {
    /// Exact-address promises and deadline timers for every await-event key.
    DurableWaitWorkflow => "LashDurableWaitWorkflow",
    /// The per-scope index that cancels, revokes and fences a scope's waits.
    DurableWaitIndex => "LashDurableWaitIndex",
    /// The segment runner a process submission starts and awaits.
    ProcessWorkflow => "LashProcessWorkflow",
    /// Arms a process terminal for a caller parked on it.
    ProcessAttach => "LashProcessAttach",
    /// An effect group's lifecycle and settlement rank.
    EffectGroupIndex => "EffectGroupIndex",
    /// An effect group's successful result bytes.
    EffectGroupPayload => "EffectGroupPayload",
    /// Sends an effect group's children and runs each one.
    EffectGroupDispatch => "EffectGroupDispatch",
}

/// What the lash services of one deployment run over.
pub(crate) struct LashServiceParts<'a, R> {
    /// The deployment's effect host: effect-group children route through the
    /// resolver registered on it and run under its authority.
    pub(crate) effect_host: &'a RestateEffectHost,
    /// The ingress the effect-group dispatcher watches cancellation through.
    pub(crate) ingress: RestateIngressClient,
    /// The session catalog a session-scope group child checks its state
    /// generation in before it runs (FIG-3619).
    pub(crate) sessions: Arc<dyn lash_core::SessionStoreFactory>,
    /// The process workflow over the deployment's process worker.
    pub(crate) process_workflow: LashProcessWorkflowImpl<R>,
}

/// Bind every [`LashService`] on `builder`.
///
/// The match is exhaustive over [`LASH_SERVICES`], so a new lash service
/// does not compile until it is bound here.
pub(crate) fn bind_lash_services<R: RestateProcessRunner>(
    builder: Builder,
    parts: LashServiceParts<'_, R>,
) -> Builder {
    let LashServiceParts {
        effect_host,
        ingress,
        sessions,
        process_workflow,
    } = parts;
    // `LASH_SERVICES` lists each variant once, so the process-workflow arm runs once and
    // always finds the workflow here.
    // A segment that keeps failing live stops after its deployment's attempt
    // bound and pauses, keeping its journal: the park reconcile parks its
    // process and a resume retries it (FIG-3675).
    let run_options = HandlerOptions::new()
        .retry_policy_max_attempts(process_workflow.retry_max_attempts())
        .retry_policy_pause_on_max_attempts();
    let mut process_workflow = Some(
        process_workflow
            .serve()
            .into_service_definition()
            .options(ServiceOptions::new().handler("run", run_options)),
    );
    LASH_SERVICES
        .iter()
        .fold(builder, |builder, service| match service {
            LashService::DurableWaitWorkflow => builder.bind(LashDurableWaitWorkflowImpl.serve()),
            LashService::DurableWaitIndex => builder.bind(LashDurableWaitIndexImpl.serve()),
            LashService::ProcessWorkflow => match process_workflow.take() {
                Some(process_workflow) => builder.bind(process_workflow),
                None => builder,
            },
            LashService::ProcessAttach => builder.bind(LashProcessAttachImpl.serve()),
            LashService::EffectGroupIndex => builder.bind(EffectGroupIndex),
            LashService::EffectGroupPayload => builder.bind(EffectGroupPayload),
            // Dispatcher preflight and child runs retry `ctx.run` without a
            // cap: a child's failure is its recorded outcome, never a
            // dispatcher giving up.
            LashService::EffectGroupDispatch => builder.bind(EffectGroupDispatch::new(
                effect_host,
                ingress.clone(),
                RunRetryPolicy::new(),
                Arc::clone(&sessions),
            )),
        })
}
