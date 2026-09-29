//! The Restate journal read the failover batch's loss waits on: a batch
//! member's loss after commit exits only once its siblings' attempts are
//! journaled.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::restate_admin_url;

/// One row of a Restate `sys_journal` read.
#[derive(Deserialize)]
struct JournalEntryRow {
    id: String,
    index: u64,
    entry_type: String,
    #[serde(default)]
    name: Option<String>,
}

/// Wait until every other member of the batch `call_id` belongs to has its
/// attempt journaled: Restate holds a later command after the member's
/// attempt run, which its child appends only once the run's completion came
/// back. A worker lost before then runs that member again: an in-attempt
/// effect whose attempt outcome was never recorded is at-least-once (ADR
/// 0042, amended by ADR 0110). This loss is about the member that commits,
/// and must not also cut a sibling between its side effect and its
/// journaled outcome.
pub(crate) async fn await_batch_siblings_journaled(
    workflow_id: &str,
    call_id: &str,
    batch_width: u64,
) -> Result<()> {
    let (_, own_position) = call_id
        .rsplit_once(":child:")
        .with_context(|| format!("batch member call id `{call_id}` names its child position"))?;
    let own_attempt = format!(":child:{own_position}:attempt:");
    let siblings =
        usize::try_from(batch_width.saturating_sub(1)).context("a batch width fits in usize")?;
    let admin = lash_restate::RestateAdminClient::new(restate_admin_url());
    let query = format!(
        "SELECT id, index, entry_type, name FROM sys_journal WHERE id IN \
         (SELECT id FROM sys_invocation WHERE target_handler_name = 'child' \
          AND target_service_key LIKE '{workflow_id}:group:%') ORDER BY id, index"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let rows = admin
            .query_json::<JournalEntryRow>(&query)
            .await
            .context("read the batch children's Restate journals")?;
        let mut journaled = std::collections::BTreeSet::new();
        for attempt in rows.iter().filter(|row| {
            row.entry_type == "Command: Run"
                && row
                    .name
                    .as_deref()
                    .is_some_and(|name| name.contains(":attempt:") && !name.contains(&own_attempt))
        }) {
            if rows.iter().any(|later| {
                later.id == attempt.id
                    && later.index > attempt.index
                    && later.entry_type.starts_with("Command:")
            }) {
                journaled.insert(attempt.id.clone());
            }
        }
        if journaled.len() >= siblings {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{} of the {siblings} siblings of `{call_id}` journaled their attempts within 60s",
                journaled.len(),
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
