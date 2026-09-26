#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API used by Lash durable waits"
)]

//! The Restate services lash itself serves, the names they are served under,
//! and the one place that binds them.
//!
//! [`LashService`] names every service lash-restate addresses: the ingress
//! calls, the handler-side clients, and the error messages that name a
//! service nothing binds all spell it through this enum.
//! [`bind_lash_services`] is the only code that binds them, and it binds
//! every variant, so a deployment serves all of lash's services or none of
//! them. A host reaches it through
//! [`RestateEngine::endpoint_builder`](crate::RestateEngine::endpoint_builder)
//! and binds only its own services on the builder it gets back.
//!
//! # Routing lanes (FIG-3795)
//!
//! Restate starts a new invocation on the newest deployment that serves its
//! service name, and keeps a started invocation pinned to the deployment it
//! started on. Every service is therefore one of two [`LaneClass`]es:
//!
//! - A **pinned** service bears a journal that only its own build may
//!   replay: the process segment workflow, the effect-group dispatcher, and
//!   the session driver's `LashSession` and `LashTurn`. Each build binds it
//!   twice: under its **stable** name (`LashProcessWorkflow`), which Restate
//!   hands to the newest build, and under its **generation** name
//!   (`LashProcessWorkflow_g<G>`, [`Lane::Generation`]), which only builds of
//!   drain generation `G` serve. Work that must reach the build that started
//!   it — an effect group's children, a redrive, a successor the newest
//!   build refused — is sent to the generation name.
//! - A **shared** service holds state every build reads and writes: the
//!   durable-wait workflow and index, process attach, and the effect-group
//!   index and payload objects. Its name is never split by generation, so a
//!   waiter on one build and a resolver on another address the same promise.
//!
//! A route is data. Whoever sends work to a pinned service records the
//! [`ServiceRoute`] it sent under beside the thing it routes (the segment
//! handover, the effect-group index record), and every later call reads the
//! recorded route back ([`ServiceRoute::parse`]) instead of recomputing a
//! name from its own build: Restate scopes workflow keys and idempotency keys
//! by service name, so a recomputed name would start the work a second time.
//!
//! A host's own services are named once, under their stable names. A host
//! submits work to the engine and never drives it (ADR 0104): no host handler
//! runs a lash turn, so no host service carries a journal that needs a
//! generation lane. A controller a host builds inside its own handler names
//! no build generation, so an effect group it opens dispatches on the stable
//! `EffectGroupDispatch` lane.

use std::borrow::Cow;
use std::sync::Arc;

use lash_core::engine::BuildGeneration;
use restate_sdk::context::RunRetryPolicy;
use restate_sdk::endpoint::{Builder, HandlerOptions, ServiceOptions};
use restate_sdk::service::macro_support::{ServiceBoxFuture, service_definition};
use restate_sdk::service::{Discoverable, Service};

use crate::RestateEffectHost;
use crate::durable_wait::{
    LashDurableWaitRegistry as _, LashDurableWaitRegistryImpl, LashDurableWaitWorkflow as _,
    LashDurableWaitWorkflowImpl,
};
use crate::effect_group::{
    EffectGroupDispatch as _, EffectGroupDispatchImpl, EffectGroupPayload, EffectGroupState,
};
use crate::ingress::RestateIngressClient;
use crate::process::{LashProcessWorkflow as _, LashProcessWorkflowImpl, RestateProcessRunner};
use crate::process_attach::{LashProcessAttach as _, LashProcessAttachImpl};
use crate::session_driver::{
    LashSession as _, LashSessionImpl, LashTurn as _, LashTurnImpl, RestateSessionDriverSlot,
};

/// Whether a lash service is split by build generation (FIG-3795).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaneClass {
    /// Journal-bearing: bound under its stable name and under each build's
    /// generation name.
    Pinned,
    /// State-holding: bound under its stable name only.
    Shared,
}

/// Declares [`LashService`] and its complete [`LASH_SERVICES`] together, so
/// a variant cannot exist without being listed, named and classed.
macro_rules! lash_services {
    ($($(#[$doc:meta])* $variant:ident => $name:literal, $class:ident;)+) => {
        /// A Restate service lash itself serves.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub(crate) enum LashService {
            $($(#[$doc])* $variant,)+
        }

        /// Every service lash serves, each once.
        pub(crate) const LASH_SERVICES: &[LashService] = &[$(LashService::$variant,)+];

        impl LashService {
            /// The service's stable Restate name: what a call to its stable
            /// lane addresses and what the endpoint's discovery document
            /// reports for that lane. A generation lane is
            /// [`ServiceRoute::name`]'s.
            pub(crate) const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)+
                }
            }

            /// Whether the service is split by build generation.
            pub(crate) const fn lane_class(self) -> LaneClass {
                match self {
                    $(Self::$variant => LaneClass::$class,)+
                }
            }
        }
    };
}

lash_services! {
    /// Exact-address promises and deadline timers for every await-event key.
    DurableWaitWorkflow => "LashDurableWaitWorkflow", Shared;
    /// The per-scope registry that cancels, revokes and fences a scope's waits.
    DurableWaitRegistry => "LashDurableWaitIndex", Shared;
    /// The segment runner a process submission starts and awaits.
    ProcessWorkflow => "LashProcessWorkflow", Pinned;
    /// Arms a process terminal for a caller parked on it.
    ProcessAttach => "LashProcessAttach", Shared;
    /// An effect group's lifecycle and settlement rank.
    EffectGroupState => "EffectGroupIndex", Shared;
    /// An effect group's successful result bytes.
    EffectGroupPayload => "EffectGroupPayload", Shared;
    /// Sends an effect group's children and runs each one.
    EffectGroupDispatch => "EffectGroupDispatch", Pinned;
    /// One session's drive: admits roots and runs each in its `LashTurn`
    /// (FIG-3600).
    SessionDriver => "LashSession", Pinned;
    /// One admitted root: its seal, turns and commits (FIG-3600).
    TurnDriver => "LashTurn", Pinned;
}

/// Which of a pinned service's names a call addresses (FIG-3795).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lane {
    /// The stable name: Restate hands a new invocation to the newest build.
    Stable,
    /// `<name>_g<G>`: only builds of drain generation `G` serve it.
    Generation(BuildGeneration),
}

/// A lash service under one of its lanes: the full Restate service name a
/// call addresses, and the value a sender records as the route it used.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ServiceRoute {
    service: LashService,
    lane: Lane,
}

impl ServiceRoute {
    /// `service` under its stable name.
    pub(crate) const fn stable(service: LashService) -> Self {
        Self {
            service,
            lane: Lane::Stable,
        }
    }

    /// `service` under the lane builds of `generation` serve. Only a
    /// [`LaneClass::Pinned`] service is bound under one; a shared service
    /// named this way names nothing any deployment serves.
    pub(crate) fn generation(service: LashService, generation: BuildGeneration) -> Self {
        Self {
            service,
            lane: Lane::Generation(generation),
        }
    }

    /// `service` under `generation`'s lane when there is one, its stable
    /// name otherwise: where the work of a caller that may or may not know
    /// its build goes.
    pub(crate) fn own_or_stable(
        service: LashService,
        generation: Option<&BuildGeneration>,
    ) -> Self {
        generation.map_or_else(
            || Self::stable(service),
            |generation| Self::generation(service, generation.clone()),
        )
    }

    pub(crate) fn service(&self) -> LashService {
        self.service
    }

    pub(crate) fn lane(&self) -> &Lane {
        &self.lane
    }

    /// The full Restate service name: the stable name, or the stable name
    /// plus the generation's `_g<hex>` suffix.
    pub(crate) fn name(&self) -> Cow<'static, str> {
        match &self.lane {
            Lane::Stable => Cow::Borrowed(self.service.name()),
            Lane::Generation(generation) => Cow::Owned(format!(
                "{}{}",
                self.service.name(),
                generation.service_suffix()
            )),
        }
    }

    /// Reads back a recorded route: `None` for a name that is no lash
    /// service under any lane.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        LASH_SERVICES.iter().find_map(|&service| {
            let rest = name.strip_prefix(service.name())?;
            if rest.is_empty() {
                return Some(Self::stable(service));
            }
            let generation = BuildGeneration::parse(rest.strip_prefix("_g")?).ok()?;
            (service.lane_class() == LaneClass::Pinned)
                .then(|| Self::generation(service, generation))
        })
    }
}

impl std::fmt::Display for ServiceRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.name())
    }
}

/// A handler-side call to `handler` of the workflow `key` under `route`:
/// the one way lash's handlers address a pinned service, so the name a
/// call addresses is always a route, never the name a typed client bakes in.
pub(crate) fn routed_workflow<'ctx, C, Req, Res>(
    ctx: &C,
    route: &ServiceRoute,
    key: impl Into<String>,
    handler: &str,
    request: Req,
) -> restate_sdk::context::Request<'ctx, restate_sdk::serde::Json<Req>, restate_sdk::serde::Json<Res>>
where
    C: restate_sdk::context::ContextClient<'ctx>,
{
    ctx.request(
        restate_sdk::context::RequestTarget::workflow(route.name().into_owned(), key, handler),
        restate_sdk::serde::Json(request),
    )
}

/// A handler-side call to `handler` of the virtual object `key` under
/// `route`: [`routed_workflow`]'s counterpart for a pinned object lane.
pub(crate) fn routed_object<'ctx, C, Req, Res>(
    ctx: &C,
    route: &ServiceRoute,
    key: impl Into<String>,
    handler: &str,
    request: Req,
) -> restate_sdk::context::Request<'ctx, restate_sdk::serde::Json<Req>, restate_sdk::serde::Json<Res>>
where
    C: restate_sdk::context::ContextClient<'ctx>,
{
    ctx.request(
        restate_sdk::context::RequestTarget::object(route.name().into_owned(), key, handler),
        restate_sdk::serde::Json(request),
    )
}

/// Binds `dispatcher` on `builder` under `name` rather than the name its
/// macro declared, with `options` applied.
///
/// The SDK keeps `ServiceOptions::name` crate-private, so a service is
/// renamed through its discovery manifest: the generated dispatcher matches
/// on the handler name alone, and the endpoint routes by the name it was
/// bound under, so the renamed binding serves every handler of the original.
/// `service_definition` is the public (documentation-hidden) constructor the
/// service macros emit for their own `IntoServiceDefinition` impls.
///
/// A lane name is a lash service name, optionally followed by `_g` and 12
/// lowercase hex digits, which the SDK's grammar accepts for every service
/// (`tests::bindings` pins it). A name it refused would leave that lane
/// unbound — reported, never a panic in the endpoint's construction — and
/// the discovery test fails on the missing lane.
fn bind_as<S>(builder: Builder, dispatcher: S, name: &str, options: ServiceOptions) -> Builder
where
    S: Service<Future = ServiceBoxFuture> + Discoverable + Send + Sync + 'static,
{
    let mut discovery = S::discover();
    match restate_sdk::discovery::ServiceName::try_from(name.to_owned()) {
        Ok(renamed) => {
            discovery.name = renamed;
            builder.bind(service_definition(dispatcher, discovery).options(options))
        }
        Err(error) => {
            tracing::error!(
                service = name,
                error = %error,
                "a lash service lane name is not a valid Restate service name; the lane is not bound"
            );
            builder
        }
    }
}

/// The lanes `service` is bound under by a build of `generation`: every
/// service under its stable name, and a pinned one under the build's
/// generation name too.
fn lanes(service: LashService, generation: &BuildGeneration) -> Vec<ServiceRoute> {
    match service.lane_class() {
        LaneClass::Shared => vec![ServiceRoute::stable(service)],
        LaneClass::Pinned => vec![
            ServiceRoute::stable(service),
            ServiceRoute::generation(service, generation.clone()),
        ],
    }
}

/// Every Restate name a build of `generation` serves for lash: each shared
/// service once, each pinned service under both lanes.
#[cfg(test)]
pub(crate) fn lash_service_routes(generation: &BuildGeneration) -> Vec<ServiceRoute> {
    LASH_SERVICES
        .iter()
        .flat_map(|&service| lanes(service, generation))
        .collect()
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
    /// Where the session handlers find the driver the core installs.
    pub(crate) session_driver: RestateSessionDriverSlot,
    /// The deployment's drain generation: every journal-bearing handler
    /// records it as its journal's first command, and each pinned service is
    /// bound under its lane.
    pub(crate) build_generation: BuildGeneration,
}

/// Bind every [`LashService`] on `builder`, each pinned service under both
/// of its lanes.
///
/// The match is exhaustive over [`LASH_SERVICES`] × lanes, so a new lash
/// service does not compile until it is bound here, and a pinned one is
/// bound once per lane from an instance that knows the lane it serves.
pub(crate) fn bind_lash_services<R: RestateProcessRunner>(
    builder: Builder,
    parts: LashServiceParts<'_, R>,
) -> Builder {
    let LashServiceParts {
        effect_host,
        ingress,
        sessions,
        process_workflow,
        session_driver,
        build_generation,
    } = parts;
    // A segment that keeps failing live stops after its deployment's attempt
    // bound and pauses, keeping its journal: the park reconcile parks its
    // process and a resume retries it (FIG-3675).
    let run_options = HandlerOptions::new()
        .retry_policy_max_attempts(process_workflow.retry_max_attempts())
        .retry_policy_pause_on_max_attempts();
    // Both session services run lash turns: a parked root fails its attempt
    // retryably, and the handler pauses after its attempt budget with its
    // journal kept.
    let session = LashSessionImpl::new(
        session_driver.clone(),
        effect_host.authority_id().clone(),
        build_generation.clone(),
    );
    let turn = LashTurnImpl::new(
        session_driver,
        effect_host.authority_id().clone(),
        build_generation.clone(),
    );
    // Dispatcher preflight and child runs retry `ctx.run` without a cap: a
    // child's failure is its recorded outcome, never a dispatcher giving up.
    let dispatch = |route: ServiceRoute| {
        EffectGroupDispatchImpl::new(
            effect_host,
            ingress.clone(),
            RunRetryPolicy::new(),
            Arc::clone(&sessions),
            route,
            build_generation.clone(),
        )
    };
    LASH_SERVICES
        .iter()
        .flat_map(|&service| lanes(service, &build_generation))
        .fold(builder, |builder, route| {
            let name = route.name();
            match route.service() {
                LashService::DurableWaitWorkflow => {
                    builder.bind(LashDurableWaitWorkflowImpl.serve())
                }
                LashService::DurableWaitRegistry => {
                    builder.bind(LashDurableWaitRegistryImpl.serve())
                }
                LashService::ProcessAttach => builder.bind(LashProcessAttachImpl.serve()),
                LashService::EffectGroupState => builder.bind(EffectGroupState),
                LashService::EffectGroupPayload => builder.bind(EffectGroupPayload),
                LashService::ProcessWorkflow => bind_as(
                    builder,
                    process_workflow.on_route(route.clone()).serve(),
                    &name,
                    ServiceOptions::new().handler("run", run_options.clone()),
                ),
                LashService::EffectGroupDispatch => bind_as(
                    builder,
                    dispatch(route.clone()).serve(),
                    &name,
                    ServiceOptions::new(),
                ),
                LashService::SessionDriver => bind_as(
                    builder,
                    session.clone().serve(),
                    &name,
                    ServiceOptions::new().handler("drive", crate::turn_handler_options()),
                ),
                LashService::TurnDriver => bind_as(
                    builder,
                    turn.clone().serve(),
                    &name,
                    ServiceOptions::new().handler("run", crate::turn_handler_options()),
                ),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_reads_back_as_the_route_it_names() {
        let generation = BuildGeneration::for_test("routes");
        for &service in LASH_SERVICES {
            for route in lanes(service, &generation) {
                assert_eq!(
                    ServiceRoute::parse(&route.name()),
                    Some(route.clone()),
                    "{route}"
                );
            }
        }
        assert_eq!(
            ServiceRoute::generation(LashService::ProcessWorkflow, generation.clone())
                .name()
                .as_ref(),
            format!("LashProcessWorkflow_g{generation}")
        );
    }

    #[test]
    fn a_name_no_lane_serves_reads_back_as_no_route() {
        let generation = BuildGeneration::for_test("routes");
        for name in [
            String::new(),
            "HostTurnWorkflow".to_owned(),
            "LashProcessWorkflowX".to_owned(),
            "LashProcessWorkflow_g".to_owned(),
            "LashProcessWorkflow_gXYZ".to_owned(),
            format!("LashProcessWorkflow_g{generation}0"),
            // A shared service is never split by generation.
            format!("LashDurableWaitWorkflow_g{generation}"),
            format!("EffectGroupIndex_g{generation}"),
        ] {
            assert_eq!(ServiceRoute::parse(&name), None, "{name}");
        }
    }

    #[test]
    fn only_journal_bearing_services_are_pinned() {
        let pinned: Vec<_> = LASH_SERVICES
            .iter()
            .filter(|service| service.lane_class() == LaneClass::Pinned)
            .map(|service| service.name())
            .collect();
        assert_eq!(
            pinned,
            [
                "LashProcessWorkflow",
                "EffectGroupDispatch",
                "LashSession",
                "LashTurn"
            ]
        );
    }
}
