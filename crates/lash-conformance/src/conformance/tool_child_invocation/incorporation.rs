use pretty_assertions::assert_eq;

use super::*;

// =============================================================================
// §6 group prefix incorporation: the journaled record pins the ranks (FIG-3411)
// =============================================================================

/// The incorporation law's group: a usage leaf that settles at once beside a
/// deferred leaf parked on its completion key — the late settlement the
/// record must exclude.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn incorporation_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    routing: ToolChildCompletionRouting,
    cancellation: crate::TurnControlBindingId,
) -> crate::RuntimeEffectGroup {
    let parent = parent_invocation(scope);
    let leaf = |position: usize, tool_id: &str, routing: ToolChildCompletionRouting| {
        child_envelope(
            scope,
            group_key,
            position,
            leaf_request(
                scope,
                session_id,
                &format!("{group_key}-call-{position}"),
                tool_id,
                tool_id.trim_start_matches("tool:"),
                catalog_admission(tool_id),
                routing,
                env_ref,
                &parent,
                cancellation.clone(),
            ),
        )
    };
    crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key.to_string(),
        vec![
            leaf(0, LEAF_USAGE, ToolChildCompletionRouting::Inline),
            leaf(1, LEAF_SPEND_DEFERRED, routing),
        ],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the incorporation group assembles")
}

/// The session-ledger stand-in an incorporating context charges into: every
/// delta the applicator applies lands here, counted by `(source, model)`.
#[derive(Default)]
pub(super) struct RecordingCharge {
    charges: std::sync::Mutex<Vec<(String, String, crate::TokenUsage)>>,
}

impl crate::session::UsageChargeSink for RecordingCharge {
    fn charge(
        &self,
        source: &str,
        model: &str,
        usage: &crate::TokenUsage,
    ) -> Result<(), crate::PluginError> {
        self.charges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((source.to_string(), model.to_string(), usage.clone()));
        Ok(())
    }
}

impl RecordingCharge {
    fn snapshot(&self) -> Vec<(String, String, crate::TokenUsage)> {
        self.charges.lock_recover().clone()
    }

    pub(super) fn count(&self) -> usize {
        self.charges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// The opener's execution context over the controller its handler was lent,
/// with the charge sink the incorporation's usage deltas land in.
fn incorporating_context<'run>(
    scoped: crate::ScopedEffectController<'run>,
    session_id: &crate::SessionId,
    charge: Arc<RecordingCharge>,
) -> crate::RuntimeExecutionContext<'run> {
    crate::testing::TestExecutionContextBuilder::over_controller(scoped)
        .session_id(session_id.clone())
        .direct_completions(
            crate::DirectCompletionClient::from_fn(|_request, _source| {
                Err(crate::PluginError::Invoke(
                    "incorporation law context serves no completions".to_string(),
                ))
            })
            .with_usage_charge_sink(charge),
        )
        .build()
        .into_runtime()
}

/// What the opener's crashed execution recorded before it died.
#[derive(Debug)]
struct CrashedOpener {
    incorporated: Vec<crate::runtime::effect::IncorporatedGroupRank>,
    charged: usize,
}

/// What the opener's redelivered execution observed, step by step.
#[derive(Debug)]
struct RedrivenOpener {
    /// The incorporation replayed at the crashed execution's cursor.
    replayed: Vec<crate::runtime::effect::IncorporatedGroupRank>,
    /// Charges after the replayed incorporation.
    replay_charged: usize,
    /// The ledger right after the replayed incorporation.
    replay_ledger: crate::session::IncorporationLedger,
    /// A second incorporation at the same cursor.
    again: Vec<crate::runtime::effect::IncorporatedGroupRank>,
    /// The rank consumed after the replay: the late settlement.
    late_position: usize,
    /// The incorporation that extends the cursor over it.
    extended: Vec<crate::runtime::effect::IncorporatedGroupRank>,
    /// Charges once the extension landed.
    final_charged: usize,
    final_usage: Vec<(String, String, crate::TokenUsage)>,
}

/// The opener's side of the law, as one handler's work: open the group,
/// consume its first settlement and incorporate that prefix. Every execution
/// of the handler runs it from the top; on a replay each step answers from
/// the journal.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn open_and_incorporate_first_rank<'run>(
    scoped: &crate::ScopedEffectController<'run>,
    group: crate::RuntimeEffectGroup,
    session_id: &crate::SessionId,
    charge: &Arc<RecordingCharge>,
) -> (
    crate::EffectGroupHandle,
    crate::RuntimeExecutionContext<'run>,
    Vec<crate::runtime::effect::IncorporatedGroupRank>,
) {
    let mut handle = scoped
        .controller()
        .open_effect_group(group)
        .await
        .expect("the group opens under the live opener");
    // Rank 1 is the usage leaf; the deferred leaf is parked and unsettled.
    let first = next_settlement(scoped, &mut handle, 0).await;
    assert_eq!(first.position, 0, "the usage leaf settles first: {first:?}");
    let context = incorporating_context(scoped.clone(), session_id, Arc::clone(charge));
    let incorporated = context
        .incorporate_group_prefix(&handle)
        .await
        .expect("the opener journals the consumed prefix");
    (handle, context, incorporated)
}

/// §6: the opener's `incorporate_group_prefix` journals the incorporated
/// prefix, and a replay of the opener re-incorporates exactly the recorded
/// ranks — never a settlement that landed after the record was cut
/// (FIG-3411 phase 2c, FIG-4094).
///
/// The group is `[usage leaf, spend-then-deferred leaf]`, and the opener runs
/// where the tier runs a turn — inside a real handler on Restate. It opens
/// the group, consumes the usage leaf's settlement, and
/// `incorporate_group_prefix` journals a record covering rank 1 alone and
/// charges that rank's usage once. The opener then crashes where it stands.
///
/// Only then is the parked leaf resolved, so rank 2's settlement — with its
/// own usage — is durably present before the opener recovers, a fact the
/// record never saw. The tier recovers the opener its own way (Restate
/// redelivers the invocation, replaying its journal). The replayed
/// incorporation at the saved cursor names rank 1 only, so rank 1's spend is
/// charged and rank 2's is not — a settlement that arrived after the record
/// is not early possession. A repeated call at the same cursor journals
/// nothing and applies nothing. Consuming the late settlement and extending
/// the cursor to rank 2 then journals a second record covering exactly that
/// rank.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn group_accounting_conserves_each_incorporated_rank(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-incorporation-session"));
    let scope = crate::ExecutionScope::turn(
        session_id.clone(),
        crate::TurnId::from(format!("{prefix}-incorporation-turn")),
    );
    let admitted = crate::admit(scope.clone());
    let opener = crate::EffectOpener::for_scope(&admitted).expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-incorporation-group");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;
    let _guard = register_opener(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let group = incorporation_group(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        ToolChildCompletionRouting::Durable,
        recorded_cancellation_authority(&host, &admitted).await,
    );

    // The opener's first life: it records rank 1's incorporation, then dies.
    let crash = crate::ConformanceCrash::new();
    let crashed = Arc::new(std::sync::Mutex::new(None::<CrashedOpener>));
    let crashing: crate::ConformanceTurnAttempt = {
        let group = group.clone();
        let session_id = session_id.clone();
        let crash = crash.clone();
        let crashed = Arc::clone(&crashed);
        Arc::new(move |scoped| {
            let group = group.clone();
            let session_id = session_id.clone();
            let crash = crash.clone();
            let crashed = Arc::clone(&crashed);
            Box::pin(async move {
                let charge = Arc::new(RecordingCharge::default());
                let (_handle, _context, incorporated) =
                    open_and_incorporate_first_rank(&scoped, group, &session_id, &charge).await;
                *crashed.lock_recover() = Some(CrashedOpener {
                    incorporated,
                    charged: charge.count(),
                });
                crash.fire();
                std::future::pending().await
            })
        })
    };
    fixture
        .turn_runner
        .run_turn_until_crash(admitted.clone(), crashing, crash)
        .await;
    let CrashedOpener {
        incorporated,
        charged,
    } = crashed
        .lock_recover()
        .take()
        .expect("the opener recorded its incorporation before it crashed");
    assert_eq!(
        incorporated.len(),
        1,
        "the record covers exactly the consumed prefix: {incorporated:?}"
    );
    assert_eq!(incorporated[0].rank, 1);
    assert!(!incorporated[0].child_replay_key.is_empty());
    assert_eq!(
        charged, 1,
        "rank 1's usage was charged exactly once at incorporation"
    );

    // The late settlement: the parked leaf resolves and takes rank 2 — a fact
    // the already-cut record does not name — and is durable before the
    // opener recovers.
    let key = scenario
        .observation
        .parked_key(&format!("{group_key}-call-1"))
        .await;
    resolve_when_registered(
        &host,
        key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "late", "via": "resolver" })),
    )
    .await;
    rank_settled(&host, &admitted, &group_key, 2).await;

    // The opener's recovery: the tier redelivers it, and it runs from the
    // top on a fresh context — a fresh ledger, the journaled record already
    // in the journal.
    let redriven = Arc::new(std::sync::Mutex::new(None::<RedrivenOpener>));
    let redrive: crate::ConformanceTurnAttempt = {
        let group = group.clone();
        let session_id = session_id.clone();
        let redriven = Arc::clone(&redriven);
        Arc::new(move |scoped| {
            let group = group.clone();
            let session_id = session_id.clone();
            let redriven = Arc::clone(&redriven);
            Box::pin(async move {
                let charge = Arc::new(RecordingCharge::default());
                let (mut handle, context, replayed) =
                    open_and_incorporate_first_rank(&scoped, group, &session_id, &charge).await;
                let replay_charged = charge.count();
                let replay_ledger = context.incorporation_ledger_snapshot();
                let again = context
                    .incorporate_group_prefix(&handle)
                    .await
                    .expect("a repeated incorporation at the same prefix succeeds");
                let late = next_settlement(&scoped, &mut handle, 1).await;
                let extended = context
                    .incorporate_group_prefix(&handle)
                    .await
                    .expect("the prefix extension journals");
                let final_charged = charge.count();
                scoped
                    .controller()
                    .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                    .await
                    .expect("the opener closes the group");
                *redriven.lock_recover() = Some(RedrivenOpener {
                    replayed,
                    replay_charged,
                    replay_ledger,
                    again,
                    late_position: late.position,
                    extended,
                    final_charged,
                    final_usage: charge.snapshot(),
                });
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    fixture.turn_runner.run_turn(admitted, redrive).await;
    let redriven = redriven
        .lock_recover()
        .take()
        .expect("the redriven opener ran to its end");

    // The recorded outcome names rank 1 only, so rank 2's settlement — now
    // durably present — is excluded from the replay.
    assert_eq!(
        redriven.replayed, incorporated,
        "replay re-incorporates exactly the recorded ranks"
    );
    assert_eq!(
        redriven.replay_charged, 1,
        "rank 2's late settlement was not incorporated on replay"
    );
    assert!(
        !redriven
            .replay_ledger
            .incorporated
            .iter()
            .any(|source| matches!(
                source,
                crate::session::SettlementSource::GroupRank { rank: 2, .. }
            )),
        "the incorporated prefix is the recorded ranks, no later rank: {:?}",
        redriven.replay_ledger
    );

    // A second call at the same cursor is the no-op the ledger makes it.
    assert!(
        redriven.again.is_empty(),
        "a repeated call at the same through_rank journals nothing: {:?}",
        redriven.again
    );

    // Consuming the late settlement and extending the cursor is a new record
    // covering exactly the new rank.
    assert_eq!(
        redriven.late_position, 1,
        "the deferred leaf settles rank 2"
    );
    assert_eq!(
        redriven.extended.len(),
        1,
        "the extension covers only the newly consumed rank: {:?}",
        redriven.extended
    );
    assert_eq!(redriven.extended[0].rank, 2);
    assert_eq!(
        redriven.final_charged, 2,
        "rank 2's spend is charged by its own record, once"
    );
    assert_eq!(
        redriven.final_usage,
        vec![
            (
                "law-usage-leaf".to_string(),
                "law-model".to_string(),
                law_direct_completion().usage
            ),
            (
                "law-spend-deferred".to_string(),
                "law-model".to_string(),
                law_direct_completion().usage
            ),
        ],
        "each incorporated rank conserves its own source, model and complete token usage"
    );
    assert_eq!(scenario.observation.executions_of("law_usage").len(), 1);
    assert_eq!(
        scenario
            .observation
            .executions_of("law_spend_deferred")
            .len(),
        1
    );
}

/// Waits until `group_key`'s rank `rank` is durably settled, read from
/// outside any opener.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn rank_settled(
    host: &Arc<dyn crate::EffectHost>,
    admitted: &crate::AdmittedScope,
    group_key: &str,
    rank: u64,
) {
    let scoped = host
        .scoped(admitted.clone())
        .expect("the group scope binds");
    let deadline = std::time::Instant::now() + SETTLE_BUDGET;
    loop {
        let settled = scoped
            .controller()
            .read_group_settlement(group_key, rank)
            .await
            .expect("the rank read is answered");
        if settled.is_some() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "rank {rank} of {group_key} never settled"
        );
        tokio::time::sleep(POLL).await;
    }
}
