-- lash-postgres-store teardown, component version 138.
--
-- Generated artifact. These bytes are exactly the DDL a host applies to drop
-- everything this component owns at the reject-and-recreate boundary;
-- `PostgresStorage::teardown_ddl()` returns this file verbatim. Every
-- statement is idempotent (`IF EXISTS`), and `CASCADE` releases the intra-lash
-- foreign keys so table order carries no meaning. Indexes, constraints, and
-- seed rows die with their tables; schema.sql declares no standalone
-- sequences, types, or functions, so there is nothing else to drop.
--
-- Like schema.sql, nothing here is schema-qualified: the file tears down
-- whichever schema the session's `search_path` resolves. Regenerate it with
-- the schema_shape suite's LASH_UPDATE_TEARDOWN_SQL=1 path, never by hand.
--
DROP TABLE IF EXISTS lash_schema_versions CASCADE;

DROP TABLE IF EXISTS lash_migrations CASCADE;

DROP TABLE IF EXISTS lash_fleet_format CASCADE;

DROP TABLE IF EXISTS lash_blobs CASCADE;

DROP TABLE IF EXISTS lash_sessions CASCADE;

DROP TABLE IF EXISTS lash_node_anchors CASCADE;

DROP TABLE IF EXISTS lash_checkpoint_blob_refs CASCADE;

DROP TABLE IF EXISTS lash_deleted_sessions CASCADE;

DROP TABLE IF EXISTS lash_graph_nodes CASCADE;

DROP TABLE IF EXISTS lash_fork_lineage CASCADE;

DROP TABLE IF EXISTS lash_usage_deltas CASCADE;

DROP TABLE IF EXISTS lash_session_meta CASCADE;

DROP TABLE IF EXISTS lash_session_meta_pending_observer_intents CASCADE;

DROP TABLE IF EXISTS lash_runtime_turn_commits CASCADE;

DROP TABLE IF EXISTS lash_turn_cancel_requests CASCADE;

DROP TABLE IF EXISTS lash_turn_cancel_affected_inputs CASCADE;

DROP TABLE IF EXISTS lash_turn_cancellation_bindings CASCADE;

DROP TABLE IF EXISTS lash_queued_runs CASCADE;

DROP TABLE IF EXISTS lash_queued_run_members CASCADE;

DROP TABLE IF EXISTS lash_turn_cancel_closure_authorizations CASCADE;

DROP TABLE IF EXISTS lash_turn_cancel_retired_scopes CASCADE;

DROP TABLE IF EXISTS lash_turn_parks CASCADE;

DROP TABLE IF EXISTS lash_turn_park_clock CASCADE;

DROP TABLE IF EXISTS lash_turn_park_events CASCADE;

DROP TABLE IF EXISTS lash_session_execution_leases CASCADE;

DROP TABLE IF EXISTS lash_queued_work_batches CASCADE;

DROP TABLE IF EXISTS lash_queued_work_items CASCADE;

DROP TABLE IF EXISTS lash_wake_redelivery_fences CASCADE;

DROP TABLE IF EXISTS lash_pending_turn_inputs CASCADE;

DROP TABLE IF EXISTS lash_session_ingress CASCADE;

DROP TABLE IF EXISTS lash_attachment_manifest CASCADE;

DROP TABLE IF EXISTS lash_attachment_condemnations CASCADE;

DROP TABLE IF EXISTS lash_process_change_clock CASCADE;

DROP TABLE IF EXISTS lash_processes CASCADE;

DROP TABLE IF EXISTS lash_process_park_clock CASCADE;

DROP TABLE IF EXISTS lash_process_park_events CASCADE;

DROP TABLE IF EXISTS lash_process_events CASCADE;

DROP TABLE IF EXISTS lash_wake_allocation_floors CASCADE;

DROP TABLE IF EXISTS lash_process_wake_deliveries CASCADE;

DROP TABLE IF EXISTS lash_process_observers CASCADE;

DROP TABLE IF EXISTS lash_process_tombstones CASCADE;

DROP TABLE IF EXISTS lash_process_artifact_cleanup CASCADE;

DROP TABLE IF EXISTS lash_process_leases CASCADE;

DROP TABLE IF EXISTS lash_process_segment_handovers CASCADE;

DROP TABLE IF EXISTS lash_parent_end_plans CASCADE;

DROP TABLE IF EXISTS lash_tool_intent_submissions CASCADE;

DROP TABLE IF EXISTS lash_trigger_subscriptions CASCADE;

DROP TABLE IF EXISTS lash_process_definitions CASCADE;

DROP TABLE IF EXISTS lash_trigger_occurrences CASCADE;

DROP TABLE IF EXISTS lash_trigger_deliveries CASCADE;

DROP TABLE IF EXISTS lash_trigger_mutation_receipts CASCADE;

DROP TABLE IF EXISTS lash_lashlang_artifacts CASCADE;

DROP TABLE IF EXISTS lash_artifact_owners CASCADE;

DROP TABLE IF EXISTS lash_artifact_owner_retirements CASCADE;

DROP TABLE IF EXISTS lash_release_stamp CASCADE;

DROP TABLE IF EXISTS lash_catalog_identity CASCADE;
