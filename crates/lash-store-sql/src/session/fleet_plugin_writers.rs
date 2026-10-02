//! `fleet_plugin_writers`: the fleet record's per-plugin writer ranges, beside
//! `fleet_format` (FIG-4746).
//!
//! One row per plugin id records the format versions the fleet permits that
//! plugin's state and config namespaces to be published in. Only PostgreSQL
//! keeps the ranges in this table: its rows are read and written under the
//! `fleet_format` row's lock, a writer's share lock or finalize's update
//! lock, so a range never moves under a transaction that was admitted
//! against it. SQLite keeps the same ranges beside its `lash_compat` row.

/// The table's unprefixed name.
pub const TABLE: &str = "fleet_plugin_writers";
