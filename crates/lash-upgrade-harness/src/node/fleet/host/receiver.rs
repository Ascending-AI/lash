//! Register the persistent H2 mutation receiver inside an actual admitted handler.
use crate::node::RestateArgs;
use anyhow::{Result, anyhow};
use lash::restate::{RestateRuntimeEffectController, restate_sdk};
use restate_sdk::prelude::*;

#[allow(
    deprecated,
    reason = "runtime namespace binding requires the SDK trait dispatcher"
)]
mod service {
    use lash::restate::restate_sdk;
    use restate_sdk::prelude::*;
    #[restate_sdk::workflow]
    #[name = "FleetReceiver"]
    pub(crate) trait FleetReceiver {
        async fn run(
            session: Json<lash::SessionId>,
        ) -> HandlerResult<Json<lash::process::ProcessStartReceipt>>;
    }
}
use service::{FleetReceiver, ServeFleetReceiver};

struct Receiver {
    core: lash::LashCore,
    authority: lash::restate::RestateAuthorityId,
    namespace: lash::restate::RestateNamespace,
}
impl FleetReceiver for Receiver {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(session): Json<lash::SessionId>,
    ) -> HandlerResult<Json<lash::process::ProcessStartReceipt>> {
        let controller = RestateRuntimeEffectController::new(
            ctx,
            self.authority.clone(),
            self.core.build_generation().clone(),
        )
        .in_namespace(self.namespace.clone());
        let scope =
            lash::runtime::AdmittedScope::runtime_operation(format!("h2-receiver:{session}"));
        let scoped = lash::runtime::ScopedEffectController::borrowed(&controller, scope)
            .map_err(TerminalError::from_error)?;
        let receipt =
            crate::node::tools::register_receiver(&self.core, &session, "tool_receipt", scoped)
                .await
                .map_err(|error| {
                    TerminalError::new(format!("register fleet receiver: {error:#}"))
                })?;
        Ok(Json(receipt))
    }
}

pub(super) fn bind(
    builder: restate_sdk::endpoint::Builder,
    args: &RestateArgs,
    core: lash::LashCore,
) -> Result<restate_sdk::endpoint::Builder> {
    use restate_sdk::service::Discoverable as _;
    let namespace = lash::restate::RestateNamespace::new(&args.namespace)?;
    let name = namespace.service_name("FleetReceiver");
    let mut discovery = ServeFleetReceiver::<Receiver>::discover();
    discovery.name = restate_sdk::discovery::ServiceName::try_from(name.clone())
        .map_err(|error| anyhow!("receiver name {name}: {error}"))?;
    Ok(
        builder.bind(restate_sdk::service::macro_support::service_definition(
            Receiver {
                core,
                authority: lash::restate::RestateAuthorityId::new(&args.authority)?,
                namespace,
            }
            .serve(),
            discovery,
        )),
    )
}

pub(super) async fn register(
    args: &RestateArgs,
    session: &lash::SessionId,
) -> Result<lash::process::ProcessStartReceipt> {
    let name = lash::restate::RestateNamespace::new(&args.namespace)?.service_name("FleetReceiver");
    let client = lash::restate::RestateIngressClient::new(lash::restate::RestateConnection::new(
        args.ingress_url.clone(),
    ));
    client
        .call_workflow_json(&name, session.as_str(), "run", session)
        .await
        .map_err(|error| anyhow!("receiver registration: {error}"))
}
