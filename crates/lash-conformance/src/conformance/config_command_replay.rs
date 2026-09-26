//! The config-command contract's store laws over the shared ingress
//! (FIG-3541, ADR 0101 §12).
//!
//! Every backend owes the same drain: claim the command lane, plan the
//! claimed `ApplyConfigPatch` commands against the session's durable head
//! config through [`plan_config_commands`], commit the planned head and
//! settle the claim — outcomes and refused windows — in the same pass. These
//! laws drive that drain against the real store and read the rows back.

use lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION;

use lash_core::store::{
    ConfigCommandPlan, ConfigRefusalCode, DriveFence, IngressCommandOutcome, IngressCommandResult,
    IngressEnqueueOutcome, IngressItemDraft, IngressLane, IngressPayload, IngressRefusedWindow,
    IngressSettlementIntent, IngressState, IngressTerminalCause,
    persisted_session_config_from_state, plan_config_commands,
};

use super::session_ingress::{
    SessionIngressHandles, Turns, admit, claim_commands, input, replay, row, rows, runtime_lease,
    seal, session, settle, wake,
};

fn accept_any_route(_: &str, _: &crate::ModelSpec) -> Result<(), ConfigRefusalCode> {
    Ok(())
}

/// A config-patch command written against `base` that moves the model to
/// `model_id`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a literal model spec is valid"
)]
fn model_patch(key: &str, base: u64, model_id: &str) -> IngressItemDraft {
    IngressItemDraft::session_command(
        session(),
        crate::SessionCommand::ApplyConfigPatch {
            patch: Box::new(crate::ApplyConfigPatch {
                base_config_revision: base,
                model: Some(
                    crate::ModelSpec::builder(model_id)
                        .context_window_tokens(200_000)
                        .build()
                        .expect("a fixed conformance model spec is valid"),
                ),
                ..crate::ApplyConfigPatch::default()
            }),
        },
        key,
    )
}

/// The session's durable `config_revision` — `0` while no head is committed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn head_config_revision(handles: &SessionIngressHandles) -> u64 {
    handles
        .runtime
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .map_or(0, |head| head.config.config_revision)
}

/// Commit `turns`'s state as a head with no lane movement — the unrelated
/// commit a `config_revision` must not move under.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit_plain_head(
    handles: &SessionIngressHandles,
    lease: &crate::SessionExecutionLease,
    turns: &mut Turns,
    tag: &str,
) {
    let mut commit = crate::RuntimeCommit::persisted_state_with_operation_for_testing(
        &turns.state,
        &[],
        crate::OperationId::new(
            crate::ExecutionScope::runtime_operation(format!("config-law-{tag}")),
            "commit",
        ),
    );
    commit.session_execution_lease_fence = Some(lease.authority());
    let receipt = handles
        .runtime
        .commit_runtime_state(commit)
        .await
        .expect("commit a head");
    turns.state.head_revision = receipt.head_revision;
}

/// The drain the runtime runs at a boundary: claim the command lane, plan
/// the claimed patches against the durable head config, commit the planned
/// head and settle the claim's outcomes and refused windows in one pass.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drain_config_commands(
    handles: &SessionIngressHandles,
    fence: &DriveFence,
    turns: &mut Turns,
    lease: &crate::SessionExecutionLease,
    validate: &dyn Fn(&str, &crate::ModelSpec) -> Result<(), ConfigRefusalCode>,
) -> ConfigCommandPlan {
    let claim = claim_commands(handles, fence)
        .await
        .expect("a config-patch claim is available");
    let mut commands = Vec::with_capacity(claim.items.len());
    for item in &claim.items {
        let IngressPayload::SessionCommand { command } = &item.payload else {
            panic!("a coalesced command claim holds only commands");
        };
        let crate::SessionCommand::ApplyConfigPatch { patch } = command else {
            panic!("a coalesced command claim holds only config patches");
        };
        commands.push((item.item_id.clone(), item.enqueue_seq, patch.as_ref()));
    }
    let running = handles
        .runtime
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .map_or_else(
            || persisted_session_config_from_state(&turns.state),
            |head| head.config,
        );
    let plan = plan_config_commands(&running, &commands, validate);
    // The drain's head commit: every applied patch lands on the running
    // state — which the commit projects into the head — advancing
    // `config_revision` once each, exactly as `plan.config` says.
    for (outcome, command) in plan.outcomes.iter().zip(commands.iter()) {
        if matches!(outcome.result, IngressCommandResult::Applied) {
            command
                .2
                .apply_to_state(&mut turns.state)
                .expect("a planned-applied patch applies");
        }
    }
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&turns.state, &[]);
    commit.session_execution_lease_fence = Some(lease.authority());
    let receipt = handles
        .runtime
        .commit_runtime_state(commit)
        .await
        .expect("commit the drain's head");
    turns.state.head_revision = receipt.head_revision;
    settle(
        handles,
        fence,
        &[&claim],
        IngressSettlementIntent::Commands {
            outcomes: plan.outcomes.clone(),
            refused_windows: plan.refused_windows.clone(),
        },
    )
    .await
    .expect("settle the drain's claim");
    plan
}

/// The FIG-3541 replay law: an applied config command is never reapplied —
/// its identical retry meets the settled tombstone, and a retry that lands a
/// fresh row after the tombstone's vacuum plans stale against the moved head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_command_replay_after_settlement_is_not_reapplied(
    handles: SessionIngressHandles,
) {
    let fence = seal(&handles, "config-replay").await;
    let lease = runtime_lease(&handles, "config-replay").await;
    let mut turns = Turns::new();
    let draft = model_patch("replay-key", 0, "model-b");
    let admitted = admit(&handles, draft.clone()).await;

    let plan = drain_config_commands(&handles, &fence, &mut turns, &lease, &accept_any_route).await;
    assert_eq!(plan.config.config_revision, 1);
    assert_eq!(
        plan.outcomes,
        vec![IngressCommandOutcome {
            item_id: admitted.item_id.clone(),
            result: IngressCommandResult::Applied,
        }]
    );
    let all = rows(&handles).await;
    let settled = row(&all, &admitted);
    assert_eq!(settled.state, IngressState::Completed);
    assert_eq!(settled.terminal_cause, Some(IngressTerminalCause::Applied));

    // The tombstone is the replay's answer: same digest, `Existing`, and the
    // settle cause is readable on the row.
    let replayed = replay(&handles, draft.clone()).await;
    let IngressEnqueueOutcome::Existing(tombstone) = &replayed else {
        panic!("an identical retry over a tombstone is Existing, got {replayed:?}")
    };
    assert_eq!(tombstone.item_id, admitted.item_id);
    assert_eq!(
        tombstone.terminal_cause,
        Some(IngressTerminalCause::Applied)
    );

    // Vacuum removes the tombstone; the retry then lands as a fresh row —
    // and its plan against the advanced head is stale, never applied again.
    handles
        .ingress
        .vacuum_session_ingress(&session())
        .await
        .expect("vacuum the tombstone");
    let readmitted = admit(&handles, draft.clone()).await;
    assert_eq!(readmitted.item_id, admitted.item_id);
    let replay_plan =
        drain_config_commands(&handles, &fence, &mut turns, &lease, &accept_any_route).await;
    assert_eq!(
        replay_plan.outcomes[0].result,
        IngressCommandResult::StaleConfigRevision { base: 0, head: 1 },
        "the replayed command is stale: the head moved past its base"
    );
    assert_eq!(replay_plan.config.config_revision, 1);
    let all = rows(&handles).await;
    let settled = row(&all, &readmitted);
    assert_eq!(settled.state, IngressState::Completed);
    assert_eq!(
        settled.terminal_cause,
        Some(IngressTerminalCause::StaleConfigRevision { base: 0, head: 1 })
    );

    // The durable head still carries exactly the first application.
    let head = handles
        .runtime
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .expect("the session head is committed");
    assert_eq!(head.config.config_revision, 1);
    assert_eq!(head.config.model.id, "model-b");
}

/// `config_revision` advances once per applied patch and never otherwise:
/// not for an unrelated head commit, not for a refused command, and not for
/// a stale one.
pub async fn config_revision_advances_once_per_applied_patch_and_never_otherwise(
    handles: SessionIngressHandles,
) {
    let fence = seal(&handles, "config-revision").await;
    let lease = runtime_lease(&handles, "config-revision").await;
    let mut turns = Turns::new();

    // An unrelated head commit moves the head revision, not the config's.
    let revision_before = head_config_revision(&handles).await;
    let head_before = turns.state.head_revision;
    commit_plain_head(&handles, &lease, &mut turns, "unrelated").await;
    assert!(
        turns.state.head_revision > head_before,
        "the commit moved the head"
    );
    assert_eq!(head_config_revision(&handles).await, revision_before);

    // Two patches in one drain each advance the revision exactly once.
    admit(&handles, model_patch("p1", 0, "model-b")).await;
    admit(&handles, model_patch("p2", 1, "model-c")).await;
    let plan = drain_config_commands(&handles, &fence, &mut turns, &lease, &accept_any_route).await;
    assert_eq!(plan.config.config_revision, 2);
    assert_eq!(head_config_revision(&handles).await, 2);

    // A refused route changes nothing.
    admit(&handles, model_patch("p3", 2, "model-d")).await;
    let refused = drain_config_commands(&handles, &fence, &mut turns, &lease, &|_, model| {
        assert_eq!(model.id, "model-d");
        Err(ConfigRefusalCode::ProviderRouteUnknown)
    })
    .await;
    assert_eq!(refused.config.config_revision, 2);
    assert_eq!(head_config_revision(&handles).await, 2);

    // A stale patch is refused before validation and changes nothing.
    admit(&handles, model_patch("p4", 0, "model-e")).await;
    let calls = std::cell::Cell::new(0usize);
    let stale = drain_config_commands(&handles, &fence, &mut turns, &lease, &|_, _| {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .await;
    assert_eq!(calls.get(), 0, "a stale patch is never validated");
    assert_eq!(
        stale.outcomes[0].result,
        IngressCommandResult::StaleConfigRevision { base: 0, head: 2 }
    );
    assert_eq!(head_config_revision(&handles).await, 2);
}

/// Adjacent config patches claim together and apply in enqueue order against
/// the running revision; a base the drain has already passed settles stale,
/// and one head commit carries the whole drain.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn coalesced_patches_apply_in_seq_order_against_a_running_revision(
    handles: SessionIngressHandles,
) {
    let fence = seal(&handles, "config-coalescing").await;
    let lease = runtime_lease(&handles, "config-coalescing").await;
    let mut turns = Turns::new();

    // Bases (0, 1, 1): the first two apply and move the revision to 2; the
    // third was written against a revision the drain already passed.
    let first = admit(&handles, model_patch("first", 0, "model-b")).await;
    let second = admit(&handles, model_patch("second", 1, "model-c")).await;
    let late = admit(&handles, model_patch("late", 1, "model-d")).await;

    let plan = drain_config_commands(&handles, &fence, &mut turns, &lease, &accept_any_route).await;
    assert_eq!(
        plan.outcomes
            .iter()
            .map(|outcome| outcome.result.clone())
            .collect::<Vec<_>>(),
        vec![
            IngressCommandResult::Applied,
            IngressCommandResult::Applied,
            IngressCommandResult::StaleConfigRevision { base: 1, head: 2 },
        ],
        "one drain applies the in-order prefixes and reports the stale tail"
    );
    assert_eq!(plan.config.config_revision, 2);
    assert_eq!(plan.config.model.id, "model-c");

    let all = rows(&handles).await;
    assert_eq!(
        row(&all, &first).terminal_cause,
        Some(IngressTerminalCause::Applied)
    );
    assert_eq!(
        row(&all, &second).terminal_cause,
        Some(IngressTerminalCause::Applied)
    );
    assert_eq!(
        row(&all, &late).terminal_cause,
        Some(IngressTerminalCause::StaleConfigRevision { base: 1, head: 2 }),
        "the stale command's tombstone records the base and the head"
    );
    assert_eq!(
        row(&all, &late).state,
        IngressState::Completed,
        "a stale command was seen and settled: completed, not cancelled"
    );

    let head = handles
        .runtime
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .expect("the session head is committed");
    assert_eq!(head.config.config_revision, 2);
    assert_eq!(head.config.model.id, "model-c");
}

/// A recomputed patch under a used idempotency key is a typed `Conflict`,
/// while an identical one is `Existing` — the key owns the submission.
pub async fn a_recomputed_patch_under_a_used_key_is_a_conflict(handles: SessionIngressHandles) {
    seal(&handles, "config-conflict").await;
    let admitted = admit(&handles, model_patch("same-key", 0, "model-a")).await;

    let identical = replay(&handles, model_patch("same-key", 0, "model-a")).await;
    assert!(
        matches!(&identical, IngressEnqueueOutcome::Existing(row) if row.item_id == admitted.item_id),
        "an identical retry is Existing: {identical:?}"
    );

    let recomputed = replay(&handles, model_patch("same-key", 3, "model-b")).await;
    assert!(
        matches!(
            &recomputed,
            IngressEnqueueOutcome::Conflict { existing_item_id }
                if *existing_item_id == admitted.item_id
        ),
        "a different patch under the same key conflicts: {recomputed:?}"
    );
    // The open row is untouched.
    let all = rows(&handles).await;
    assert_eq!(row(&all, &admitted).state, IngressState::Open);
}

/// A head written before the contract and a patch written before it are
/// refused typed — never defaulted or silently applied.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pre_contract_head_and_patch_are_refused_typed(handles: SessionIngressHandles) {
    let lease = runtime_lease(&handles, "pre-contract").await;
    let mut turns = Turns::new();
    commit_plain_head(&handles, &lease, &mut turns, "pre-contract").await;

    // A patch from before the contract refuses at validation, before any CAS.
    let legacy_patch = crate::ApplyConfigPatch {
        schema_version: crate::FleetFormat::current()
            .writer_version(lash_core::surface_format!(SESSION_HEAD_META_SCHEMA_VERSION))
            - 1,
        ..crate::ApplyConfigPatch::default()
    };
    let refusal = legacy_patch
        .validate()
        .expect_err("a pre-contract patch is refused");
    assert_eq!(refusal.code, crate::RuntimeErrorCode::SessionCommandClaim);

    // A head written before the contract refuses at load rather than
    // answering with a defaulted `config_revision`.
    handles
        .ingress
        .rewrite_session_tool_access_for_testing(
            crate::store::SESSION_HEAD_META_SCHEMA_VERSION - 1,
            None,
        )
        .await
        .expect("rewrite the head at a pre-contract schema version");
    assert!(
        matches!(
            handles.runtime.load_session_head_meta().await,
            Err(crate::StoreError::UnsupportedRecordSchemaVersion { .. })
        ),
        "a pre-contract head is refused typed at load"
    );
}

/// HoS decision 68: a refused config command turns back the open turn-lane
/// items enqueued after it and before the drain's next config command —
/// nothing earlier, nothing after, nothing admitted since the drain.
pub async fn a_refused_command_refuses_only_its_window(handles: SessionIngressHandles) {
    let fence = seal(&handles, "config-window").await;
    let lease = runtime_lease(&handles, "config-window").await;
    let mut turns = Turns::new();

    // before ─ R(refused) ─ in-window ─ in-window ─ A(applies) ─ after-window
    let before = admit(&handles, input("before the command")).await;
    let refused_command = admit(&handles, model_patch("refused", 0, "model-x")).await;
    let in_window = admit(&handles, input("inside the window")).await;
    let in_window_too = admit(&handles, input("inside too")).await;
    let next_command = admit(&handles, model_patch("next", 0, "model-y")).await;
    let after_window = admit(&handles, input("after the window")).await;

    assert!(
        before.enqueue_seq < refused_command.enqueue_seq
            && refused_command.enqueue_seq < in_window.enqueue_seq
            && in_window_too.enqueue_seq < next_command.enqueue_seq
            && next_command.enqueue_seq < after_window.enqueue_seq,
        "the fixture's interleaving is the asserted one"
    );

    let plan = drain_config_commands(&handles, &fence, &mut turns, &lease, &|_, model| {
        if model.id == "model-x" {
            Err(ConfigRefusalCode::ProviderCredentialsMissing)
        } else {
            Ok(())
        }
    })
    .await;
    assert_eq!(
        plan.refused_windows,
        vec![IngressRefusedWindow {
            after: refused_command.enqueue_seq,
            before: Some(next_command.enqueue_seq),
            code: ConfigRefusalCode::ProviderCredentialsMissing,
        }]
    );

    let all = rows(&handles).await;
    // The refused command was seen and settled — completed — with the typed
    // cause; the items its window covers are cancelled with the same cause.
    let refused_row = row(&all, &refused_command);
    assert_eq!(refused_row.state, IngressState::Completed);
    assert_eq!(
        refused_row.terminal_cause,
        Some(IngressTerminalCause::Refused {
            code: ConfigRefusalCode::ProviderCredentialsMissing,
        })
    );
    for covered in [&in_window, &in_window_too] {
        let covered_row = row(&all, covered);
        assert_eq!(covered_row.state, IngressState::Cancelled);
        assert_eq!(
            covered_row.terminal_cause,
            Some(IngressTerminalCause::Refused {
                code: ConfigRefusalCode::ProviderCredentialsMissing,
            }),
            "the window's items are turned back with the command's code"
        );
    }
    // The items before it and after the next command stand.
    assert_eq!(row(&all, &before).state, IngressState::Open);
    assert_eq!(row(&all, &after_window).state, IngressState::Open);
    // The next command applied under the unchanged revision.
    let next_row = row(&all, &next_command);
    assert_eq!(next_row.state, IngressState::Completed);
    assert_eq!(next_row.terminal_cause, Some(IngressTerminalCause::Applied));
    assert_eq!(head_config_revision(&handles).await, 1);
}

/// A refused command's window ends at the lane's tail when no later config
/// command bounds it.
pub async fn a_refused_command_without_a_successor_refuses_to_the_lane_tail(
    handles: SessionIngressHandles,
) {
    let fence = seal(&handles, "config-window-tail").await;
    let lease = runtime_lease(&handles, "config-window-tail").await;
    let mut turns = Turns::new();

    let refused_command = admit(&handles, model_patch("refused", 0, "model-x")).await;
    let covered_input = admit(&handles, input("covered")).await;
    let covered_wake = admit(&handles, wake("proc", 1)).await;

    let plan = drain_config_commands(&handles, &fence, &mut turns, &lease, &|_, _| {
        Err(ConfigRefusalCode::ProviderRouteUnknown)
    })
    .await;
    assert_eq!(
        plan.refused_windows,
        vec![IngressRefusedWindow {
            after: refused_command.enqueue_seq,
            before: None,
            code: ConfigRefusalCode::ProviderRouteUnknown,
        }]
    );

    let all = rows(&handles).await;
    for covered in [&covered_input, &covered_wake] {
        let covered_row = row(&all, covered);
        assert_eq!(covered_row.state, IngressState::Cancelled);
        assert_eq!(
            covered_row.terminal_cause,
            Some(IngressTerminalCause::Refused {
                code: ConfigRefusalCode::ProviderRouteUnknown,
            })
        );
    }
    assert_eq!(head_config_revision(&handles).await, 0);
    let lane = || {
        all.iter()
            .filter(|item| item.lane() == IngressLane::Turn)
            .count()
    };
    assert_eq!(lane(), 2, "the lane held exactly the two refused items");
}
