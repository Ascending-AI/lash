//! Replay after prune or advance, for every effect family (FIG-4324, ADR 0105
//! §1).
//!
//! A path that can run more than once for one durable effect decides what it
//! records or returns from recorded results: a Restate handler's replay reads
//! its journal, and a redelivered durable identity reads the admission its
//! first delivery recorded. Neither may read today's mutable store state and
//! decide again. Each law here records one durable operation, moves the store
//! on (its target pruned and compacted, its session deleted, its wait resolved
//! again, its session advanced by later commands), and requires the recorded
//! outcome back with no new registry row and no new invocation.
//!
//! Legs run on the Restate server double over SQLite memory, SQLite file and
//! PostgreSQL (`LASH_POSTGRES_DATABASE_URL`), and on a live `restate-server`
//! for the families whose operations run in a handler (the `raa-live` suite
//! in `scripts/restate-suites.toml`).

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions; a failed expectation is the test failure"
)]
#![allow(
    clippy::disallowed_methods,
    reason = "service legs read the gate's environment"
)]

#[path = "replay_after_advance/families.rs"]
mod families;
#[path = "replay_after_advance/harness.rs"]
mod harness;

use families::{Advance, Surface};
use harness::StorageKind;

/// One module per backend: the double over each storage kind, and live
/// Restate over SQLite memory. `$pg` and `$live` name the reason a leg is
/// held for its service.
macro_rules! backend_laws {
    ($name:ident, $kind:ident, $live:expr $(, $service:literal)?) => {
        mod $name {
            use super::*;

            #[tokio::test]
            $(#[ignore = $service])?
            async fn process_start_replay_after_prune_returns_recorded_receipt() {
                families::process_start(StorageKind::$kind, $live, Advance::PruneAndCompact).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn process_start_replay_after_session_delete_returns_recorded_receipt() {
                families::process_start(StorageKind::$kind, $live, Advance::DeleteSession).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn trigger_emit_replay_after_delivery_prune_returns_recorded_report() {
                families::trigger_emit(StorageKind::$kind, $live).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn session_signal_replay_after_prune_returns_recorded_event() {
                families::signal(StorageKind::$kind, $live, Surface::Session, Advance::Prune).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn session_cancel_replay_after_prune_returns_recorded_receipt() {
                families::cancel(StorageKind::$kind, $live, Surface::Session, Advance::Prune).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn session_signal_replay_after_compaction_returns_recorded_event() {
                families::signal(
                    StorageKind::$kind,
                    $live,
                    Surface::Session,
                    Advance::PruneAndCompact,
                )
                .await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn facade_cancel_all_replay_after_compaction_returns_recorded_receipts() {
                families::cancel_all(StorageKind::$kind, $live).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn external_completion_replay_after_compaction_returns_recorded_outcome() {
                families::external_completion(StorageKind::$kind, $live, false).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn external_completion_replay_after_observer_transfer_returns_recorded_outcome() {
                families::external_completion(StorageKind::$kind, $live, true).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn facade_signal_replay_after_compaction_returns_recorded_event() {
                families::signal(
                    StorageKind::$kind,
                    $live,
                    Surface::Facade,
                    Advance::PruneAndCompact,
                )
                .await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn facade_cancel_replay_after_compaction_returns_recorded_receipt() {
                families::cancel(
                    StorageKind::$kind,
                    $live,
                    Surface::Facade,
                    Advance::PruneAndCompact,
                )
                .await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn process_await_replay_after_prune_returns_recorded_output() {
                families::attach_await(StorageKind::$kind, $live).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn durable_wait_replay_after_a_second_resolution_returns_recorded_resolution() {
                families::durable_wait(StorageKind::$kind, $live).await;
            }
        }
    };
}

/// The duplicate-delivery families: a redelivery arrives with no journal, so
/// they run on the double only. The tool-intent family's redelivery laws run
/// in the ingress unit tests (`crates/lash/src/tests/tool_intent_ingress/
/// replay_after_advance.rs`): the ingress on Restate runs only inside a
/// handler scope.
macro_rules! duplicate_laws {
    ($name:ident, $kind:ident $(, $service:literal)?) => {
        mod $name {
            use super::*;

            #[tokio::test]
            #[ignore = "FIG-4352: a settled command's batch is deleted, so its resubmission applies again"]
            async fn session_command_resubmission_after_advance_returns_first_receipt() {
                families::session_command(StorageKind::$kind).await;
            }

            #[tokio::test]
            $(#[ignore = $service])?
            async fn session_cancel_of_a_pruned_target_still_refuses() {
                families::fresh_command_refuses_a_pruned_target(StorageKind::$kind, Surface::Session)
                    .await;
            }
        }
    };
}

backend_laws!(double_sqlite_memory, Memory, false);
backend_laws!(double_sqlite_file, File, false);
backend_laws!(double_postgres, Postgres, false, "requires PostgreSQL");
backend_laws!(live_sqlite_memory, Memory, true, "requires live Restate");

duplicate_laws!(duplicate_sqlite_memory, Memory);
duplicate_laws!(duplicate_sqlite_file, File);
duplicate_laws!(duplicate_postgres, Postgres, "requires PostgreSQL");
