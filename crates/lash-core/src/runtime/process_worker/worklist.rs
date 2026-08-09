use std::num::NonZeroUsize;

use crate::{PluginError, ProcessRegistry, ProcessWorklistCursor, ProcessWorklistPage};

pub(super) const MAX_INTAKE_PAGE: usize = 256;
pub(super) const FETCH_ATTEMPTS: usize = 3;
const FETCH_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(10);

pub(super) async fn fetch_page_with_retry(
    registry: &dyn ProcessRegistry,
    limit: NonZeroUsize,
    continuation: Option<ProcessWorklistCursor>,
) -> Result<ProcessWorklistPage, PluginError> {
    let mut retry_after = FETCH_RETRY_BASE;
    for attempt in 1..=FETCH_ATTEMPTS {
        match registry
            .list_non_terminal_page(limit, continuation.clone())
            .await
        {
            Ok(page) => return Ok(page),
            Err(error) if attempt == FETCH_ATTEMPTS => return Err(error),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    attempt,
                    "retrying incomplete process worklist scan"
                );
                tokio::time::sleep(retry_after).await;
                retry_after = retry_after.saturating_mul(2);
            }
        }
    }
    unreachable!("the non-zero worklist retry budget returns from the loop")
}
