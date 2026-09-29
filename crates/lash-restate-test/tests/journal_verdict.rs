//! Restate's journal verdict on the in-process double (ADR 0113 §7.17).
//!
//! The artifact-cleanup executor severs nothing an execution referrer holds,
//! and nothing a gate protects, until the engine answers `Settled` for the
//! journal (§2.5). This law asks the engine's own effect host, bound to its
//! store set and admin API as a deployment binds it, for each scope kind's
//! verdict: a wait retirement alone never settles a journal, a quiescent
//! runtime operation does, a session delete always is, a process the registry
//! no longer holds was pruned, and a turn whose root has no terminal evidence
//! may still replay. A completed root drive answering `Settled` is the
//! evidence suite's first case, which waits for it before the turn's
//! execution edge goes (`//crates/lash:artifact_referrers_evidence__test`).

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed expect is the test failure"
)]

use lash_core::{EffectJournalRetirement, ExecutionScope, JournalReplay};
use lash_restate_test::ServerConfig;

const SESSION: &str = "journal-verdict-session";

async fn verdict(host: &dyn lash_core::EffectHost, scope: &ExecutionScope) -> JournalReplay {
    host.journal_replay(&scope.journal_identity().expect("scope journal identity"))
        .await
        .expect("read the journal verdict")
}

#[tokio::test]
async fn restate_answers_settled_only_for_a_journal_nothing_can_replay() {
    let restate = lash_restate_test::backend(0x4031_0017, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let backend = restate.lash_backend();
    let host = backend.effect_host();

    // A session delete runs no publication.
    assert_eq!(
        verdict(host.as_ref(), &ExecutionScope::session_delete(SESSION)).await,
        JournalReplay::Settled
    );

    // A runtime operation is settled once its waits are retired under
    // `WhenQuiescent`, and not before.
    let operation = ExecutionScope::runtime_operation("journal-verdict-operation");
    assert_eq!(
        verdict(host.as_ref(), &operation).await,
        JournalReplay::MayReplay
    );
    host.retire_effect_journal(
        EffectJournalRetirement::runtime_operation("journal-verdict-operation").when_quiescent(),
    )
    .await
    .expect("retire the quiescent operation's waits");
    assert_eq!(
        verdict(host.as_ref(), &operation).await,
        JournalReplay::Settled
    );

    // A live process may replay, and retiring its waits proves nothing.
    let registry = backend.process_registry();
    let live = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::Engine {
                    kind: "journal-verdict-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
                "process-env:journal-verdict",
            ))),
        )
        .await
        .expect("register a live process")
        .id;
    let live_scope = ExecutionScope::process(live.clone());
    assert_eq!(
        verdict(host.as_ref(), &live_scope).await,
        JournalReplay::MayReplay
    );
    host.retire_effect_journal(EffectJournalRetirement::process(live))
        .await
        .expect("retire the live process's waits");
    assert_eq!(
        verdict(host.as_ref(), &live_scope).await,
        JournalReplay::MayReplay,
        "a wait retirement alone does not settle a process journal"
    );

    // A process the registry no longer holds was pruned, which requires it
    // terminal with its journals retired.
    assert_eq!(
        verdict(
            host.as_ref(),
            &ExecutionScope::process(lash_core::mint_process_id())
        )
        .await,
        JournalReplay::Settled
    );

    // A turn whose root has no terminal evidence may still replay.
    assert_eq!(
        verdict(
            host.as_ref(),
            &ExecutionScope::turn(SESSION, "journal-verdict-root")
        )
        .await,
        JournalReplay::MayReplay
    );
}
