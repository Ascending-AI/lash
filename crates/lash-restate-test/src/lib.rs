//! `lash-restate-test`: an in-process Restate server double for testing
//! `lash-restate` fast, deterministically and without sockets.
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

mod backend;
pub mod protocol;
pub mod server;

pub use backend::{BackendError, HandlerAttempt, RestateTestBackend, backend};
pub use protocol::ProtocolVersion;
pub use server::{
    CrashPoint, CrashRule, InvocationView, JournalEntryView, RandomCrashes, RestateTestServer,
    RetryPolicy, ServerConfig, StartError, Stats, TimeMode, TimerView,
};
