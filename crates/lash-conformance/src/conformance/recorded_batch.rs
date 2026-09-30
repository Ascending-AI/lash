//! Durable witnesses shared by batch crash and cancellation laws.

use lash_sansio::sync::MutexExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
pub(crate) struct RecordedBatch {
    groups: Mutex<Vec<crate::RuntimeEffectGroup>>,
}

#[async_trait::async_trait]
impl crate::testing::EffectLayer for RecordedBatch {
    async fn open_effect_group(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        let recorded = group.clone();
        let opened = inner.open_effect_group(group).await?;
        let mut groups = self.groups.lock_recover();
        if !groups
            .iter()
            .any(|previous| previous.group_key() == recorded.group_key())
        {
            groups.push(recorded);
        }
        Ok(opened)
    }
}

impl RecordedBatch {
    pub(crate) fn layer(self: &Arc<Self>) -> Arc<dyn crate::testing::EffectLayer> {
        Arc::clone(self) as Arc<dyn crate::testing::EffectLayer>
    }

    pub(crate) fn group_keys(&self) -> Vec<String> {
        self.groups
            .lock_recover()
            .iter()
            .map(|group| group.group_key().to_owned())
            .collect()
    }

    /// Reads the recorded rank and payload, rather than a tool-body signal.
    #[expect(
        clippy::expect_used,
        reason = "conformance fixture reads its tier's durable records"
    )]
    pub(crate) async fn final_for(
        &self,
        host: &dyn crate::EffectHost,
        admitted: crate::AdmittedScope,
        call_id: &crate::ToolCallId,
        budget: Duration,
    ) {
        let scoped = host.scoped(admitted).expect("scope the durable final read");
        tokio::time::timeout(budget, async {
            loop {
                let groups = self.groups.lock_recover().clone();
                for group in groups {
                    let selected = group.children().iter().find(|child| {
                        matches!(&child.command, crate::RuntimeEffectCommand::ToolInvocation { request }
                            if &request.call.call_id == call_id)
                    });
                    let Some(selected) = selected else { continue };
                    for rank in 1..=group.children().len() as u64 {
                        if let Some(final_record) = scoped.controller()
                            .read_group_settlement(group.group_key(), rank).await
                            .expect("read a selected member's recorded final")
                            && final_record.child_replay_key == selected.invocation.effect_replay_key()
                        {
                            assert!(final_record.outcome.is_ok(), "the selected member recorded a successful final: {final_record:?}");
                            return;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("the selected member's final is durably recorded within the watchdog");
    }

    /// Whether each member of the law's one group recorded a successful
    /// durable final, in group position order: what every assembled batch
    /// row must say.
    #[expect(
        clippy::expect_used,
        reason = "conformance fixture reads its tier's durable records"
    )]
    pub(crate) async fn member_successes(
        &self,
        host: &dyn crate::EffectHost,
        admitted: crate::AdmittedScope,
    ) -> Vec<bool> {
        let scoped = host
            .scoped(admitted)
            .expect("scope the durable finals read");
        let groups = self.groups.lock_recover().clone();
        let [group] = groups.as_slice() else {
            panic!(
                "the law opened exactly one tool group, not {}",
                groups.len()
            );
        };
        let mut successes = vec![None; group.children().len()];
        for rank in 1..=group.children().len() as u64 {
            let final_record = scoped
                .controller()
                .read_group_settlement(group.group_key(), rank)
                .await
                .expect("reread a durable final")
                .expect("every batch member has a recorded final");
            let position = group
                .children()
                .iter()
                .position(|child| {
                    child.invocation.effect_replay_key() == final_record.child_replay_key
                })
                .expect("a recorded final names one of the group's members");
            successes[position] = Some(final_record.outcome.is_ok());
        }
        successes
            .into_iter()
            .map(|success| success.expect("every member position has a recorded final"))
            .collect()
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance fixture reads its tier's durable records"
    )]
    pub(crate) async fn finals(
        &self,
        host: &dyn crate::EffectHost,
        admitted: crate::AdmittedScope,
    ) -> serde_json::Value {
        let scoped = host
            .scoped(admitted)
            .expect("scope the durable finals read");
        let groups = self.groups.lock_recover().clone();
        let mut finals = Vec::new();
        assert!(!groups.is_empty(), "the law opened a tool group");
        for group in groups {
            for rank in 1..=group.children().len() as u64 {
                let final_record = scoped
                    .controller()
                    .read_group_settlement(group.group_key(), rank)
                    .await
                    .expect("reread a durable final")
                    .expect("every cancelled batch member has a recorded final");
                finals.push(serde_json::json!({
                    "group": group.group_key(), "rank": rank,
                    "sequence": final_record.sequence,
                    "child": final_record.child_replay_key,
                    "outcome": final_record.outcome,
                }));
            }
        }
        serde_json::Value::Array(finals)
    }
}
