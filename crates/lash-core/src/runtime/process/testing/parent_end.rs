use std::num::NonZeroUsize;

use crate::{PluginError, ProcessParentEndPlan};

use super::TestLocalProcessRegistry;

pub(super) async fn list(
    registry: &TestLocalProcessRegistry,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessParentEndPlan>, PluginError> {
    let _transaction = registry.transaction.lock().await;
    let managed = registry.managed.lock().await;
    let mut plans = managed
        .iter()
        .filter_map(|(process_id, record)| {
            record
                .parent_end_actions
                .as_ref()
                .map(|actions| ProcessParentEndPlan {
                    process_id: process_id.clone(),
                    actions: actions.clone(),
                })
        })
        .collect::<Vec<_>>();
    plans.sort_by(|left, right| left.process_id.cmp(&right.process_id));
    plans.truncate(limit.get());
    Ok(plans)
}

pub(super) async fn get(
    registry: &TestLocalProcessRegistry,
    process_id: &str,
) -> Result<Option<ProcessParentEndPlan>, PluginError> {
    let _transaction = registry.transaction.lock().await;
    Ok(registry
        .managed
        .lock()
        .await
        .get(process_id)
        .and_then(|record| {
            record
                .parent_end_actions
                .as_ref()
                .map(|actions| ProcessParentEndPlan {
                    process_id: process_id.to_string(),
                    actions: actions.clone(),
                })
        }))
}

pub(super) async fn complete(
    registry: &TestLocalProcessRegistry,
    process_id: &str,
) -> Result<(), PluginError> {
    let _transaction = registry.transaction.lock().await;
    if let Some(record) = registry.managed.lock().await.get_mut(process_id) {
        record.parent_end_actions = None;
    }
    Ok(())
}
