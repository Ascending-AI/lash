//! The local-participation arm and the process-lifetime completion route of
//! the recorded-authority checks. Both exist only for the native effect host
//! (ADR 0102: every host journals), so these tests go with them; FIG-3585
//! deletes the arm, the route and this file together.

use std::sync::Arc;

use super::tests::{child_controller, request};
use super::*;
use crate::ExecutionScope;
use crate::runtime::ToolChildCompletionRouting;

/// The tool-child host the recorded-authority checks run against.
fn tool_children(host: &Arc<dyn EffectHost>) -> Arc<ToolChildHost> {
    ToolChildHost::new(
        host,
        Arc::new(crate::InMemoryProcessExecutionEnvStore::default()),
    )
}

/// A controller that reports `EffectJournaling::Journaled` and
/// names the host's await-event authority, so the recorded-authority checks
/// see the durable arms rather than the native local ones.
struct DurableReplayController {
    authority_id: std::sync::OnceLock<String>,
}

impl crate::AwaitEventResolver for DurableReplayController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.authority_id.get().cloned()
    }
}
#[async_trait::async_trait]
impl crate::RuntimeEffectController for DurableReplayController {
    fn effect_journaling(&self) -> crate::EffectJournaling {
        crate::EffectJournaling::Journaled
    }

    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("the authority-check tests execute no effects")
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DurableReplayController"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DurableReplayController"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("DurableReplayController"))
    }
}

/// A scoped durable-participant controller whose await-event authority is this
/// host's, so the host's binding derivation accepts it.
fn durable_child_controller(host: &Arc<dyn EffectHost>) -> ScopedEffectController<'static> {
    let controller = Arc::new(DurableReplayController {
        authority_id: std::sync::OnceLock::new(),
    });
    controller
        .authority_id
        .set(host.turn_control_binding_id())
        .expect("the authority id is set once");
    ScopedEffectController::shared(
        controller,
        crate::AdmittedScope::turn("child-session", "turn"),
    )
    .expect("a valid child scope")
}

/// A request whose recorded cancellation authority is exactly what `host`
/// derives for the child's admitted scope — the fixture every durable-side
/// check needs, because a durable participant always records `Some`.
fn durably_admitted_request(
    host: &Arc<dyn EffectHost>,
    routing: ToolChildCompletionRouting,
) -> ToolChildRequest {
    let derived = crate::runtime::effect::executor::turn_control_binding_id_for_scope(
        &host.turn_control_binding_id(),
        &ExecutionScope::turn("child-session", "turn"),
    )
    .expect("a scope-derived binding id");
    let mut request = request().with_cancellation_authority(
        crate::TurnControlBindingId::new(derived).expect("a valid binding id"),
    );
    request.completion_routing = routing;
    request
}

/// §14's routing line, wrong issuer: a process-lifetime key minted by another
/// registry is unresolvable here — refused, never re-minted into a second
/// dispatch.
#[tokio::test]
async fn a_process_lifetime_key_from_a_foreign_issuer_is_refused() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let mut request = request();
    request.completion_routing = ToolChildCompletionRouting::ProcessLifetime {
        issuer: crate::TurnControlBindingId::new("registry-not-this-one")
            .expect("a valid binding id"),
    };
    let error = validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect_err("a key issued by another registry is refused");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting
    );
}
/// The same routing bound to this host's registry identity is accepted.
#[tokio::test]
async fn a_process_lifetime_key_from_this_registry_is_accepted() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let mut request = request();
    request.completion_routing = ToolChildCompletionRouting::ProcessLifetime {
        issuer: crate::TurnControlBindingId::new(host.turn_control_binding_id())
            .expect("a valid binding id"),
    };
    validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect("a key this registry issued resolves here");
}
/// `Durable` routing needs a durable await-event authority behind the child's
/// controller — on a locally-participating one the child is refused rather
/// than parked on a key nothing resolves. The request is a consistent local
/// admission (`None` cancellation record), so the refusal it reaches is the
/// routing check's, not the cancellation matrix's.
#[tokio::test]
async fn durable_routing_without_a_durable_authority_is_refused() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let mut request = request();
    request.completion_routing = ToolChildCompletionRouting::Durable;
    let error = validate_recorded_authorities(&tool_children, &child_controller(), &request)
        .await
        .expect_err("durable routing needs a durable await-event authority");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting
    );
}
/// §14's routing line, right issuer wrong participation: a process-lifetime
/// key is a local-participation admission — a durable journal would resolve it
/// after the issuing process is gone. Even this registry's own issuer id is
/// therefore refused on a durable-journaled controller, before any key is
/// prepared.
#[tokio::test]
async fn a_process_lifetime_key_is_refused_under_durable_participation() {
    let host: Arc<dyn EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let tool_children = tool_children(&host);
    let controller = durable_child_controller(&host);
    let mut request = durably_admitted_request(&host, ToolChildCompletionRouting::Inline);
    request.completion_routing = ToolChildCompletionRouting::ProcessLifetime {
        issuer: crate::TurnControlBindingId::new(host.turn_control_binding_id())
            .expect("a valid binding id"),
    };
    let error = validate_recorded_authorities(&tool_children, &controller, &request)
        .await
        .expect_err("a process-lifetime key cannot ride a durable journal");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeEffectToolChildCompletionRouting
    );
}
