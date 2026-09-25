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
    pub(super) fn count(&self) -> usize {
        self.charges
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// The opener's execution context over this host's admitted scope, with the
/// charge sink the incorporation's usage deltas land in.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn incorporating_context(
    host: &Arc<dyn crate::EffectHost>,
    admitted: &crate::AdmittedScope,
    session_id: &crate::SessionId,
    charge: Arc<RecordingCharge>,
) -> crate::RuntimeExecutionContext<'static> {
    let controller = host
        .scoped_static(admitted.clone())
        .expect("the host lends a scoped controller")
        .expect("this host hands out owned scoped controllers");
    crate::testing::TestExecutionContextBuilder::over_controller(controller)
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

/// §6: the opener's `incorporate_group_prefix` journals the incorporated
/// prefix, and a replay re-incorporates exactly the recorded ranks — never a
/// settlement that landed after the record was cut (FIG-3411 phase 2c).
///
/// The group is `[usage leaf, spend-then-deferred leaf]`. The usage leaf
/// settles first and is consumed; `incorporate_group_prefix` then journals a
/// record covering rank 1 alone and charges that rank's usage once. Only then
/// is the parked leaf resolved, so rank 2's settlement — with its own usage —
/// is a fact the record never saw.
///
/// A fresh context over the same substrate replays the prefix at the saved
/// cursor: the journaled outcome names rank 1, so rank 1's spend is charged
/// and rank 2's is not — a settlement that arrived after the record is not
/// early possession. A repeated call at the same cursor journals nothing and
/// applies nothing. Extending the cursor to rank 2 then journals a second
/// record covering exactly that rank.
///
/// On tiers whose controller cannot journal this command the
/// law asserts the cursorless rank read the record is built on and stops.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_group_prefix_incorporation_reincorporates_exactly_the_recorded_ranks(
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
    let scoped = host
        .scoped(admitted.clone())
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(incorporation_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &admitted).await,
        ))
        .await
        .expect("the group opens under the live opener");

    // Rank 1 is the usage leaf; the deferred leaf is parked and unsettled.
    let first = next_settlement(&scoped, &mut handle, 0).await;
    assert_eq!(first.position, 0, "the usage leaf settles first: {first:?}");

    let charge = Arc::new(RecordingCharge::default());
    let context = incorporating_context(&host, &admitted, &session_id, Arc::clone(&charge));
    let incorporated = match context.incorporate_group_prefix(&handle).await {
        Ok(incorporated) => incorporated,
        Err(error)
            if error.code == crate::RuntimeErrorCode::RestateEffectHostRequiresHandlerScope =>
        {
            // This tier executes journaled commands only inside a handler
            // context, so the incorporation record is cut there, not on the
            // host controller the law drives. What the law can still hold the
            // tier to is the cursorless read the record is built from.
            let settled = scoped
                .controller()
                .read_group_settlement(&group_key, 1)
                .await
                .expect("the rank read is answered")
                .expect("rank 1 is settled");
            assert_eq!(
                settled.sequence, first.sequence,
                "the cursorless read names the settled rank"
            );
            assert!(!settled.child_replay_key.is_empty());
            return;
        }
        Err(error) => panic!("incorporate_group_prefix failed: {error}"),
    };
    assert_eq!(
        incorporated.len(),
        1,
        "the record covers exactly the consumed prefix: {incorporated:?}"
    );
    assert_eq!(incorporated[0].rank, 1);
    assert!(!incorporated[0].child_replay_key.is_empty());
    assert_eq!(
        charge.count(),
        1,
        "rank 1's usage was charged exactly once at incorporation"
    );

    // The late settlement: the parked leaf resolves and takes rank 2 — a fact
    // the already-cut record does not name.
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
    let mut observer =
        crate::EffectGroupHandle::restored(group_key.clone(), 2, 1).expect("cursor 1 restores");
    let second = next_settlement(&scoped, &mut observer, 1).await;
    assert_eq!(second.position, 1, "the deferred leaf settles rank 2");
    assert!(
        charge.count() == 1,
        "consuming a settlement is not incorporation: rank 2's spend is uncharged"
    );

    // Replay at the saved cursor on a fresh context — a fresh ledger, the
    // journaled record already in the store. The recorded outcome names rank
    // 1 only, so rank 2's settlement — now durably present — is excluded.
    let replay_charge = Arc::new(RecordingCharge::default());
    let replay_context =
        incorporating_context(&host, &admitted, &session_id, Arc::clone(&replay_charge));
    let replayed_handle =
        crate::EffectGroupHandle::restored(group_key.clone(), 2, 1).expect("cursor 1 restores");
    let replayed = replay_context
        .incorporate_group_prefix(&replayed_handle)
        .await
        .expect("the recorded prefix replays");
    assert_eq!(
        replayed, incorporated,
        "replay re-incorporates exactly the recorded ranks"
    );
    assert_eq!(
        replay_charge.count(),
        1,
        "rank 2's late settlement was not incorporated on replay"
    );
    let ledger = replay_context.incorporation_ledger_snapshot();
    assert!(
        !ledger.incorporated.iter().any(|source| matches!(
            source,
            crate::session::SettlementSource::GroupRank { rank: 2, .. }
        )),
        "the incorporated prefix is the recorded ranks, no later rank: {ledger:?}"
    );

    // A second call at the same cursor is the no-op the ledger makes it.
    let again = replay_context
        .incorporate_group_prefix(&replayed_handle)
        .await
        .expect("a repeated incorporation at the same prefix succeeds");
    assert!(
        again.is_empty(),
        "a repeated call at the same through_rank journals nothing: {again:?}"
    );
    assert_eq!(
        replay_charge.count(),
        1,
        "the repeated call charged nothing"
    );

    // Extending the cursor is a new record covering exactly the new rank.
    let full_handle =
        crate::EffectGroupHandle::restored(group_key.clone(), 2, 2).expect("cursor 2 restores");
    let extended = replay_context
        .incorporate_group_prefix(&full_handle)
        .await
        .expect("the prefix extension journals");
    assert_eq!(
        extended.len(),
        1,
        "the extension covers only the newly consumed rank: {extended:?}"
    );
    assert_eq!(extended[0].rank, 2);
    assert_eq!(
        replay_charge.count(),
        2,
        "rank 2's spend is charged by its own record, once"
    );

    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the opener closes the group");
}
