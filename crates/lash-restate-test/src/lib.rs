//! `lash-restate-test`: an in-process Restate server double for testing
//! `lash-restate` fast and without sockets.
//!
//! It is a test double for the Restate *server*, not a second effect engine.
//! The code under test is the real thing: lash-restate's services bound on a
//! real `restate_sdk::endpoint::Endpoint`, driven through
//! `Endpoint::handle` by synthetic invocation streams, with the pinned
//! `restate-sdk-shared-core` VM running inside. What the double replaces is
//! `restate-server`: [`RestateTestServer`] keeps each invocation's journal,
//! acknowledges `ctx.run` results, suspends and resumes with a replayed
//! journal, fires durable timers on virtual time, serializes virtual-object
//! keys, runs workflows once per key with their durable promises, routes calls,
//! sends, signals and awakeables, cancels and kills, retries and pauses on the
//! handler retry policy, and can crash an attempt mid-step and replay it.
//!
//! Engine-agnostic laws must not depend on this crate's internals; they run
//! against lash's effect interface over a backend this crate builds.
//!
//! # Seeded choices and concurrent execution
//!
//! The double's own decisions are fully seeded; lash's concurrent handlers
//! race exactly as on a real server.
//!
//! Every choice the double makes derives from the seed and from what an
//! invocation *is* (the command that created it, its workflow or idempotency
//! key), never from creation order: invocation ids, random seeds, timer
//! tie-breaks and random crash points. Handlers run concurrently on Tokio,
//! with SQLite completing on its own threads, so when two invocations race —
//! a durable-wait registration and an index read, say — lash may take a
//! different path from one run to the next, as it would against
//! `restate-server`. Tests assert outcomes, not journal bytes. (lash also
//! journals some wall-clock values and fresh ids inside `ctx.run` results,
//! FIG-3672.)
//!

mod backend;
pub mod live;
mod open_handler;
pub mod protocol;
pub mod server;

pub use backend::{
    BackendError, HandlerAttempt, RestateTestBackend, SESSION_DRIVER_SERVICE, TURN_DRIVER_SERVICE,
    backend, backend_with, backend_with_build, backend_with_segment_budget,
};
pub use open_handler::OpenHandler;
pub use protocol::ProtocolVersion;
pub use server::{
    AttemptDispatch, CrashListener, CrashPoint, CrashRule, DeploymentHooks, DeploymentId,
    DropWatch, Hold, InvocationView, JournalEntryView, RandomCrashes, Refusal, RefuseHook,
    RemoveDeploymentError, RestateTestServer, ResumeDeployment, ResumeRefusal, RetryPolicy,
    ServedHook, ServerConfig, StartError, Stats, TimeMode, TimerView,
};
