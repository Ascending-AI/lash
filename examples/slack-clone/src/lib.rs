//! `slack-clone` — a Slack-shaped chat platform and a Lash bot living inside it.
//!
//! Every other example in this repository is a host that *owns* its UI: Lash is
//! the product. This one is inverted, which is the shape most real integrations
//! have. Two binaries, one crate:
//!
//! * [`platform`] — a multiplayer chat product with **no Lash dependency**,
//!   exposing a Slack-compatible Web API and Events API. It stands in for
//!   someone else's product.
//! * [`bot`] — the reference Lash embedding. A **standard-mode** host (native
//!   tool loop, plain chat turns) that reaches the platform only over HTTP, keeps
//!   one durable session per channel, folds ambient room traffic in as queued
//!   turn input, and answers when mentioned.
//!
//! The three shared modules are the seam between them: [`wire`] is the Slack
//! contract both sides speak, [`ids`] mints Slack-shaped identifiers and message
//! timestamps, and [`store`] is the tiny SQLite helper each side uses for its own
//! durable state.
//!
//! See `README.md` for the Slack-fidelity statement, the session-mapping
//! doctrine, and the migration notes for pointing the bot at real Slack.

pub mod bot;
pub mod ids;
#[cfg(feature = "live-e2e")]
pub mod live_e2e;
pub mod mcp_server;
pub mod platform;
pub mod secrets;
pub mod store;
pub mod wire;

#[cfg(test)]
mod tests;
