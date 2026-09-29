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
//! # Namespaces (FIG-3898)
//!
//! Restate names services per server, and a new registration of a name
//! takes over every call to it. A deployment therefore names its services
//! under a [`RestateNamespace`]: the empty namespace keeps the bare names
//! (`LashSession`), and a namespace `ns` prefixes every name lash serves and
//! calls (`ns.LashSession`, `ns.LashProcessWorkflow_g<G>`), so several lash
//! deployments share one server without taking over each other's calls.
//! Every name below is its namespace's: a [`ServiceRoute`] carries the
//! namespace it was built in, and the handler-side clients
//! ([`DurableWaitRegistryCalls`] and its siblings) address the namespace
//! their deployment serves. Each binding carries the deployment's claim in
//! its discovery metadata ([`CLAIM_AUTHORITY_METADATA`]), which
//! [`RestateEngine::register_deployment`](crate::RestateEngine::register_deployment)
//! reads to refuse a registration that would take over another deployment's
//! names.
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
use restate_sdk::context::{ContextClient, Request, RequestTarget, RunRetryPolicy};
use restate_sdk::endpoint::{Builder, HandlerOptions, ServiceOptions};
use restate_sdk::serde::Json;
use restate_sdk::service::macro_support::{ServiceBoxFuture, service_definition};
use restate_sdk::service::{Discoverable, Service};

use crate::RestateEffectHost;
use crate::durable_wait::{
    LashDurableWaitRegistry as _, LashDurableWaitRegistryImpl, LashDurableWaitWorkflow as _,
    LashDurableWaitWorkflowImpl,
};
use crate::effect_group::{
    EffectGroupDispatch as _, EffectGroupDispatchImpl, EffectGroupPayload as _,
    EffectGroupPayloadImpl, EffectGroupState as _, EffectGroupStateImpl,
};
use crate::ingress::RestateIngressClient;
use crate::process::{LashProcessWorkflow as _, LashProcessWorkflowImpl, RestateProcessRunner};
use crate::process_attach::{LashProcessAttach as _, LashProcessAttachImpl};
use crate::session_driver::{
    LashSession as _, LashSessionImpl, LashTurn as _, LashTurnImpl, RestateSessionDriverSlot,
};

/// The longest namespace a deployment may take, in bytes.
const NAMESPACE_MAX_LEN: usize = 63;

/// The discovery-metadata key under which every lash service a deployment
/// binds names the Restate authority that deployment journals under: the
/// deployment's claim on its namespace's names (FIG-3898).
pub(crate) const CLAIM_AUTHORITY_METADATA: &str = "lash.authority";

/// The namespace a lash deployment's Restate services are named under
/// (FIG-3898).
///
/// Restate keeps one set of service names per server, and the newest
/// registration of a name serves every new call to it, so two deployments
/// that serve the same names on one server take over each other's work. A
/// deployment's namespace keeps its names its own: under the namespace `ns`,
/// every service lash serves and every name lash calls, reads or filters on
/// is `ns.<name>` (`ns.LashSession`, `ns.LashProcessWorkflow_g<G>`).
///
/// The [`default`](Self::default) namespace is empty and keeps the bare
/// names (`LashSession`): a deployment that is alone on its server needs no
/// namespace. Deployments that share a server each take a distinct one.
///
/// A namespace is 1 to 63 bytes of lowercase ASCII letters, digits and `-`,
/// starting with a letter and not with `restate` (Restate reserves those
/// names). `.` separates the namespace from the name, and `_` is a wildcard
/// in the admin API's SQL `LIKE`, so neither may appear in one.
///
/// Every durable name a deployment records (a workflow key, a route in an
/// effect-group index or a segment handover) is addressed under its
/// namespace, so changing a deployment's namespace starts a new deployment:
/// its predecessor's work stays under the old names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct RestateNamespace(Option<Arc<str>>);

/// Why a string is not a [`RestateNamespace`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RestateNamespaceError {
    #[error("Restate namespace `{namespace}` is longer than {NAMESPACE_MAX_LEN} bytes")]
    TooLong { namespace: String },
    #[error("Restate namespace `{namespace}` must start with a lowercase ASCII letter")]
    FirstCharacter { namespace: String },
    #[error(
        "Restate namespace `{namespace}` holds `{character}`: a namespace is lowercase ASCII \
         letters, digits and `-`"
    )]
    Character { namespace: String, character: char },
    #[error("Restate namespace `{namespace}` starts with `restate`, which Restate reserves")]
    Reserved { namespace: String },
}

impl RestateNamespace {
    /// `value` as a namespace; the empty string is the
    /// [`default`](Self::default) namespace.
    pub fn new(value: impl AsRef<str>) -> Result<Self, RestateNamespaceError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Ok(Self::default());
        }
        let refuse = |make: fn(String) -> RestateNamespaceError| Err(make(value.to_owned()));
        if value.len() > NAMESPACE_MAX_LEN {
            return refuse(|namespace| RestateNamespaceError::TooLong { namespace });
        }
        if !value.starts_with(|c: char| c.is_ascii_lowercase()) {
            return refuse(|namespace| RestateNamespaceError::FirstCharacter { namespace });
        }
        if let Some(character) = value
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(RestateNamespaceError::Character {
                namespace: value.to_owned(),
                character,
            });
        }
        if value.starts_with("restate") {
            return refuse(|namespace| RestateNamespaceError::Reserved { namespace });
        }
        Ok(Self(Some(Arc::from(value))))
    }

    /// The namespace as written; empty for the default namespace.
    pub fn as_str(&self) -> &str {
        self.0.as_deref().unwrap_or("")
    }

    /// Whether this is the default namespace, which keeps the bare names.
    pub fn is_default(&self) -> bool {
        self.0.is_none()
    }

    /// The Restate name `name` takes under this namespace: `name` itself in
    /// the default namespace, `<namespace>.<name>` otherwise. A host names
    /// its own services, and the lash names it reads (an admin query on
    /// `LashTurn`), through it.
    pub fn service_name(&self, name: &str) -> String {
        self.qualify(name).into_owned()
    }

    fn qualify<'a>(&self, name: &'a str) -> Cow<'a, str> {
        match &self.0 {
            None => Cow::Borrowed(name),
            Some(namespace) => Cow::Owned(format!("{namespace}.{name}")),
        }
    }

    /// `name` with this namespace's prefix removed: `None` for a name
    /// outside the namespace.
    fn strip<'a>(&self, name: &'a str) -> Option<&'a str> {
        match &self.0 {
            None => Some(name),
            Some(namespace) => name.strip_prefix(namespace.as_ref())?.strip_prefix('.'),
        }
    }

    /// `service` under its stable name in this namespace.
    pub(crate) fn stable(&self, service: LashService) -> ServiceRoute {
        ServiceRoute {
            namespace: self.clone(),
            service,
            lane: Lane::Stable,
        }
    }

    /// `service` under the lane builds of `generation` serve in this
    /// namespace. Only a [`LaneClass::Pinned`] service is bound under one;
    /// a shared service named this way names nothing any deployment serves.
    pub(crate) fn generation(
        &self,
        service: LashService,
        generation: BuildGeneration,
    ) -> ServiceRoute {
        ServiceRoute {
            namespace: self.clone(),
            service,
            lane: Lane::Generation(generation),
        }
    }

    /// `service` under `generation`'s lane when there is one, its stable
    /// name otherwise: where the work of a caller that may or may not know
    /// its build goes.
    pub(crate) fn own_or_stable(
        &self,
        service: LashService,
        generation: Option<&BuildGeneration>,
    ) -> ServiceRoute {
        generation.map_or_else(
            || self.stable(service),
            |generation| self.generation(service, generation.clone()),
        )
    }

    /// Reads back a route recorded in this namespace: `None` for a name that
    /// is no lash service of this namespace under any lane.
    pub(crate) fn parse(&self, name: &str) -> Option<ServiceRoute> {
        let name = self.strip(name)?;
        LASH_SERVICES.iter().find_map(|&service| {
            let rest = name.strip_prefix(service.base_name())?;
            if rest.is_empty() {
                return Some(self.stable(service));
            }
            let generation = BuildGeneration::parse(rest.strip_prefix("_g")?).ok()?;
            (service.lane_class() == LaneClass::Pinned)
                .then(|| self.generation(service, generation))
        })
    }

    /// The admin API's `sys_invocation` filter matching every lane of
    /// `service` in this namespace: its stable name, or that name followed
    /// by a generation suffix `_g<G>` (FIG-3795). Restate's SQL reads `_` in
    /// a `LIKE` pattern as any one character; a namespace holds no `_`, so
    /// the widening stays inside this namespace's own names.
    pub(crate) fn service_lanes_sql(&self, service: LashService) -> String {
        crate::ingress::service_lanes_sql(&self.stable(service).name())
    }
}

/// The default namespace with a `'static` borrow, for tests that address
/// lash's services under their bare names.
#[cfg(test)]
pub(crate) static DEFAULT_NAMESPACE: RestateNamespace = RestateNamespace(None);

impl std::fmt::Display for RestateNamespace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for RestateNamespace {
    type Err = RestateNamespaceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

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
            /// The service's stable name in the default namespace. A call
            /// never addresses it directly: [`RestateNamespace::stable`]
            /// and [`ServiceRoute::name`] name it in the deployment's
            /// namespace and lane.
            pub(crate) const fn base_name(self) -> &'static str {
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

/// A lash service under one of its lanes in one namespace: the full Restate
/// service name a call addresses, and the value a sender records as the
/// route it used. [`RestateNamespace`] builds and parses routes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ServiceRoute {
    namespace: RestateNamespace,
    service: LashService,
    lane: Lane,
}

impl ServiceRoute {
    pub(crate) fn service(&self) -> LashService {
        self.service
    }

    pub(crate) fn lane(&self) -> &Lane {
        &self.lane
    }

    /// The namespace the route names its service in.
    pub(crate) fn namespace(&self) -> &RestateNamespace {
        &self.namespace
    }

    /// The full Restate service name: the namespaced stable name, or that
    /// name plus the generation's `_g<hex>` suffix.
    pub(crate) fn name(&self) -> Cow<'static, str> {
        let stable = self.namespace.qualify(self.service.base_name());
        match &self.lane {
            Lane::Stable => stable,
            Lane::Generation(generation) => {
                Cow::Owned(format!("{stable}{}", generation.service_suffix()))
            }
        }
    }
}

impl std::fmt::Display for ServiceRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.name())
    }
}

/// Declares the handler-side client of each shared lash service: the typed
/// calls lash's handlers make to it, addressed in the caller's namespace.
///
/// The SDK's generated clients bake the service's default-namespace name
/// into every call, so lash's handlers call through these instead. Each
/// method is pinned to the SDK client of the same service at compile time:
/// the handler must exist there under the same name, with the same request
/// and response types.
macro_rules! lash_clients {
    ($(
        $(#[$doc:meta])*
        $client:ident, $ctor:ident: $service:ident $kind:ident, pinned to $($sdk:ident)::+ {
            $($handler:ident($($arg:ty)?) -> $res:ty;)+
        }
    )+) => {
        $(
            $(#[$doc])*
            pub(crate) struct $client<'a, C> {
                ctx: &'a C,
                name: String,
                key: String,
            }

            impl<'a, C> $client<'a, C> {
                lash_clients!(@target $kind);

                $(lash_clients!(@method $handler($($arg)?) -> $res);)+
            }

            impl RestateNamespace {
                #[doc = concat!("The [`", stringify!($client), "`] to the `key` instance of this namespace's service.")]
                pub(crate) fn $ctor<'a, C>(&self, ctx: &'a C, key: impl Into<String>) -> $client<'a, C> {
                    $client {
                        ctx,
                        name: self.stable(LashService::$service).name().into_owned(),
                        key: key.into(),
                    }
                }
            }

            const _: () = {
                type Sdk<'ctx> = $($sdk)::+<'ctx>;

                #[allow(dead_code, reason = "a compile-time pin, never called")]
                fn pinned() {
                    $(let _ = lash_clients!(@pin $handler($($arg)?) -> $res);)+
                }
            };
        )+
    };
    (@target object) => {
        fn target(&self, handler: &str) -> RequestTarget {
            RequestTarget::object(self.name.clone(), self.key.clone(), handler)
        }
    };
    (@target workflow) => {
        fn target(&self, handler: &str) -> RequestTarget {
            RequestTarget::workflow(self.name.clone(), self.key.clone(), handler)
        }
    };
    (@method $handler:ident($arg:ty) -> $res:ty) => {
        pub(crate) fn $handler<'ctx>(&self, request: $arg) -> Request<'ctx, $arg, $res>
        where
            C: ContextClient<'ctx>,
        {
            self.ctx.request(self.target(stringify!($handler)), request)
        }
    };
    (@method $handler:ident() -> $res:ty) => {
        pub(crate) fn $handler<'ctx>(&self) -> Request<'ctx, (), $res>
        where
            C: ContextClient<'ctx>,
        {
            self.ctx.request(self.target(stringify!($handler)), ())
        }
    };
    (@pin $handler:ident($arg:ty) -> $res:ty) => {
        <Sdk<'static>>::$handler as fn(&Sdk<'static>, $arg) -> Request<'static, $arg, $res>
    };
    (@pin $handler:ident() -> $res:ty) => {
        <Sdk<'static>>::$handler as fn(&Sdk<'static>) -> Request<'static, (), $res>
    };
}

lash_clients! {
    /// Calls to one `LashDurableWaitIndex` object.
    DurableWaitRegistryCalls, durable_wait_registry: DurableWaitRegistry object,
    pinned to crate::durable_wait::LashDurableWaitRegistryClient {
        is_revoked(Json<()>) -> Json<bool>;
        peek_turn_gate(Json<crate::durable_wait::RestateDurableWaitIndexRequest>)
            -> Json<crate::durable_wait::RestateTurnGatePeek>;
        register(Json<crate::durable_wait::RestateDurableWaitIndexRequest>)
            -> Json<crate::durable_wait::RestateDurableWaitRegistration>;
        settle(Json<crate::durable_wait::RestateDurableWaitSettleRequest>) -> Json<()>;
        register_awakeable(Json<crate::durable_wait::RestateDurableWaitAwakeableRequest>)
            -> Json<crate::durable_wait::RestateDurableWaitRegistration>;
        unregister_awakeable(Json<crate::durable_wait::RestateDurableWaitAwakeableRequest>)
            -> Json<()>;
        resolve(Json<crate::durable_wait::RestateDurableWaitResolveRequest>)
            -> Json<crate::durable_wait::RestateDurableWaitResolveResponse>;
        fence_cancel_decided(Json<crate::durable_wait::RestateDurableWaitCancelDecidedRequest>)
            -> Json<()>;
        retain_resolution(Json<crate::durable_wait::RestateDurableWaitResolveRequest>)
            -> Json<()>;
        cancel_all() -> Json<()>;
        revoke_all() -> Json<()>;
        begin_effect(Json<crate::durable_wait::RestateDurableWaitEffectRequest>) -> Json<bool>;
        end_effect(Json<crate::durable_wait::RestateDurableWaitEffectRequest>) -> Json<()>;
        record_group(Json<crate::durable_wait::RestateDurableWaitGroupRequest>) -> Json<bool>;
        record_group_child(Json<crate::durable_wait::RestateDurableWaitGroupChildRequest>)
            -> Json<bool>;
        group_child_membership(
            Json<crate::durable_wait::RestateDurableWaitGroupChildMembershipRequest>
        ) -> Json<Option<String>>;
    }

    /// Calls to one `LashDurableWaitWorkflow`.
    DurableWaitWorkflowCalls, durable_wait_workflow: DurableWaitWorkflow workflow,
    pinned to crate::durable_wait::LashDurableWaitWorkflowClient {
        await_resolution(Json<crate::durable_wait::RestateDurableWaitAwaitInput>)
            -> Json<lash_core::Resolution>;
        peek() -> Json<Option<lash_core::Resolution>>;
        resolve(Json<crate::durable_wait::RestateDurableWaitResolveRequest>)
            -> Json<lash_core::ResolveOutcome>;
    }

    /// Calls to one `LashProcessAttach` workflow.
    ProcessAttachCalls, process_attach: ProcessAttach workflow,
    pinned to crate::process_attach::LashProcessAttachClient {
        run(Json<crate::process_attach::RestateProcessAttachRequest>) -> Json<()>;
    }

    /// Calls to one `EffectGroupIndex` object.
    EffectGroupStateCalls, effect_group_state: EffectGroupState object,
    pinned to crate::effect_group::EffectGroupStateClient {
        probe() -> Json<crate::effect_group::EffectGroupProbeResponse>;
        unsettled_children() -> Json<usize>;
        open(Json<crate::effect_group::EffectGroupOpenRequest>)
            -> Json<crate::effect_group::EffectGroupOpenResponse>;
        probe_and_adopt(Json<crate::effect_group::EffectGroupAdoptRequest>)
            -> Json<crate::effect_group::EffectGroupProbeAdoptResponse>;
        record_dispatch(Json<crate::effect_group::EffectGroupRecordDispatchRequest>)
            -> Json<crate::effect_group::EffectGroupRecordDispatchResponse>;
        register_children(Json<crate::effect_group::EffectGroupRegisterRequest>)
            -> Json<crate::effect_group::EffectGroupRegisterResponse>;
        register_refusal(Json<crate::effect_group::EffectGroupRefusalRequest>)
            -> Json<crate::effect_group::EffectGroupRegisterRefusalResponse>;
        admit_child(Json<crate::effect_group::EffectGroupAdmissionRequest>)
            -> Json<crate::effect_group::EffectGroupAdmissionResponse>;
        commit_child(Json<crate::effect_group::EffectGroupCommitChildRequest>)
            -> Json<crate::effect_group::EffectGroupCommitChildResponse>;
        admit_semantic(Json<crate::effect_group::EffectGroupAdmitSemanticRequest>)
            -> Json<crate::effect_group::EffectGroupAdmitSemanticResponse>;
        drain_blockers(Json<crate::effect_group::EffectGroupDrainBlockersRequest>)
            -> Json<crate::effect_group::EffectGroupDrainBlockersResponse>;
        record_settlement(Json<crate::effect_group::EffectGroupRecordSettlementRequest>)
            -> Json<crate::effect_group::EffectGroupRecordSettlementResponse>;
        read_rank(Json<crate::effect_group::EffectGroupReadRankRequest>)
            -> Json<crate::effect_group::EffectGroupReadRankResponse>;
        close(Json<crate::effect_group::EffectGroupCloseRequest>)
            -> Json<crate::effect_group::EffectGroupCloseResponse>;
        retire() -> Json<crate::effect_group::EffectGroupRetireResponse>;
        finish_retirement() -> Json<crate::effect_group::EffectGroupFinishRetirementResponse>;
        retirement_cancel() -> Json<crate::effect_group::EffectGroupRetirementCancelResponse>;
    }

    /// Calls to one `EffectGroupPayload` object.
    EffectGroupPayloadCalls, effect_group_payload: EffectGroupPayload object,
    pinned to crate::effect_group::EffectGroupPayloadClient {
        put(Json<crate::effect_group::EffectGroupPayloadPutRequest>)
            -> Json<crate::effect_group::EffectGroupPayloadPutResponse>;
        get() -> Json<crate::effect_group::EffectGroupPayloadGetResponse>;
        retire() -> Json<()>;
        delete_bytes() -> Json<()>;
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
/// A lane name is an optional namespace and `.`, a lash service name, and
/// optionally `_g` and 12 lowercase hex digits, which the SDK's grammar
/// accepts for every service in every namespace [`RestateNamespace::new`]
/// admits (`tests::bindings` pins it). A name it refused would leave that
/// lane unbound — reported, never a panic in the endpoint's construction —
/// and the discovery test fails on the missing lane.
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

/// The lanes `service` is bound under by a build of `generation` in
/// `namespace`: every service under its stable name, and a pinned one under
/// the build's generation name too.
fn lanes(
    namespace: &RestateNamespace,
    service: LashService,
    generation: &BuildGeneration,
) -> Vec<ServiceRoute> {
    match service.lane_class() {
        LaneClass::Shared => vec![namespace.stable(service)],
        LaneClass::Pinned => vec![
            namespace.stable(service),
            namespace.generation(service, generation.clone()),
        ],
    }
}

/// Every Restate name a build of `generation` serves for lash in
/// `namespace`: each shared service once, each pinned service under both
/// lanes.
#[cfg(test)]
pub(crate) fn lash_service_routes(
    namespace: &RestateNamespace,
    generation: &BuildGeneration,
) -> Vec<ServiceRoute> {
    LASH_SERVICES
        .iter()
        .flat_map(|&service| lanes(namespace, service, generation))
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
    pub(crate) sessions: Arc<dyn lash_core::DeploymentStore>,
    /// The process workflow over the deployment's process worker.
    pub(crate) process_workflow: LashProcessWorkflowImpl<R>,
    /// Where the session handlers find the driver the core installs.
    pub(crate) session_driver: RestateSessionDriverSlot,
    /// The deployment's drain generation: every journal-bearing handler
    /// records it as its journal's first command, and each pinned service is
    /// bound under its lane.
    pub(crate) build_generation: BuildGeneration,
    /// The namespace every service is bound, and every call addressed, in.
    pub(crate) namespace: RestateNamespace,
}

/// Bind every [`LashService`] on `builder` under its name in the
/// deployment's namespace, each pinned service under both of its lanes, and
/// each carrying the deployment's claim ([`CLAIM_AUTHORITY_METADATA`]).
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
        namespace,
    } = parts;
    let claimed = || {
        ServiceOptions::new().metadata(
            CLAIM_AUTHORITY_METADATA,
            effect_host.authority_id().binding_id(),
        )
    };
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
        &namespace,
    );
    let turn = LashTurnImpl::new(
        session_driver,
        effect_host.authority_id().clone(),
        build_generation.clone(),
        &namespace,
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
        .flat_map(|&service| lanes(&namespace, service, &build_generation))
        .fold(builder, |builder, route| {
            let name = route.name();
            match route.service() {
                LashService::DurableWaitWorkflow => bind_as(
                    builder,
                    LashDurableWaitWorkflowImpl::new(namespace.clone()).serve(),
                    &name,
                    claimed(),
                ),
                LashService::DurableWaitRegistry => bind_as(
                    builder,
                    LashDurableWaitRegistryImpl::new(namespace.clone()).serve(),
                    &name,
                    claimed().enable_lazy_state(true),
                ),
                LashService::ProcessAttach => bind_as(
                    builder,
                    LashProcessAttachImpl::new(namespace.clone()).serve(),
                    &name,
                    claimed(),
                ),
                LashService::EffectGroupState => bind_as(
                    builder,
                    EffectGroupStateImpl::new(namespace.clone()).serve(),
                    &name,
                    claimed(),
                ),
                LashService::EffectGroupPayload => {
                    bind_as(builder, EffectGroupPayloadImpl.serve(), &name, claimed())
                }
                LashService::ProcessWorkflow => bind_as(
                    builder,
                    process_workflow.on_route(route.clone()).serve(),
                    &name,
                    claimed().handler("run", run_options.clone()),
                ),
                LashService::EffectGroupDispatch => {
                    bind_as(builder, dispatch(route.clone()).serve(), &name, claimed())
                }
                LashService::SessionDriver => bind_as(
                    builder,
                    session.on_route(route.clone()).serve(),
                    &name,
                    claimed().handler("drive", crate::turn_handler_options()),
                ),
                LashService::TurnDriver => bind_as(
                    builder,
                    turn.on_route(route.clone()).serve(),
                    &name,
                    claimed().handler("run", crate::turn_handler_options()),
                ),
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn namespaces() -> [RestateNamespace; 3] {
        [
            RestateNamespace::default(),
            RestateNamespace::new("toolbench-r1").expect("a valid namespace"),
            RestateNamespace::new("a").expect("a valid namespace"),
        ]
    }

    #[test]
    fn a_route_reads_back_as_the_route_it_names() {
        let generation = BuildGeneration::for_test("routes");
        for namespace in namespaces() {
            for &service in LASH_SERVICES {
                for route in lanes(&namespace, service, &generation) {
                    assert_eq!(
                        namespace.parse(&route.name()),
                        Some(route.clone()),
                        "{route}"
                    );
                }
            }
        }
        assert_eq!(
            RestateNamespace::default()
                .generation(LashService::ProcessWorkflow, generation.clone())
                .name()
                .as_ref(),
            format!("LashProcessWorkflow_g{generation}")
        );
        assert_eq!(
            RestateNamespace::new("tb")
                .expect("a valid namespace")
                .generation(LashService::ProcessWorkflow, generation.clone())
                .name()
                .as_ref(),
            format!("tb.LashProcessWorkflow_g{generation}")
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
            // Another namespace's names are not the default namespace's.
            "tb.LashSession".to_owned(),
        ] {
            assert_eq!(RestateNamespace::default().parse(&name), None, "{name}");
        }
    }

    /// Two deployments on one server name nothing alike: no name of one
    /// namespace reads back as a route of another (FIG-3898).
    #[test]
    fn no_namespace_reads_another_namespaces_names() {
        let generation = BuildGeneration::for_test("routes");
        let namespaces = [
            RestateNamespace::default(),
            RestateNamespace::new("a").expect("a valid namespace"),
            RestateNamespace::new("ab").expect("a valid namespace"),
            RestateNamespace::new("a-b").expect("a valid namespace"),
        ];
        for owner in &namespaces {
            for &service in LASH_SERVICES {
                for route in lanes(owner, service, &generation) {
                    for reader in namespaces.iter().filter(|reader| *reader != owner) {
                        assert_eq!(
                            reader.parse(&route.name()),
                            None,
                            "`{reader}` read `{route}`"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_namespace_is_a_restate_name_prefix_or_refused_typed() {
        for valid in ["a", "toolbench-r1", "x9", &"n".repeat(63)] {
            let namespace = RestateNamespace::new(valid).expect(valid);
            assert_eq!(namespace.as_str(), valid);
            for route in lash_service_routes(&namespace, &BuildGeneration::for_test("names")) {
                restate_sdk::discovery::ServiceName::try_from(route.name().into_owned())
                    .unwrap_or_else(|error| panic!("{route} is no Restate name: {error}"));
            }
        }
        assert!(RestateNamespace::new("").expect("empty").is_default());
        for (invalid, refusal) in [
            (
                "n".repeat(64),
                RestateNamespaceError::TooLong {
                    namespace: "n".repeat(64),
                },
            ),
            (
                "1st".to_owned(),
                RestateNamespaceError::FirstCharacter {
                    namespace: "1st".to_owned(),
                },
            ),
            (
                "Upper".to_owned(),
                RestateNamespaceError::FirstCharacter {
                    namespace: "Upper".to_owned(),
                },
            ),
            (
                "a.b".to_owned(),
                RestateNamespaceError::Character {
                    namespace: "a.b".to_owned(),
                    character: '.',
                },
            ),
            (
                "a_b".to_owned(),
                RestateNamespaceError::Character {
                    namespace: "a_b".to_owned(),
                    character: '_',
                },
            ),
            (
                "restate-x".to_owned(),
                RestateNamespaceError::Reserved {
                    namespace: "restate-x".to_owned(),
                },
            ),
        ] {
            assert_eq!(RestateNamespace::new(&invalid), Err(refusal), "{invalid}");
        }
    }

    #[test]
    fn only_journal_bearing_services_are_pinned() {
        let pinned: Vec<_> = LASH_SERVICES
            .iter()
            .filter(|service| service.lane_class() == LaneClass::Pinned)
            .map(|service| service.base_name())
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
