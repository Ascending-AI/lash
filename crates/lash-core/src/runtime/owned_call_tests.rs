//! CALL-DEADLINE (FIG-5259): an owned call's resend keeps the model-total
//! deadline its admission pinned, sends the admitted body without lowering
//! again, and is never sent once that deadline has passed.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_durable::domain::{ModelCallId, PromptCallKey};
use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeId, NodeSpec};
use tokio_util::sync::CancellationToken;

use super::{OwnedAdmission, OwnedCall, OwnedPrompt};
use crate::prompt_sections::{PromptPlan, PromptPurpose};
use crate::testing::{TestClock, TestProvider};
use crate::{
    ActorContext, ExecutionBudgets, ExecutionBudgetsConfig, LlmProfiles as _,
    RecordedRequestTemplate,
};

const SESSION: &str = "owned-call-deadline";
const MODEL: &str = "owned-call-model";
const START_MS: u64 = 1_000_000;
const MODEL_TOTAL: Duration = Duration::from_secs(900);

struct World {
    clock: Arc<TestClock>,
    cx: ActorContext,
    binding: crate::LlmProfileBinding,
    request: crate::LlmRequest,
    lowerings: Arc<AtomicUsize>,
}

async fn world() -> World {
    let clock = Arc::new(TestClock::new(START_MS));
    let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
        .await
        .expect("open the memory store set");
    let backend = crate::Backend::for_testing(Arc::new(stores));
    let formats = FormatSet::new("owned-call-deadline");
    let actor = ActorKey::session(SESSION).expect("a session actor key");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats.clone());
    backend
        .durable()
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("create the session actor");
    let lease = backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new("owned-call-owner"),
            decodes: vec![formats],
            ttl_millis: 15_000,
        })
        .await
        .expect("register the owner");
    let claimed = backend.durable().claim(&lease, 1).await.expect("claim");
    let cx = ActorContext::new(
        backend,
        actor,
        claimed[0].epoch,
        crate::AdmittedScope::turn(SESSION, "run-1"),
        CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    );
    // Every lowering is a builder of its own generation.
    let lowerings = Arc::new(AtomicUsize::new(0));
    let provider = {
        let lowerings = Arc::clone(&lowerings);
        TestProvider::builder()
            .kind("owned-call")
            .template(move |request| {
                let generation = lowerings.fetch_add(1, Ordering::SeqCst) + 1;
                RecordedRequestTemplate::literal(
                    crate::ProviderRouteIdentity::new(
                        "owned-call",
                        "owned-call",
                        request.model.wire_model(),
                    ),
                    request.stream_events.is_some(),
                    None,
                    format!("{{\"builder\":{generation}}}"),
                )
            })
            .build()
            .into_handle()
    };
    let registry = crate::LlmProfileRegistry::new()
        .register(
            MODEL,
            crate::RegisteredLlmProfile::new(
                crate::LlmProfileMetadata::builder(MODEL)
                    .context_window_tokens(128_000)
                    .build()
                    .expect("valid model"),
                provider,
            ),
        )
        .expect("register the model");
    let recorded = registry
        .snapshot(&crate::LlmProfileKey::new(MODEL))
        .expect("the model is registered");
    let request = crate::direct::build_llm_request(
        crate::DirectRequest::text("summarize"),
        crate::LlmProfileConfig::new(recorded.clone()),
    )
    .expect("the request builds");
    World {
        binding: crate::LlmProfileBinding::new(recorded, Arc::new(registry), clock.clone()),
        clock,
        cx,
        request,
        lowerings,
    }
}

fn budgets() -> ExecutionBudgets {
    ExecutionBudgets::new(ExecutionBudgetsConfig {
        model_total: MODEL_TOTAL,
        ..ExecutionBudgetsConfig::default()
    })
    .expect("valid budgets")
}

/// The owner's redrive making call `key` again.
async fn admit(world: &World) -> OwnedAdmission {
    let plugins = lash_core_execution::testing::test_plugin_host(Vec::new())
        .build_session(crate::plugin::PluginSessionRequest::creation(
            SESSION,
            Default::default(),
        ))
        .expect("the plugin session builds");
    OwnedCall {
        cx: &world.cx,
        key: PromptCallKey {
            session: crate::SessionId::from(SESSION),
            call: ModelCallId::Owned {
                owner: "session-operation:compaction".to_owned(),
                key: "summary".to_owned(),
            },
        },
        purpose: PromptPurpose::Compaction,
        prompt: OwnedPrompt {
            facts: None,
            plugins,
            plan: PromptPlan::default(),
            config: crate::AdmittedPluginConfig::default(),
            frame: None,
            session: None,
        },
        request: world.request.clone(),
        binding: world.binding.clone(),
        attachment_store: Arc::new(crate::RuntimeAttachmentStore::unavailable()),
        budgets: budgets(),
    }
    .admit()
    .await
    .expect("the owner admits the call")
}

fn sent(admission: OwnedAdmission) -> (Arc<RecordedRequestTemplate>, u64) {
    match admission {
        OwnedAdmission::Send { admitted, .. } => (admitted.template, admitted.limit.expires_at),
        OwnedAdmission::Unsent(error) => panic!("the call settled unsent: {error:?}"),
    }
}

#[tokio::test]
async fn an_owned_calls_resend_keeps_its_pinned_deadline_and_body() {
    let world = world().await;
    let pinned = START_MS + u64::try_from(MODEL_TOTAL.as_millis()).expect("ms");

    let (first, deadline) = sent(admit(&world).await);
    assert_eq!(deadline, pinned, "the admission pins the model total");

    // A redrive later in the call's life resends the admitted bytes under
    // the deadline the admission pinned, not a fresh one.
    world.clock.advance(10_000);
    let (again, deadline) = sent(admit(&world).await);
    assert_eq!(again, first, "a resend sends the admitted body");
    assert_eq!(deadline, pinned, "a resend keeps the pinned deadline");
    assert_eq!(
        world.lowerings.load(Ordering::SeqCst),
        1,
        "nothing lowers again"
    );

    // Past the pinned deadline the call is never sent again.
    world.clock.set(pinned);
    match admit(&world).await {
        OwnedAdmission::Unsent(error) => assert_eq!(
            error.code,
            Some(crate::FailureCode::lash(
                crate::TurnFailureCode::ModelTotalExceeded
            )),
            "{error:?}"
        ),
        OwnedAdmission::Send { admitted, .. } => panic!(
            "a call past its pinned deadline was sent again until {}",
            admitted.limit.expires_at
        ),
    }
    assert_eq!(
        world.lowerings.load(Ordering::SeqCst),
        1,
        "nothing lowers again"
    );
}
