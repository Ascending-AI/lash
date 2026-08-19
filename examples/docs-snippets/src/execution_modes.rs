//! Compiled sources for the Rust snippets on `docs/execution-modes.html`.

use std::sync::Arc;

use lash::ModelSpec;
use lash::provider::ProviderHandle;

async fn standard_mode(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:standard-core
    // `LashCore::standard_builder(lash::TurnBudget::Unbounded)` selects native provider tool-calling, the default mode.
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    // A plain `open()` runs the session in standard mode.
    let session = core.session("chat-1").open().await?;
    // docs:end:standard-core
    Ok(())
}

async fn rlm_mode(provider: ProviderHandle, model: ModelSpec) -> anyhow::Result<()> {
    // docs:start:rlm-core
    // Build one RLM core; each session chooses its durable source dialect.
    use lash::rlm::{RLM_PROTOCOL_PLUGIN_ID, RlmCreateExtras, RlmDialect};

    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::new(
            lash::rlm::ExecutionBound::instructions(1_000_000),
            lash::rlm::ExecutionBound::secs(30),
            lash::rlm::ExecutionBound::instructions(64 * 1024 * 1024),
        ),
        Arc::new(lash::persistence::InMemoryLashlangArtifactStore::new()),
    );
    let core = lash::LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
        .provider(provider)
        .model(model)
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        // Start bounded; tune both limits for your backend's latency envelope.
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())?;

    // Absence is the durable Lashlang default.
    assert_eq!(RlmDialect::Lashlang.language_id(), "lashlang");
    let lashlang = core.session("task-lashlang").open().await?;
    assert_eq!(
        lashlang.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("lashlang")
    );

    // TypeScript is *stated* at creation and remains pinned on rehydrate. The
    // statement goes through the plugin-agnostic options seam and is applied as
    // a guarded set-if-unset write: it lands on a session that recorded
    // nothing, and a session that recorded another dialect refuses rather than
    // reopening in the old one.
    assert_eq!(RlmDialect::Typescript.language_id(), "typescript");
    let typescript = core
        .session("task-typescript")
        .plugin_option(
            RLM_PROTOCOL_PLUGIN_ID,
            RlmCreateExtras {
                dialect: Some(RlmDialect::Typescript),
                ..RlmCreateExtras::default()
            },
        )?
        .open()
        .await?;
    assert_eq!(
        typescript.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("typescript")
    );

    // A host that offers the choice — a create form, a flag, an environment
    // variable — reads the menu from the registered dialects rather than
    // writing its own list, and refuses an unregistered id instead of quietly
    // defaulting it: the dialect is pinned for the session's whole life.
    assert_eq!(
        RlmDialect::ALL.map(RlmDialect::language_id),
        ["lashlang", "typescript"]
    );
    assert_eq!(
        RlmDialect::from_language_id("typescript"),
        Some(RlmDialect::Typescript)
    );
    assert_eq!(RlmDialect::from_language_id("lashscript"), None);
    assert_eq!(
        RlmDialect::registered_language_ids(),
        "`lashlang`, `typescript`"
    );

    // Durable session facts are read as recorded and written set-if-unset
    // (ADR 0066). Absence is a distinct answer from the default: this session
    // never stated a termination, so it reads as `None` rather than `Natural`.
    use lash::rlm::{
        RlmSessionConfig, RlmSessionConfigConflict, RlmSessionConfigError, RlmSessionExt as _,
        RlmTermination,
    };

    let stated = Some(RlmDialect::Typescript);
    assert_eq!(typescript.rlm_config().dialect, stated);
    assert!(RlmSessionConfig::new().is_empty());
    let recorded = typescript.rlm_config();
    assert_eq!(recorded.termination, None);

    // `assert` is host code: compare the read against what you require.
    if recorded.dialect != Some(RlmDialect::Typescript) {
        anyhow::bail!("this session is not the one this job was written for");
    }

    // `prefer` is the guarded write: it lands on a fact the session has not
    // recorded, and restating a recorded fact is a no-op.
    let written = typescript
        .set_rlm_config_if_unset(
            RlmSessionConfig::new().termination(RlmTermination::FinishRequired { schema: None }),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let term = written.termination;
    assert!(matches!(term, Some(RlmTermination::FinishRequired { .. })));

    // Disagreeing with a recorded fact is a typed refusal, never a fallback:
    // the host reads the two values off the error instead of its prose.
    let refused = typescript
        .set_rlm_config_if_unset(RlmSessionConfig::new().dialect(RlmDialect::Lashlang))
        .await;
    assert!(matches!(refused, Err(RlmSessionConfigError::Conflict(_))));
    let Err(RlmSessionConfigError::Conflict(conflict)) = &refused else {
        anyhow::bail!("a recorded dialect must refuse a different one");
    };
    assert_eq!(conflict.field(), "dialect");
    assert!(matches!(conflict, RlmSessionConfigConflict::Dialect { .. }));
    let RlmSessionConfigConflict::Dialect {
        recorded,
        requested,
    } = conflict
    else {
        anyhow::bail!("a refused dialect names the dialect fact it refused");
    };
    assert_eq!(*recorded, RlmDialect::Typescript);
    assert_eq!(*requested, RlmDialect::Lashlang);
    // docs:end:rlm-core
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn documented_execution_mode_builders_resolve() {
        standard_mode(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("standard-mode snippet must build");
        rlm_mode(
            crate::test_support::provider(),
            crate::test_support::model(),
        )
        .await
        .expect("RLM snippet must build");
    }
}
