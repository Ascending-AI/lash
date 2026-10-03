//! K0 compile witness (FIG-4867): the SDK surface the tool end state
//! builds on, named only through Lash's `restate_sdk` re-export. Hosts and
//! Lash share one `Endpoint` type and one handler context; a journaled
//! attempt is a named `run` with a recorded retry policy. FIG-4870 moves the
//! re-export to the concurrent-run fork and keeps this witness compiling;
//! eager consuming starts arrive with that revision.

use crate::restate_sdk::context::{ContextSideEffects, RunFuture, RunRetryPolicy};
use crate::restate_sdk::endpoint::{Builder, Endpoint};
use crate::restate_sdk::errors::{HandlerError, TerminalError};
use crate::restate_sdk::serde::Json;

/// One recorded attempt, as a Run journals it: named after its replay key
/// and bounded by its recorded retry policy.
#[expect(
    dead_code,
    reason = "a compile witness: the signature is the contract, nothing calls it"
)]
fn recorded_attempt<'ctx, C: ContextSideEffects<'ctx>>(
    ctx: &C,
    replay_key: String,
    retry: RunRetryPolicy,
) -> impl RunFuture<Result<Json<u64>, TerminalError>> + 'ctx {
    ctx.run(|| async { Ok::<_, HandlerError>(Json(1_u64)) })
        .name(replay_key)
        .retry_policy(retry)
}

#[test]
fn hosts_and_lash_name_one_endpoint_through_the_reexport() {
    let builder: fn() -> Builder = Endpoint::builder;
    let endpoint: Endpoint = builder().build();
    drop(endpoint);
}
