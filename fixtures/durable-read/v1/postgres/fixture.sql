--
-- PostgreSQL database dump
--


-- Dumped from database version 16.15
-- Dumped by pg_dump version 16.15

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: lash_durable_read_fixture; Type: SCHEMA; Schema: -; Owner: -
--

CREATE SCHEMA lash_durable_read_fixture;


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: lash_artifact_owner_retirements; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_artifact_owner_retirements (
    owner_kind text NOT NULL,
    owner_id text NOT NULL,
    CONSTRAINT ck_artifact_owner_retirements_owner_kind CHECK ((owner_kind = 'execution'::text))
);


--
-- Name: lash_artifact_owners; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_artifact_owners (
    namespace text NOT NULL,
    artifact_ref text NOT NULL,
    owner_kind text NOT NULL,
    owner_id text NOT NULL,
    CONSTRAINT ck_artifact_owners_owner_kind CHECK ((owner_kind = ANY (ARRAY['host'::text, 'process'::text, 'execution'::text])))
);


--
-- Name: lash_attachment_condemnations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_condemnations (
    attachment_id text NOT NULL,
    phase text NOT NULL,
    write_token text,
    write_session_id text,
    CONSTRAINT ck_attachment_condemnations_phase CHECK ((phase = ANY (ARRAY['condemned'::text, 'deleting'::text]))),
    CONSTRAINT ck_attachment_condemnations_write_token_pairing CHECK (((write_token IS NULL) = (write_session_id IS NULL))),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (((write_token IS NULL) OR (phase = 'condemned'::text)))
);


--
-- Name: lash_attachment_manifest; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_manifest (
    attachment_id text NOT NULL,
    session_id text NOT NULL,
    canonical_uri text NOT NULL,
    intent_at_ms bigint NOT NULL,
    write_id text,
    written_at_ms bigint,
    committed_at_ms bigint,
    owner_kind text,
    owner_id text,
    CONSTRAINT ck_attachment_manifest_owner_kind CHECK ((owner_kind = ANY (ARRAY['turn'::text, 'process'::text]))),
    CONSTRAINT ck_lash_attachment_manifest_owner_identity CHECK ((((owner_kind IS NULL) AND (owner_id IS NULL)) OR ((owner_kind = ANY (ARRAY['turn'::text, 'process'::text])) AND (owner_id IS NOT NULL))))
);


--
-- Name: lash_blobs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_blobs (
    hash text NOT NULL,
    content bytea NOT NULL
);


--
-- Name: lash_catalog_identity; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_catalog_identity (
    singleton boolean DEFAULT true NOT NULL,
    catalog_id text NOT NULL,
    CONSTRAINT ck_catalog_identity_singleton CHECK (singleton)
);


--
-- Name: lash_checkpoint_blob_refs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_checkpoint_blob_refs (
    checkpoint_ref text NOT NULL,
    blob_ref text NOT NULL
);


--
-- Name: lash_control_intents; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_control_intents (
    intent_id bigint NOT NULL,
    session_id text NOT NULL,
    format bigint NOT NULL,
    kind text NOT NULL,
    kind_json text NOT NULL,
    state text NOT NULL,
    state_json text NOT NULL,
    attempts bigint NOT NULL,
    created_at_ms bigint NOT NULL,
    engine_ref text,
    CONSTRAINT ck_control_intents_kind CHECK ((kind = ANY (ARRAY['redrive'::text, 'cancel'::text, 'fork'::text, 'close_session'::text]))),
    CONSTRAINT ck_control_intents_state CHECK ((state = ANY (ARRAY['pending'::text, 'acknowledged'::text, 'superseded'::text, 'failed_retryable'::text, 'failed'::text])))
);


--
-- Name: lash_control_intents_intent_id_seq; Type: SEQUENCE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE SEQUENCE lash_durable_read_fixture.lash_control_intents_intent_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: lash_control_intents_intent_id_seq; Type: SEQUENCE OWNED BY; Schema: lash_durable_read_fixture; Owner: -
--

ALTER SEQUENCE lash_durable_read_fixture.lash_control_intents_intent_id_seq OWNED BY lash_durable_read_fixture.lash_control_intents.intent_id;


--
-- Name: lash_deleted_sessions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_deleted_sessions (
    session_id text NOT NULL,
    created_at_ms bigint,
    last_commit_at_ms bigint,
    head_revision bigint,
    relation_kind text,
    parent_session_id text
);


--
-- Name: lash_fleet_format; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_fleet_format (
    singleton boolean DEFAULT true NOT NULL,
    format_version integer NOT NULL,
    CONSTRAINT ck_fleet_format_singleton CHECK (singleton)
);


--
-- Name: lash_fork_lineage; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_fork_lineage (
    session_id text NOT NULL,
    ancestor_session_id text NOT NULL,
    fork_node_id text NOT NULL,
    fork_generation bigint NOT NULL,
    CONSTRAINT ck_fork_lineage_fork_generation CHECK ((fork_generation >= 0))
);


--
-- Name: lash_graph_nodes; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_graph_nodes (
    session_id text NOT NULL,
    node_id text NOT NULL,
    parent_node_id text,
    generation bigint NOT NULL,
    frame_node_id text NOT NULL,
    node_json text NOT NULL,
    tombstoned boolean DEFAULT false NOT NULL,
    CONSTRAINT ck_graph_nodes_generation CHECK ((generation >= 0))
);


--
-- Name: lash_lashlang_artifacts; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_lashlang_artifacts (
    namespace text NOT NULL,
    artifact_ref text NOT NULL,
    artifact_bytes bytea NOT NULL
);


--
-- Name: lash_migrations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_migrations (
    phase text NOT NULL,
    migration text NOT NULL,
    release text NOT NULL,
    state text NOT NULL,
    from_version integer,
    to_version integer NOT NULL,
    started_at_ms bigint NOT NULL,
    finished_at_ms bigint,
    CONSTRAINT ck_lash_migrations_phase CHECK ((phase = ANY (ARRAY['expand'::text, 'backfill'::text, 'contract'::text]))),
    CONSTRAINT ck_lash_migrations_state CHECK ((state = ANY (ARRAY['running'::text, 'applied'::text])))
);


--
-- Name: lash_node_anchors; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_node_anchors (
    node_id text NOT NULL,
    checkpoint_ref text NOT NULL,
    source_session_id text NOT NULL
);


--
-- Name: lash_parent_end_plans; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_parent_end_plans (
    parent_kind text NOT NULL,
    parent_id text NOT NULL COLLATE pg_catalog."C",
    parent_payload text NOT NULL,
    ended_at_ms bigint NOT NULL,
    settled_at_ms bigint,
    CONSTRAINT ck_parent_end_plans_kind CHECK ((parent_kind = ANY (ARRAY['turn'::text, 'queue_drain'::text, 'process'::text, 'session'::text])))
);


--
-- Name: lash_pending_turn_inputs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_pending_turn_inputs (
    enqueue_seq bigint NOT NULL,
    input_id text NOT NULL,
    session_id text NOT NULL,
    source_key text,
    ingress_json text NOT NULL,
    state text NOT NULL,
    input_json text NOT NULL,
    submitted_ingress_json text NOT NULL,
    submission_digest text NOT NULL,
    enqueued_at_ms bigint NOT NULL,
    claim_id text,
    claim_owner_id text,
    claim_owner_incarnation_id text,
    claim_token text,
    claim_fencing_token bigint DEFAULT 0 NOT NULL,
    claim_session_lease_generation bigint DEFAULT 0 NOT NULL,
    claim_bound_turn_id text,
    claim_bound_receipt_input_id text,
    CONSTRAINT ck_pending_turn_inputs_bound_claim_is_next_turn CHECK ((((claim_bound_turn_id IS NULL) AND (claim_bound_receipt_input_id IS NULL)) OR ((claim_bound_turn_id IS NOT NULL) AND (claim_bound_receipt_input_id IS NOT NULL) AND (claim_token IS NOT NULL) AND (state = 'deferred_next_turn'::text)))),
    CONSTRAINT ck_pending_turn_inputs_claim_identity_all_or_none CHECK ((((claim_id IS NULL) AND (claim_owner_id IS NULL) AND (claim_owner_incarnation_id IS NULL) AND (claim_token IS NULL)) OR ((claim_id IS NOT NULL) AND (claim_owner_id IS NOT NULL) AND (claim_owner_incarnation_id IS NOT NULL) AND (claim_token IS NOT NULL)))),
    CONSTRAINT ck_pending_turn_inputs_state CHECK ((state = ANY (ARRAY['pending_active'::text, 'deferred_next_turn'::text, 'accepted'::text, 'cancelled'::text, 'completed'::text]))),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK ((((((ingress_json)::jsonb ->> 'scope'::text) = 'active_turn'::text) AND (state = ANY (ARRAY['pending_active'::text, 'accepted'::text, 'cancelled'::text, 'completed'::text]))) OR ((((ingress_json)::jsonb ->> 'scope'::text) = 'next_turn'::text) AND (state = ANY (ARRAY['deferred_next_turn'::text, 'cancelled'::text, 'completed'::text])))))
);


--
-- Name: lash_process_artifact_cleanup; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_artifact_cleanup (
    process_id text NOT NULL COLLATE pg_catalog."C",
    cleanup_json text NOT NULL
);


--
-- Name: lash_process_change_clock; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_change_clock (
    singleton boolean DEFAULT true NOT NULL,
    current_seq bigint NOT NULL,
    tombstone_compaction_horizon bigint DEFAULT 0 NOT NULL,
    CONSTRAINT ck_process_change_clock_singleton CHECK (singleton)
);


--
-- Name: lash_process_definitions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_definitions (
    definition_id text NOT NULL,
    owner_scope text NOT NULL,
    name text NOT NULL,
    revision bigint NOT NULL,
    fingerprint text NOT NULL,
    lifecycle text NOT NULL,
    deleted_at_ms bigint,
    change_seq bigint NOT NULL,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    record_json text NOT NULL,
    CONSTRAINT ck_process_definitions_lifecycle CHECK ((((lifecycle = ANY (ARRAY['enabled'::text, 'disabled'::text])) AND (deleted_at_ms IS NULL)) OR ((lifecycle = 'tombstoned'::text) AND (deleted_at_ms IS NOT NULL))))
);


--
-- Name: lash_process_events; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_events (
    process_id text NOT NULL COLLATE pg_catalog."C",
    sequence bigint NOT NULL,
    event_type text NOT NULL,
    idempotency_key text,
    event_json text NOT NULL
);


--
-- Name: lash_process_leases; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_leases (
    process_id text NOT NULL COLLATE pg_catalog."C",
    lease_owner_id text,
    lease_owner_incarnation_id text,
    lease_token text,
    lease_fencing_token bigint DEFAULT 0 NOT NULL,
    lease_claimed_at_ms bigint DEFAULT 0 NOT NULL,
    lease_expires_at_ms bigint DEFAULT 0 NOT NULL
);


--
-- Name: lash_process_observers; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_observers (
    session_id text NOT NULL,
    process_id text NOT NULL COLLATE pg_catalog."C"
);


--
-- Name: lash_process_park_clock; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_park_clock (
    singleton boolean DEFAULT true NOT NULL,
    current_seq bigint DEFAULT 0 NOT NULL,
    compaction_horizon bigint DEFAULT 0 NOT NULL,
    CONSTRAINT ck_process_park_clock_singleton CHECK (singleton)
);


--
-- Name: lash_process_park_events; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_park_events (
    seq bigint NOT NULL,
    process_id text NOT NULL COLLATE pg_catalog."C",
    park_id bigint NOT NULL,
    kind text NOT NULL,
    cause text,
    reason_json text,
    at_ms bigint NOT NULL,
    CONSTRAINT ck_process_park_events_kind CHECK ((kind = ANY (ARRAY['parked'::text, 'unparked'::text, 'cancelled'::text]))),
    CONSTRAINT ck_process_park_events_parked_reason CHECK ((((kind = 'parked'::text) AND (reason_json IS NOT NULL) AND (cause IS NULL)) OR ((kind <> 'parked'::text) AND (reason_json IS NULL) AND (cause IS NOT NULL))))
);


--
-- Name: lash_process_segment_handovers; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_segment_handovers (
    process_id text NOT NULL COLLATE pg_catalog."C",
    segment_ordinal bigint NOT NULL,
    handover_json text NOT NULL,
    started_json text
);


--
-- Name: lash_process_tombstones; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_tombstones (
    process_id text NOT NULL COLLATE pg_catalog."C",
    terminal_label text NOT NULL,
    pruned_at_ms bigint NOT NULL,
    pruned_change_seq bigint NOT NULL
);


--
-- Name: lash_process_wake_deliveries; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_wake_deliveries (
    delivery_id text NOT NULL,
    process_id text NOT NULL COLLATE pg_catalog."C",
    target_session_id text NOT NULL,
    sequence bigint NOT NULL,
    state text NOT NULL,
    claim_token text,
    attempts bigint DEFAULT 0 NOT NULL,
    first_attempt_ms bigint,
    next_attempt_at_ms bigint NOT NULL,
    expires_at_ms bigint NOT NULL,
    discard_reason text,
    delivery_json text NOT NULL,
    CONSTRAINT ck_process_wake_deliveries_discard_reason CHECK ((discard_reason = ANY (ARRAY['expired'::text, 'target_gone'::text, 'retargeted'::text, 'sequence_rewound'::text]))),
    CONSTRAINT ck_process_wake_deliveries_state CHECK ((state = ANY (ARRAY['pending'::text, 'enqueuing'::text, 'enqueued'::text, 'discarded'::text])))
);


--
-- Name: lash_processes; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_processes (
    process_id text NOT NULL COLLATE pg_catalog."C",
    start_key text COLLATE pg_catalog."C",
    originator_id text NOT NULL,
    wake_session_id text,
    identity_kind text NOT NULL,
    identity_label text,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    last_event_sequence bigint NOT NULL,
    change_seq bigint NOT NULL,
    status text NOT NULL,
    lifetime text NOT NULL,
    lifetime_scope_kind text,
    lifetime_scope_id text COLLATE pg_catalog."C",
    cancel_requested_at_ms bigint,
    parked_since_ms bigint,
    parked_reason_code text,
    park_executable_generation text,
    record_json text NOT NULL,
    CONSTRAINT ck_processes_lifetime CHECK ((lifetime = ANY (ARRAY['until'::text, 'detached'::text]))),
    CONSTRAINT ck_processes_lifetime_scope CHECK ((((lifetime = 'detached'::text) AND (lifetime_scope_kind IS NULL) AND (lifetime_scope_id IS NULL)) OR ((lifetime = 'until'::text) AND (lifetime_scope_kind = ANY (ARRAY['turn'::text, 'queue_drain'::text, 'process'::text, 'session'::text])) AND (lifetime_scope_id IS NOT NULL)))),
    CONSTRAINT ck_processes_parked CHECK (((parked_since_ms IS NULL) = (parked_reason_code IS NULL))),
    CONSTRAINT ck_processes_status CHECK ((status = ANY (ARRAY['running'::text, 'waiting'::text, 'completed'::text, 'failed'::text, 'cancelled'::text, 'abandoned'::text, 'caller_departed'::text])))
);


--
-- Name: lash_queued_run_members; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_queued_run_members (
    session_id text NOT NULL,
    scope_id text NOT NULL,
    collection_kind text NOT NULL,
    ordinal bigint NOT NULL,
    member_kind text NOT NULL,
    member_id text NOT NULL,
    CONSTRAINT ck_queued_run_members_collection_kind CHECK ((collection_kind = ANY (ARRAY['initial'::text, 'current'::text, 'withheld'::text, 'assigned'::text]))),
    CONSTRAINT ck_queued_run_members_member_kind CHECK ((member_kind = ANY (ARRAY['input'::text, 'batch'::text]))),
    CONSTRAINT ck_queued_run_members_ordinal CHECK ((ordinal >= 0))
);


--
-- Name: lash_queued_runs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_queued_runs (
    session_id text NOT NULL,
    scope_id text NOT NULL,
    status text NOT NULL,
    revision bigint NOT NULL,
    admission_json text NOT NULL,
    CONSTRAINT ck_queued_runs_revision CHECK ((revision >= 0)),
    CONSTRAINT ck_queued_runs_status CHECK ((status = ANY (ARRAY['pending'::text, 'settled'::text])))
);


--
-- Name: lash_queued_work_batches; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_queued_work_batches (
    enqueue_seq bigint NOT NULL,
    batch_id text NOT NULL,
    session_id text NOT NULL,
    source_key text,
    delivery_policy text NOT NULL,
    work_kind text NOT NULL,
    authority_json text NOT NULL,
    merge_key text,
    available_at_ms bigint NOT NULL,
    enqueued_at_ms bigint NOT NULL,
    claim_id text,
    claim_token text,
    claim_fencing_token bigint DEFAULT 0 NOT NULL,
    claim_session_lease_generation bigint DEFAULT 0 NOT NULL,
    CONSTRAINT ck_queued_work_batches_claim_id_token_all_or_none CHECK ((((claim_id IS NULL) AND (claim_token IS NULL)) OR ((claim_id IS NOT NULL) AND (claim_token IS NOT NULL)))),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK ((delivery_policy = ANY (ARRAY['earliest_safe_boundary'::text, 'after_current_turn_commit'::text]))),
    CONSTRAINT ck_queued_work_batches_work_kind CHECK ((work_kind = ANY (ARRAY['turn'::text, 'control'::text])))
);


--
-- Name: lash_queued_work_items; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_queued_work_items (
    batch_id text NOT NULL,
    item_index integer NOT NULL,
    item_id text NOT NULL,
    payload_json text NOT NULL
);


--
-- Name: lash_release_stamp; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_release_stamp (
    singleton boolean DEFAULT true NOT NULL,
    release_version text NOT NULL,
    schema_versions text NOT NULL,
    written_at_epoch_ms bigint NOT NULL,
    CONSTRAINT ck_release_stamp_singleton CHECK (singleton)
);


--
-- Name: lash_runtime_turn_commits; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_runtime_turn_commits (
    session_id text NOT NULL,
    turn_id text NOT NULL,
    turn_commit_hash text NOT NULL,
    result_json text NOT NULL,
    committed_at_ms bigint NOT NULL,
    request_identity_hash text,
    requested_node_count bigint,
    identity_encoding_version integer,
    CONSTRAINT ck_runtime_turn_commits_identity CHECK ((((request_identity_hash IS NULL) = (identity_encoding_version IS NULL)) AND ((requested_node_count IS NULL) OR (request_identity_hash IS NOT NULL))))
);


--
-- Name: lash_schema_versions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_schema_versions (
    component text NOT NULL,
    version integer NOT NULL
);


--
-- Name: lash_session_execution_leases; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_execution_leases (
    session_id text NOT NULL,
    lease_owner_id text,
    lease_owner_incarnation_id text,
    lease_executor_id text,
    lease_token text,
    lease_fencing_token bigint DEFAULT 0 NOT NULL,
    lease_claimed_at_ms bigint DEFAULT 0 NOT NULL,
    lease_term_ms bigint DEFAULT 0 NOT NULL,
    lease_expires_at_ms bigint DEFAULT 0 NOT NULL,
    CONSTRAINT ck_session_execution_leases_identity_all_or_none CHECK ((((lease_owner_id IS NULL) AND (lease_owner_incarnation_id IS NULL) AND (lease_executor_id IS NULL) AND (lease_token IS NULL)) OR ((lease_owner_id IS NOT NULL) AND (lease_owner_incarnation_id IS NOT NULL) AND (lease_executor_id IS NOT NULL) AND (lease_token IS NOT NULL))))
);


--
-- Name: lash_session_ingress; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_ingress (
    enqueue_seq bigint NOT NULL,
    item_id text NOT NULL,
    session_id text NOT NULL,
    lane text NOT NULL,
    kind text NOT NULL,
    source_key text,
    delivery_scope text NOT NULL,
    delivery_turn_id text,
    delivery_min_boundary text,
    submission_digest text NOT NULL,
    payload_json text NOT NULL,
    authority_json text,
    merge_key text,
    wake_process_id text,
    wake_sequence bigint,
    state text NOT NULL,
    terminal_cause_json text,
    enqueued_at_ms bigint NOT NULL,
    terminal_at_ms bigint,
    claim_id text,
    claim_token text,
    claim_admission_id text,
    claim_fencing_token bigint DEFAULT 0 NOT NULL,
    claim_drive_epoch bigint,
    claim_turn_id text,
    CONSTRAINT ck_session_ingress_claim CHECK ((((state = 'accepted'::text) AND (claim_id IS NOT NULL) AND (claim_token IS NOT NULL) AND (claim_admission_id IS NOT NULL) AND (claim_drive_epoch IS NOT NULL)) OR ((state <> 'accepted'::text) AND (claim_id IS NULL) AND (claim_token IS NULL) AND (claim_admission_id IS NULL) AND (claim_drive_epoch IS NULL) AND (claim_turn_id IS NULL)))),
    CONSTRAINT ck_session_ingress_delivery CHECK ((((delivery_scope = 'turn'::text) AND (delivery_turn_id IS NOT NULL) AND (delivery_min_boundary = ANY (ARRAY['after_work'::text, 'before_completion'::text]))) OR ((delivery_scope = ANY (ARRAY['any_boundary'::text, 'next_turn'::text])) AND (delivery_turn_id IS NULL) AND (delivery_min_boundary IS NULL)))),
    CONSTRAINT ck_session_ingress_kind CHECK ((kind = ANY (ARRAY['input'::text, 'process_wake'::text, 'session_command'::text]))),
    CONSTRAINT ck_session_ingress_kind_delivery CHECK (((kind = 'input'::text) OR ((kind = 'process_wake'::text) AND (delivery_scope = 'any_boundary'::text)) OR ((kind = 'session_command'::text) AND (delivery_scope = 'next_turn'::text)))),
    CONSTRAINT ck_session_ingress_lane CHECK ((((kind = 'session_command'::text) AND (lane = 'command'::text)) OR ((kind = ANY (ARRAY['input'::text, 'process_wake'::text])) AND (lane = 'turn'::text)))),
    CONSTRAINT ck_session_ingress_state CHECK ((state = ANY (ARRAY['open'::text, 'accepted'::text, 'completed'::text, 'cancelled'::text]))),
    CONSTRAINT ck_session_ingress_terminal CHECK ((((state = ANY (ARRAY['completed'::text, 'cancelled'::text])) AND (terminal_cause_json IS NOT NULL) AND (terminal_at_ms IS NOT NULL)) OR ((state = ANY (ARRAY['open'::text, 'accepted'::text])) AND (terminal_cause_json IS NULL) AND (terminal_at_ms IS NULL)))),
    CONSTRAINT ck_session_ingress_wake_source CHECK ((((kind = 'process_wake'::text) AND (wake_process_id IS NOT NULL) AND (wake_sequence IS NOT NULL)) OR ((kind <> 'process_wake'::text) AND (wake_process_id IS NULL) AND (wake_sequence IS NULL))))
);


--
-- Name: lash_session_ingress_sequence; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_ingress_sequence (
    session_id text NOT NULL,
    enqueue_seq bigint NOT NULL,
    CONSTRAINT ck_session_ingress_sequence_positive CHECK ((enqueue_seq > 0))
);


--
-- Name: lash_session_meta; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_meta (
    session_id text NOT NULL,
    session_state_version integer,
    created_at_ms bigint,
    last_commit_at_ms bigint,
    relation_kind text NOT NULL,
    parent_session_id text,
    caused_by_kind text,
    caused_by_session_id text,
    caused_by_turn_id text,
    caused_by_effect_id text,
    caused_by_call_id text,
    caused_by_process_id text,
    caused_by_process_event_sequence text,
    caused_by_occurrence_id text,
    caused_by_subscription_id text,
    caused_by_subscription_incarnation text,
    caused_by_subscription_revision text,
    caused_by_node_id text,
    source_session_id text,
    source_node_id text,
    drive_epoch bigint DEFAULT 0 NOT NULL,
    drive_admission_id text,
    drive_root_start text,
    admission_base_checkpoint_ref text,
    closing_intent bigint,
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((((caused_by_kind IS NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'turn'::text) AND (caused_by_session_id IS NOT NULL) AND (caused_by_turn_id IS NOT NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'effect_address'::text) AND (caused_by_effect_id IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'tool_call'::text) AND (caused_by_session_id IS NOT NULL) AND (caused_by_call_id IS NOT NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'process'::text) AND (caused_by_process_id IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'process_event'::text) AND (caused_by_process_id IS NOT NULL) AND (caused_by_process_event_sequence IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'trigger_occurrence'::text) AND (caused_by_occurrence_id IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'session_node'::text) AND (caused_by_session_id IS NOT NULL) AND (caused_by_node_id IS NOT NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL)) OR ((caused_by_kind IS NOT NULL) AND (NOT (caused_by_kind = ANY (ARRAY['turn'::text, 'effect_address'::text, 'tool_call'::text, 'process'::text, 'process_event'::text, 'trigger_occurrence'::text, 'session_node'::text])))))),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK ((caused_by_kind = ANY (ARRAY['turn'::text, 'effect_address'::text, 'tool_call'::text, 'process'::text, 'process_event'::text, 'trigger_occurrence'::text, 'session_node'::text]))),
    CONSTRAINT ck_session_meta_relation_family CHECK ((((relation_kind = 'root'::text) AND (parent_session_id IS NULL) AND (caused_by_kind IS NULL) AND (source_session_id IS NULL) AND (source_node_id IS NULL)) OR ((relation_kind = 'child'::text) AND (parent_session_id IS NOT NULL) AND (source_session_id IS NULL) AND (source_node_id IS NULL)) OR ((relation_kind = 'fork'::text) AND (parent_session_id IS NULL) AND (caused_by_kind IS NULL) AND (source_session_id IS NOT NULL) AND (source_node_id IS NOT NULL)) OR ((relation_kind IS NOT NULL) AND (NOT (relation_kind = ANY (ARRAY['root'::text, 'child'::text, 'fork'::text])))))),
    CONSTRAINT ck_session_meta_relation_kind CHECK ((relation_kind = ANY (ARRAY['root'::text, 'child'::text, 'fork'::text])))
);


--
-- Name: lash_session_meta_pending_observer_intents; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_meta_pending_observer_intents (
    session_id text NOT NULL,
    process_index bigint NOT NULL,
    process_id text NOT NULL
);


--
-- Name: lash_session_root_inputs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_root_inputs (
    session_id text NOT NULL,
    input_id text NOT NULL,
    root text NOT NULL
);


--
-- Name: lash_session_roots; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_roots (
    session_id text NOT NULL,
    root text NOT NULL,
    terminal_kind text,
    terminal_cause_json text,
    terminal_head_revision bigint,
    terminal_at_ms bigint,
    CONSTRAINT ck_session_roots_terminal CHECK ((((terminal_kind IS NULL) AND (terminal_cause_json IS NULL) AND (terminal_head_revision IS NULL) AND (terminal_at_ms IS NULL)) OR ((terminal_kind = ANY (ARRAY['answered'::text, 'failed'::text, 'cancelled'::text])) AND (terminal_cause_json IS NOT NULL) AND (terminal_at_ms IS NOT NULL))))
);


--
-- Name: lash_sessions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_sessions (
    session_id text NOT NULL,
    head_revision bigint DEFAULT 0 NOT NULL,
    head_json text NOT NULL,
    checkpoint_ref text,
    leaf_node_id text,
    pending_follow_on_json text
);


--
-- Name: lash_tool_intent_submissions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_tool_intent_submissions (
    replay_key text NOT NULL,
    session_id text NOT NULL,
    execution_scope_id text NOT NULL,
    tool_call_id text NOT NULL,
    intent_index bigint NOT NULL,
    kind text NOT NULL,
    payload_hash text NOT NULL,
    submission_json text NOT NULL,
    CONSTRAINT ck_tool_intent_submissions_kind CHECK ((kind = ANY (ARRAY['start_process'::text, 'signal_process'::text, 'cancel_process'::text, 'emit_process_event'::text, 'emit_trigger'::text])))
);


--
-- Name: lash_trigger_deliveries; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_trigger_deliveries (
    occurrence_id text NOT NULL,
    subscription_id text NOT NULL,
    process_id text,
    subscription_incarnation text NOT NULL,
    subscription_revision bigint NOT NULL,
    subscription_snapshot_json text NOT NULL,
    created_at_ms bigint NOT NULL
);


--
-- Name: lash_trigger_mutation_receipts; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_trigger_mutation_receipts (
    operation_id text NOT NULL,
    owner_kind text NOT NULL,
    owner_id text NOT NULL,
    request_fingerprint text NOT NULL,
    result_json text NOT NULL,
    created_at_ms bigint NOT NULL,
    CONSTRAINT ck_trigger_receipts_owner_kind CHECK ((owner_kind = ANY (ARRAY['session'::text, 'host'::text, 'platform'::text])))
);


--
-- Name: lash_trigger_occurrences; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_trigger_occurrences (
    occurrence_id text NOT NULL,
    idempotency_key text NOT NULL,
    source_type text NOT NULL,
    source_key text NOT NULL,
    occurred_at_ms bigint NOT NULL,
    reclaimable_at_ms bigint,
    record_json text NOT NULL
);


--
-- Name: lash_trigger_subscriptions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_trigger_subscriptions (
    subscription_id text NOT NULL,
    owner_scope text NOT NULL,
    subscription_key text NOT NULL,
    incarnation text NOT NULL,
    revision bigint NOT NULL,
    definition_fingerprint text NOT NULL,
    source_type text NOT NULL,
    source_key text NOT NULL,
    lifecycle text NOT NULL,
    deleted_at_ms bigint,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    record_json text NOT NULL,
    CONSTRAINT ck_trigger_subscriptions_lifecycle CHECK ((lifecycle = ANY (ARRAY['enabled'::text, 'disabled'::text, 'tombstoned'::text]))),
    CONSTRAINT ck_trigger_subscriptions_lifecycle_deleted_at CHECK ((((lifecycle = ANY (ARRAY['enabled'::text, 'disabled'::text])) AND (deleted_at_ms IS NULL)) OR ((lifecycle = 'tombstoned'::text) AND (deleted_at_ms IS NOT NULL))))
);


--
-- Name: lash_turn_cancel_affected_inputs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_cancel_affected_inputs (
    session_id text NOT NULL,
    turn_id text NOT NULL,
    ordinal bigint NOT NULL,
    input_id text NOT NULL,
    disposition text NOT NULL,
    input_json text NOT NULL,
    CONSTRAINT ck_turn_cancel_affected_inputs_disposition CHECK ((disposition = ANY (ARRAY['defer'::text, 'drop'::text])))
);


--
-- Name: lash_turn_cancel_closure_authorizations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_cancel_closure_authorizations (
    session_id text NOT NULL,
    turn_id text NOT NULL,
    authorization_json text NOT NULL
);


--
-- Name: lash_turn_cancel_requests; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_cancel_requests (
    session_id text NOT NULL,
    turn_id text NOT NULL,
    request_id text NOT NULL,
    origin text,
    reason text,
    disposition text DEFAULT 'defer'::text NOT NULL,
    mode text DEFAULT 'immediate'::text NOT NULL,
    intent_revision bigint NOT NULL,
    CONSTRAINT ck_turn_cancel_requests_intent_revision CHECK ((intent_revision >= 1))
);


--
-- Name: lash_turn_cancel_retired_scopes; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_cancel_retired_scopes (
    scope_id text NOT NULL
);


--
-- Name: lash_turn_cancellation_bindings; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_cancellation_bindings (
    session_id text NOT NULL,
    binding_id text NOT NULL,
    admitted_scope_json text,
    CONSTRAINT ck_turn_cancellation_bindings_binding_id CHECK ((length(binding_id) > 0))
);


--
-- Name: lash_turn_park_clock; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_park_clock (
    singleton boolean DEFAULT true NOT NULL,
    current_seq bigint DEFAULT 0 NOT NULL,
    compaction_horizon bigint DEFAULT 0 NOT NULL,
    CONSTRAINT ck_turn_park_clock_singleton CHECK (singleton)
);


--
-- Name: lash_turn_park_events; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_park_events (
    seq bigint NOT NULL,
    session_id text NOT NULL,
    turn_id text NOT NULL,
    park_id bigint NOT NULL,
    kind text NOT NULL,
    cause text,
    reason_json text,
    at_ms bigint NOT NULL,
    CONSTRAINT ck_turn_park_events_kind CHECK ((kind = ANY (ARRAY['parked'::text, 'unparked'::text, 'cancelled'::text, 'redrive_requested'::text]))),
    CONSTRAINT ck_turn_park_events_parked_reason CHECK ((((kind = 'parked'::text) AND (reason_json IS NOT NULL) AND (cause IS NULL)) OR ((kind <> 'parked'::text) AND (reason_json IS NULL) AND (cause IS NOT NULL))))
);


--
-- Name: lash_turn_parks; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_turn_parks (
    session_id text NOT NULL,
    turn_id text NOT NULL,
    park_id bigint NOT NULL,
    reason_code text NOT NULL,
    reason_json text NOT NULL,
    since_ms bigint NOT NULL,
    last_refused_ms bigint NOT NULL,
    attempts bigint NOT NULL,
    park_executable_generation text,
    engine_ref text,
    resume_intent bigint,
    CONSTRAINT ck_turn_parks_attempts CHECK ((attempts >= 1))
);


--
-- Name: lash_usage_deltas; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_usage_deltas (
    seq bigint NOT NULL,
    session_id text NOT NULL,
    operation_storage_key text NOT NULL,
    entry_ordinal bigint NOT NULL,
    payload_encoding_version integer NOT NULL,
    payload_hash text NOT NULL,
    source text NOT NULL,
    model text NOT NULL,
    input_tokens bigint NOT NULL,
    output_tokens bigint NOT NULL,
    cache_read_input_tokens bigint NOT NULL,
    cache_write_input_tokens bigint NOT NULL,
    reasoning_output_tokens bigint NOT NULL,
    usage_disposition_json text NOT NULL
);


--
-- Name: lash_usage_deltas_seq_seq; Type: SEQUENCE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE SEQUENCE lash_durable_read_fixture.lash_usage_deltas_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: lash_usage_deltas_seq_seq; Type: SEQUENCE OWNED BY; Schema: lash_durable_read_fixture; Owner: -
--

ALTER SEQUENCE lash_durable_read_fixture.lash_usage_deltas_seq_seq OWNED BY lash_durable_read_fixture.lash_usage_deltas.seq;


--
-- Name: lash_wake_allocation_floors; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_wake_allocation_floors (
    target_session_id text NOT NULL,
    process_id text NOT NULL COLLATE pg_catalog."C",
    allocation_floor bigint NOT NULL
);


--
-- Name: lash_wake_redelivery_fences; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_wake_redelivery_fences (
    session_id text NOT NULL,
    process_id text NOT NULL,
    allocation_floor bigint NOT NULL
);


--
-- Name: lash_control_intents intent_id; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_control_intents ALTER COLUMN intent_id SET DEFAULT nextval('lash_durable_read_fixture.lash_control_intents_intent_id_seq'::regclass);


--
-- Name: lash_usage_deltas seq; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_deltas ALTER COLUMN seq SET DEFAULT nextval('lash_durable_read_fixture.lash_usage_deltas_seq_seq'::regclass);


--
-- Data for Name: lash_artifact_owner_retirements; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_artifact_owners; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_artifact_owners VALUES ('process_execution_env', 'process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738', 'host', 'durable-read-fixture');


--
-- Data for Name: lash_attachment_condemnations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_attachment_manifest; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_attachment_manifest VALUES ('durable-read-attachment', 'durable-read-fixture', 'session:durable-read-fixture:sha256:durable-read-attachment', 100, '88888888888848888888888888888888', 1700000000000, 1700000000000, NULL, NULL);


--
-- Data for Name: lash_blobs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('210ae4978f17c413088878b1bd7c77d4cc5037fbf99030bc96176da68601bacb', '\x82ae736368656d615f76657273696f6e04aa7475726e5f737461746583aa7475726e5f696e64657800ab746f6b656e5f757361676585ac696e7075745f746f6b656e7300ad6f75747075745f746f6b656e7300b763616368655f726561645f696e7075745f746f6b656e7300b863616368655f77726974655f696e7075745f746f6b656e7300b7726561736f6e696e675f6f75747075745f746f6b656e7300b570726f746f636f6c5f7475726e5f6f7074696f6e7382ae736368656d615f76657273696f6e01a77061796c6f616480');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8', '\x464947383837');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d', '\x83ae736368656d615f76657273696f6e04aa7475726e5f737461746583aa7475726e5f696e64657807ab746f6b656e5f757361676585ac696e7075745f746f6b656e730dad6f75747075745f746f6b656e7308b763616368655f726561645f696e7075745f746f6b656e7305b863616368655f77726974655f696e7075745f746f6b656e7303b7726561736f6e696e675f6f75747075745f746f6b656e7302b570726f746f636f6c5f7475726e5f6f7074696f6e7382ae736368656d615f76657273696f6e01a77061796c6f616480aa636f6d706f6e656e747383af657865637574696f6e5f737461746582a8626c6f625f726566d94037366133316561393765313333616537656432333365333433353564386562353330386465613861623033316231623339613637393336666238626339396338b0656e636f64696e675f76657273696f6e02ac706c7567696e5f737461746582a8626c6f625f726566d94063363135356664663164333731613130613731613030373333373630366564356535616137386262653666346636373363303264353234363065323062323439b0656e636f64696e675f76657273696f6e02aa746f6f6c5f737461746582a8626c6f625f726566d94039623332393338663139643030636538653363633031313637363961383635393065396138623936333562646138346162353632373637316361363261353164b0656e636f64696e675f76657273696f6e02');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d', '\x82aa67656e65726174696f6ecd0377a5746f6f6c7380');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249', '\x81bc64757261626c652d726561642d736e617073686f742d706c7567696e82aa67656e65726174696f6ecd0377a676616c75657381a5737461746582a766697874757265ac706c7567696e2d7374617465a576616c7565cd0377');


--
-- Data for Name: lash_catalog_identity; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_catalog_identity VALUES (true, '00000000-0000-4000-8000-000000000887');


--
-- Data for Name: lash_checkpoint_blob_refs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d', '76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8');
INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d', 'c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249');
INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d', '9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d');


--
-- Data for Name: lash_control_intents; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_deleted_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_deleted_sessions VALUES ('durable-read-deleted-session', 1700000000000, NULL, 0, 'root', NULL);


--
-- Data for Name: lash_fleet_format; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_fleet_format VALUES (true, 1);


--
-- Data for Name: lash_fork_lineage; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_graph_nodes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940', NULL, 0, 'frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940', '{"schema_version":22,"timestamp":"2023-11-14T22:13:20+00:00","kind":"frame_open","frame_key":"frame-key/v2/c9b08bd8c779c300744ed4d57455bc2d2166e56c7b2b2783b2fa9db331ef66e6","reason":"initial","assignment":{"policy":{"model":{"id":"","variant":"provider_default","limits":{"context_window_tokens":1}},"provider_id":"","session_id":null,"autonomous":false,"turn_budget":"unbounded"},"plugin_options":{}},"protocol_turn_options":{"schema_version":1,"payload":{}}}', false);
INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08', 'frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940', 1, 'frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940', '{"schema_version":22,"timestamp":"2023-11-14T22:13:20+00:00","kind":"event","event":{"Conversation":{"id":"m_append_9304aea3269b249d9b8e240f046976a16b2a95e618eb374edf1eded586a60e3c","role":"User","parts":[{"id":"m_append_9304aea3269b249d9b8e240f046976a16b2a95e618eb374edf1eded586a60e3c.p0","kind":"Text","content":"durable read user message"}],"origin":{"kind":"plugin","plugin_id":"plugin"}}}}', false);
INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', 'n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08', 2, 'frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940', '{"schema_version":22,"timestamp":"2023-11-14T22:13:20+00:00","kind":"plugin","plugin_type":"durable-read-plugin","body":{"fixture":true,"order":2}}', false);


--
-- Data for Name: lash_lashlang_artifacts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_lashlang_artifacts VALUES ('process_execution_env', 'process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738', '\x7b22706c7567696e5f6f7074696f6e73223a7b7d2c22706f6c696379223a7b226d6f64656c223a7b226964223a22222c2276617269616e74223a2270726f76696465725f64656661756c74222c226c696d697473223a7b22636f6e746578745f77696e646f775f746f6b656e73223a317d7d2c2270726f76696465725f6964223a22222c2273657373696f6e5f6964223a6e756c6c2c226175746f6e6f6d6f7573223a66616c73652c227475726e5f627564676574223a22756e626f756e646564227d7d');


--
-- Data for Name: lash_migrations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_node_anchors; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_node_anchors VALUES ('n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', '92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d', 'durable-read-fixture');


--
-- Data for Name: lash_parent_end_plans; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_parent_end_plans VALUES ('process', 'process:34:p_00000000000070008000000000000003', '{"version":2,"scope":{"kind":"opener","scope":{"kind":"process","process_id":"p_00000000000070008000000000000003"}}}', 1700000000000, NULL);


--
-- Data for Name: lash_pending_turn_inputs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_pending_turn_inputs VALUES (2, 'durable-read-pending-input', 'durable-read-fixture', 'durable-read-input-source', '{"scope":"next_turn"}', 'deferred_next_turn', '{"items":[{"type":"text","text":"durable read pending input"}]}', '{"scope":"next_turn"}', 'turn-input-submission:v1:blake3:cfa33cf885994ad5ebc91e08f8f36422a792cda3baab46dff1f6f4433e6bc20a', 1700000000000, NULL, NULL, NULL, NULL, 0, 0, NULL, NULL);


--
-- Data for Name: lash_process_artifact_cleanup; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_artifact_cleanup VALUES ('p_00000000000070008000000000000003', '{"input": {"type": "external", "metadata": {"fixture": "tombstone"}}, "env_ref": null, "process_id": "p_00000000000070008000000000000003"}');


--
-- Data for Name: lash_process_change_clock; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_change_clock VALUES (true, 10, 0);


--
-- Data for Name: lash_process_definitions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_process_events; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 1, 'process.observer_added', 'process:p_00000000000070008000000000000001:observer:durable-read-fixture:add:registration', '{"process_id":"p_00000000000070008000000000000001","sequence":1,"event_type":"process.observer_added","payload":{"by":{"kind":"host","operation_id":"registration"},"session":"durable-read-fixture"},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":1,"event_type":"process.observer_added"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"process:p_00000000000070008000000000000001:observer:durable-read-fixture:add:registration"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 2, 'process.waiting', 'process:p_00000000000070008000000000000001:wait:durable-read-wait-key:since:123:entered', '{"process_id":"p_00000000000070008000000000000001","sequence":2,"event_type":"process.waiting","payload":{"wait":{"kind":{"event_type":"process.signal.fixture-ready","key":"durable-read-wait-key","kind":"signal","name":"fixture-ready","ordinal":1},"since_ms":123}},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":2,"event_type":"process.waiting"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"process:p_00000000000070008000000000000001:wait:durable-read-wait-key:since:123:entered"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 3, 'process.effect_outcome', 'durable-read-tool-effect:1', '{"process_id":"p_00000000000070008000000000000001","sequence":3,"event_type":"process.effect_outcome","payload":{"code":"lash:trigger_invalid","node_id":"durable-read-tool-node","occurrence":1,"operation":"tool:fixture","outcome_class":"failure","replay_key":"durable-read-tool-effect:1","vocabulary_version":1},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":3,"event_type":"process.effect_outcome"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"durable-read-tool-effect:1"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 4, 'process.effect_omissions', 'durable-read-effect-omissions', '{"process_id":"p_00000000000070008000000000000001","sequence":4,"event_type":"process.effect_omissions","payload":{"nodes":{"durable-read-tool-node":{"cancelled":0,"failure":1,"success":3}},"occurrence_cap":8,"vocabulary_version":1},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":4,"event_type":"process.effect_omissions"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"durable-read-effect-omissions"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000002', 1, 'fixture.wake', NULL, '{"process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake","payload":{"wake_input":"durable read wake"},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000002"}},"semantics":{"wake":{"input":"durable read wake"}},"occurred_at":1700000000000}');


--
-- Data for Name: lash_process_leases; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_leases VALUES ('p_00000000000070008000000000000001', 'durable-read-owner', 'durable-read-incarnation', '3f3f47c7f931e38b58bccf32ed73e92f50897d7577e5374fd1a4233cccacfb62', 1, 1700000000000, 1700000000100);


--
-- Data for Name: lash_process_observers; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_observers VALUES ('durable-read-fixture', 'p_00000000000070008000000000000001');


--
-- Data for Name: lash_process_park_clock; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_park_clock VALUES (true, 0, 0);


--
-- Data for Name: lash_process_park_events; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_process_segment_handovers; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_segment_handovers VALUES ('p_00000000000070008000000000000001', 1, '{"segment_ordinal":1,"handover":{"reason":"journal_budget","program_hash":"durable-read-program-v1","engine_state":[8,8,7]}}', NULL);


--
-- Data for Name: lash_process_tombstones; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_tombstones VALUES ('p_00000000000070008000000000000003', 'completed', 1700000000000, 10);


--
-- Data for Name: lash_process_wake_deliveries; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_wake_deliveries VALUES ('wake:v1:blake3:5f8eb7fb2de74745b4f301023356a7a43ea1452d796158036ec9542fde33d409', 'p_00000000000070008000000000000002', 'durable-read-fixture', 1, 'pending', NULL, 0, NULL, 1700000000000, 1700604800000, NULL, '{"version":4,"wake_id":"wake:v1:blake3:5f8eb7fb2de74745b4f301023356a7a43ea1452d796158036ec9542fde33d409","target_session_id":"durable-read-fixture","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake","event_invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000002"}},"authority":{"principal":"host"},"input":"durable read wake","created_at_ms":1700000000000}');


--
-- Data for Name: lash_processes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_processes VALUES ('p_00000000000070008000000000000001', 'process-start-key:v1:host:blake3:31478e59bda91f8d59606543c763719fe0813e431d1bc9e7ba1856a186e31ce4', 'host', NULL, 'durable-read-engine', 'Durable read fixture', 1700000000000, 1700000000000, 4, 5, 'waiting', 'detached', NULL, NULL, NULL, NULL, NULL, NULL, '{"id":"p_00000000000070008000000000000001","start_key":"process-start-key:v1:host:blake3:31478e59bda91f8d59606543c763719fe0813e431d1bc9e7ba1856a186e31ce4","last_event_sequence":4,"input":{"type":"engine","kind":"durable-read-engine","payload":{"fixture":"process"}},"disposition":"rerunnable","lifetime":{"lifetime":"detached"},"ancestry":[],"identity":{"kind":"durable-read-engine","label":"Durable read fixture","definition":{"engine_kind":"durable-read-engine","definition":{"fixture":"process"},"signature":{"signature":"unknown"}}},"event_types":[{"name":"process.completed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"completed","await_output":{"pointer":"/await_output"}}}},{"name":"process.failed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"failed","await_output":{"pointer":"/await_output"}}}},{"name":"process.cancelled","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"cancelled","await_output":{"pointer":"/await_output"}}}},{"name":"process.abandoned","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"abandoned","await_output":{"pointer":"/await_output"}}}}],"provenance":{"originator":{"type":"host"}},"env_ref":"process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738","created_at_ms":1700000000000,"updated_at_ms":1700000000000,"wait":{"kind":{"kind":"signal","name":"fixture-ready","event_type":"process.signal.fixture-ready","key":"durable-read-wait-key","ordinal":1},"since_ms":123},"status":"waiting"}');
INSERT INTO lash_durable_read_fixture.lash_processes VALUES ('p_00000000000070008000000000000002', NULL, 'host', 'durable-read-fixture', 'external', NULL, 1700000000000, 1700000000000, 1, 7, 'running', 'detached', NULL, NULL, NULL, NULL, NULL, NULL, '{"id":"p_00000000000070008000000000000002","last_event_sequence":1,"input":{"type":"external","metadata":{"fixture":"wake"}},"disposition":"externally_owned","lifetime":{"lifetime":"detached"},"ancestry":[],"identity":{"kind":"external"},"event_types":[{"name":"process.completed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"completed","await_output":{"pointer":"/await_output"}}}},{"name":"process.failed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"failed","await_output":{"pointer":"/await_output"}}}},{"name":"process.cancelled","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"cancelled","await_output":{"pointer":"/await_output"}}}},{"name":"process.abandoned","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"abandoned","await_output":{"pointer":"/await_output"}}}},{"name":"fixture.wake","payload_schema":{"schema":{}},"semantics":{"wake":{"when":{"present":"/wake_input"},"input":{"pointer":"/wake_input"}}}}],"provenance":{"originator":{"type":"host"}},"created_at_ms":1700000000000,"updated_at_ms":1700000000000,"status":"running"}');


--
-- Data for Name: lash_queued_run_members; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_queued_runs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_queued_work_batches; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_queued_work_batches VALUES (1, 'qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48', 'durable-read-fixture', 'process:p_69d943393ff87a56a6f44a7f58b2d465:event:1:wake', 'earliest_safe_boundary', 'turn', '{}', 'lash.process_wake', 0, 1700000000000, NULL, NULL, 0, 0);


--
-- Data for Name: lash_queued_work_items; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_queued_work_items VALUES ('qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48', 0, 'qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48:item:0', '{"type":"process_wake","wake":{"version":4,"wake_id":"durable-read-queue-wake","target_session_id":"durable-read-fixture","process_id":"p_69d943393ff87a56a6f44a7f58b2d465","sequence":1,"event_type":"process.wake","event_invocation":{"attribution":{"session_id":"durable-read-fixture"},"subject":{"type":"process_event","process_id":"p_69d943393ff87a56a6f44a7f58b2d465","sequence":1,"event_type":"process.wake"}},"input":"durable read queued task","created_at_ms":1700000000000}}');


--
-- Data for Name: lash_release_stamp; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_release_stamp VALUES (true, '0.0.0-dev', 'lash-postgres-store=141', 1700000000000);


--
-- Data for Name: lash_runtime_turn_commits; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"append-session-nodes","scope":{"operation_id":"session:durable-read-fixture:boundary:durable-read-current-append","type":"runtime_operation"}}', '9e9a441ea3cfee8cd11d9bc6ec13ff8fd8fe471bdece7dcf5dda8a5a23ec4be6', '{"schema_version":2,"head_revision":1,"checkpoint_ref":"210ae4978f17c413088878b1bd7c77d4cc5037fbf99030bc96176da68601bacb","manifest":{"schema_version":4,"turn_state":{"turn_index":0,"token_usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_write_input_tokens":0,"reasoning_output_tokens":0},"protocol_turn_options":{"schema_version":1,"payload":{}}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[{"node_id":"frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940","timestamp":"2023-11-14T22:13:20+00:00"},{"node_id":"n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08","timestamp":"2023-11-14T22:13:20+00:00"},{"node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","timestamp":"2023-11-14T22:13:20+00:00"}]}', 1700000000000, '3e8aeb1a0000dd8ff135743a2ef9b7d7b35ab31afba21c26692ac2f7bae54f48', 2, 7);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-legacy-commit","type":"runtime_operation"}}', 'a8e546ba83930b0a8a3a8baaf8925bd8689762ef2b0720df07e83d6d256a5f11', '{"schema_version":2,"head_revision":2,"checkpoint_ref":"92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d","manifest":{"schema_version":4,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2},"protocol_turn_options":{"schema_version":1,"payload":{}}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":2},"plugin_state":{"blob_ref":"c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249","encoding_version":2},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":2}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[],"committed_usage_delta_identities":[{"operation_storage_key":"{\"key\":\"commit\",\"scope\":{\"operation_id\":\"durable-read-legacy-commit\",\"type\":\"runtime_operation\"}}","entry_ordinal":0,"payload_encoding_version":4,"payload_hash":"0885a585f704e9086220f97cad28cd83905497f1ad2484f2fd1d1895945e5dba"}]}', 1700000000000, NULL, NULL, NULL);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"record-config","scope":{"operation_id":"session:durable-read-fixture:boundary:protocol-materialization","type":"runtime_operation"}}', '77e7dd1001ceb1041bd7a5661e7e1c699bdcc927b4c60b24ff12a91c954bcc46', '{"schema_version":2,"head_revision":3,"checkpoint_ref":"92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d","manifest":{"schema_version":4,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2},"protocol_turn_options":{"schema_version":1,"payload":{}}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":2},"plugin_state":{"blob_ref":"c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249","encoding_version":2},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":2}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[]}', 1700000000000, 'bc65c05bb2e993f4c118602bdc825c96ed78aa1d7762118c841a3f9bd98aea6e', NULL, 3);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-wake-settlement","type":"runtime_operation"}}', '7d1a81af7200c71d51ff92138efc3c540ceb2e9fe950a4397c552fd59b1d6d74', '{"schema_version":2,"head_revision":4,"checkpoint_ref":"92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d","manifest":{"schema_version":4,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2},"protocol_turn_options":{"schema_version":1,"payload":{}}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":2},"plugin_state":{"blob_ref":"c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249","encoding_version":2},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":2}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[]}', 1700000000000, NULL, NULL, NULL);


--
-- Data for Name: lash_schema_versions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_schema_versions VALUES ('lash-postgres-store', 141);


--
-- Data for Name: lash_session_execution_leases; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_execution_leases VALUES ('durable-read-fixture', 'durable-read-session-owner', 'durable-read-session-incarnation', 'durable-read-retained-executor', 'durable-read-retained-session-lease', 2, 1700000000000, 100, 1700000000100);


--
-- Data for Name: lash_session_ingress; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_session_ingress_sequence; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_ingress_sequence VALUES ('durable-read-fixture', 3);


--
-- Data for Name: lash_session_meta; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_meta VALUES ('durable-read-fixture', 3, 1700000000000, 1700000000000, 'root', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_session_meta_pending_observer_intents; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_session_root_inputs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_session_roots; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_sessions VALUES ('durable-read-fixture', 4, '{"schema_version":11,"session_id":"durable-read-fixture","config":{"provider_id":"","model":{"id":"","variant":"provider_default","limits":{"context_window_tokens":1}},"turn_budget":"unbounded","prompt":{},"generation":{},"tool_access":{"mode":"ambient"},"subagent":null,"protocol_turn_options":{"schema_version":1,"payload":{}},"config_revision":0},"current_frame_node_id":"frame-node/v3/1eea72aaea89086d6bc4149c359256b8e3a459bbafee748808da3e69e7888940"}', '92171b9c5f5a51fe643c34750d2fd654a6d73af6a28ead15125fec2326f0be1d', 'n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', NULL);


--
-- Data for Name: lash_tool_intent_submissions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_trigger_deliveries; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_deliveries VALUES ('trigger:durable-read-occurrence', 'trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74', NULL, 'durable-read-trigger-incarnation', 1, '{"subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v3:blake3:a359a0f8b9619d0aa56a03b9a45ffaaa89c69ad02e3e9f45a7311249b8ee4df3","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"source_capture":{"constructor_path":["fixture","event"],"config_schema":{"schema":{"additionalProperties":false,"properties":{"fixture":{"type":"string"}},"type":"object"}},"route":{"kind":"provider","provider_id":"fixture-provider","route":{"account":"fixture"}}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"engine_kind":"durable-read-trigger-target","definition":{"fixture":"trigger"},"signature":{"signature":"unknown"}}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","lifecycle":{"lifecycle":"enabled"},"created_at_ms":1700000000000,"updated_at_ms":1700000000000}', 1700000000000);


--
-- Data for Name: lash_trigger_mutation_receipts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_mutation_receipts VALUES ('trigger-operation:v2:blake3:d53259dcf40a55e423a5392198cae3383e86089d874f1294ec29183c9a874a4f', 'session', 'durable-read-fixture', 'trigger-command:v6:blake3:856c9bb689ee094ff80cf15559ef868bba0272b640be5c5d8be659696ecbbcc3', '{"Ok":{"type":"mutation","receipt":{"owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v3:blake3:a359a0f8b9619d0aa56a03b9a45ffaaa89c69ad02e3e9f45a7311249b8ee4df3","enabled":true,"disposition":"created","record_snapshot":{"subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v3:blake3:a359a0f8b9619d0aa56a03b9a45ffaaa89c69ad02e3e9f45a7311249b8ee4df3","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"source_capture":{"constructor_path":["fixture","event"],"config_schema":{"schema":{"additionalProperties":false,"properties":{"fixture":{"type":"string"}},"type":"object"}},"route":{"kind":"provider","provider_id":"fixture-provider","route":{"account":"fixture"}}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"engine_kind":"durable-read-trigger-target","definition":{"fixture":"trigger"},"signature":{"signature":"unknown"}}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","lifecycle":{"lifecycle":"enabled"},"created_at_ms":1700000000000,"updated_at_ms":1700000000000}}}}', 1700000000000);


--
-- Data for Name: lash_trigger_occurrences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_occurrences VALUES ('trigger:durable-read-occurrence', 'durable-read-occurrence', 'fixture.event', 'fixture-source', 1700000000000, NULL, '{"occurrence_id":"trigger:durable-read-occurrence","source_type":"fixture.event","source_key":"fixture-source","payload":{"value":42},"idempotency_key":"durable-read-occurrence","occurred_at_ms":1700000000000}');


--
-- Data for Name: lash_trigger_subscriptions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_subscriptions VALUES ('trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74', 'session:durable-read-fixture', 'durable-read-trigger', 'durable-read-trigger-incarnation', 1, 'trigger-definition:v3:blake3:a359a0f8b9619d0aa56a03b9a45ffaaa89c69ad02e3e9f45a7311249b8ee4df3', 'fixture.event', 'fixture-source', 'enabled', NULL, 1700000000000, 1700000000000, '{"subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v3:blake3:a359a0f8b9619d0aa56a03b9a45ffaaa89c69ad02e3e9f45a7311249b8ee4df3","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v6:blake3:4999a9eb5f1038bea76c7d1c114893c28c91b7fd479339f4b1edf60314744738","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"source_capture":{"constructor_path":["fixture","event"],"config_schema":{"schema":{"additionalProperties":false,"properties":{"fixture":{"type":"string"}},"type":"object"}},"route":{"kind":"provider","provider_id":"fixture-provider","route":{"account":"fixture"}}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"engine_kind":"durable-read-trigger-target","definition":{"fixture":"trigger"},"signature":{"signature":"unknown"}}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","lifecycle":{"lifecycle":"enabled"},"created_at_ms":1700000000000,"updated_at_ms":1700000000000}');


--
-- Data for Name: lash_turn_cancel_affected_inputs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_turn_cancel_closure_authorizations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_turn_cancel_requests; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_turn_cancel_retired_scopes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_turn_cancellation_bindings; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_turn_park_clock; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_turn_park_clock VALUES (true, 0, 0);


--
-- Data for Name: lash_turn_park_events; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_turn_parks; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_usage_deltas; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_usage_deltas VALUES (1, 'durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-legacy-commit","type":"runtime_operation"}}', 0, 4, '0885a585f704e9086220f97cad28cd83905497f1ad2484f2fd1d1895945e5dba', 'durable-read-turn', 'durable-read-model', 21, 12, 5, 3, 2, '{"kind":"reported"}');


--
-- Data for Name: lash_wake_allocation_floors; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_wake_allocation_floors VALUES ('durable-read-fixture', 'p_00000000000070008000000000000002', 1);


--
-- Data for Name: lash_wake_redelivery_fences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_wake_redelivery_fences VALUES ('durable-read-fixture', 'p_00000000000070008000000000000002', 1);


--
-- Name: lash_control_intents_intent_id_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_control_intents_intent_id_seq', 1, false);


--
-- Name: lash_usage_deltas_seq_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_usage_deltas_seq_seq', 1, true);


--
-- Name: lash_artifact_owner_retirements lash_artifact_owner_retirements_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_artifact_owner_retirements
    ADD CONSTRAINT lash_artifact_owner_retirements_pkey PRIMARY KEY (owner_kind, owner_id);


--
-- Name: lash_artifact_owners lash_artifact_owners_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_artifact_owners
    ADD CONSTRAINT lash_artifact_owners_pkey PRIMARY KEY (namespace, artifact_ref, owner_kind, owner_id);


--
-- Name: lash_attachment_condemnations lash_attachment_condemnations_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_condemnations
    ADD CONSTRAINT lash_attachment_condemnations_pkey PRIMARY KEY (attachment_id);


--
-- Name: lash_attachment_manifest lash_attachment_manifest_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_manifest
    ADD CONSTRAINT lash_attachment_manifest_pkey PRIMARY KEY (session_id, attachment_id);


--
-- Name: lash_blobs lash_blobs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_blobs
    ADD CONSTRAINT lash_blobs_pkey PRIMARY KEY (hash);


--
-- Name: lash_catalog_identity lash_catalog_identity_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_catalog_identity
    ADD CONSTRAINT lash_catalog_identity_pkey PRIMARY KEY (singleton);


--
-- Name: lash_checkpoint_blob_refs lash_checkpoint_blob_refs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_checkpoint_blob_refs
    ADD CONSTRAINT lash_checkpoint_blob_refs_pkey PRIMARY KEY (checkpoint_ref, blob_ref);


--
-- Name: lash_control_intents lash_control_intents_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_control_intents
    ADD CONSTRAINT lash_control_intents_pkey PRIMARY KEY (intent_id);


--
-- Name: lash_deleted_sessions lash_deleted_sessions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_deleted_sessions
    ADD CONSTRAINT lash_deleted_sessions_pkey PRIMARY KEY (session_id);


--
-- Name: lash_fleet_format lash_fleet_format_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_fleet_format
    ADD CONSTRAINT lash_fleet_format_pkey PRIMARY KEY (singleton);


--
-- Name: lash_fork_lineage lash_fork_lineage_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_fork_lineage
    ADD CONSTRAINT lash_fork_lineage_pkey PRIMARY KEY (session_id, ancestor_session_id);


--
-- Name: lash_graph_nodes lash_graph_nodes_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_graph_nodes
    ADD CONSTRAINT lash_graph_nodes_pkey PRIMARY KEY (node_id);


--
-- Name: lash_graph_nodes lash_graph_nodes_session_id_generation_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_graph_nodes
    ADD CONSTRAINT lash_graph_nodes_session_id_generation_key UNIQUE (session_id, generation);


--
-- Name: lash_lashlang_artifacts lash_lashlang_artifacts_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_lashlang_artifacts
    ADD CONSTRAINT lash_lashlang_artifacts_pkey PRIMARY KEY (namespace, artifact_ref);


--
-- Name: lash_migrations lash_migrations_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_migrations
    ADD CONSTRAINT lash_migrations_pkey PRIMARY KEY (phase, migration);


--
-- Name: lash_node_anchors lash_node_anchors_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_node_anchors
    ADD CONSTRAINT lash_node_anchors_pkey PRIMARY KEY (node_id);


--
-- Name: lash_parent_end_plans lash_parent_end_plans_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_parent_end_plans
    ADD CONSTRAINT lash_parent_end_plans_pkey PRIMARY KEY (parent_kind, parent_id);


--
-- Name: lash_pending_turn_inputs lash_pending_turn_inputs_input_id_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_pending_turn_inputs
    ADD CONSTRAINT lash_pending_turn_inputs_input_id_key UNIQUE (input_id);


--
-- Name: lash_pending_turn_inputs lash_pending_turn_inputs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_pending_turn_inputs
    ADD CONSTRAINT lash_pending_turn_inputs_pkey PRIMARY KEY (session_id, enqueue_seq);


--
-- Name: lash_pending_turn_inputs lash_pending_turn_inputs_session_id_source_key_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_pending_turn_inputs
    ADD CONSTRAINT lash_pending_turn_inputs_session_id_source_key_key UNIQUE (session_id, source_key);


--
-- Name: lash_process_artifact_cleanup lash_process_artifact_cleanup_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_artifact_cleanup
    ADD CONSTRAINT lash_process_artifact_cleanup_pkey PRIMARY KEY (process_id);


--
-- Name: lash_process_change_clock lash_process_change_clock_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_change_clock
    ADD CONSTRAINT lash_process_change_clock_pkey PRIMARY KEY (singleton);


--
-- Name: lash_process_definitions lash_process_definitions_owner_scope_name_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_definitions
    ADD CONSTRAINT lash_process_definitions_owner_scope_name_key UNIQUE (owner_scope, name);


--
-- Name: lash_process_definitions lash_process_definitions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_definitions
    ADD CONSTRAINT lash_process_definitions_pkey PRIMARY KEY (definition_id);


--
-- Name: lash_process_events lash_process_events_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_events
    ADD CONSTRAINT lash_process_events_pkey PRIMARY KEY (process_id, sequence);


--
-- Name: lash_process_leases lash_process_leases_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_leases
    ADD CONSTRAINT lash_process_leases_pkey PRIMARY KEY (process_id);


--
-- Name: lash_process_observers lash_process_observers_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_observers
    ADD CONSTRAINT lash_process_observers_pkey PRIMARY KEY (session_id, process_id);


--
-- Name: lash_process_park_clock lash_process_park_clock_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_park_clock
    ADD CONSTRAINT lash_process_park_clock_pkey PRIMARY KEY (singleton);


--
-- Name: lash_process_park_events lash_process_park_events_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_park_events
    ADD CONSTRAINT lash_process_park_events_pkey PRIMARY KEY (seq);


--
-- Name: lash_process_segment_handovers lash_process_segment_handovers_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_segment_handovers
    ADD CONSTRAINT lash_process_segment_handovers_pkey PRIMARY KEY (process_id, segment_ordinal);


--
-- Name: lash_process_tombstones lash_process_tombstones_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_tombstones
    ADD CONSTRAINT lash_process_tombstones_pkey PRIMARY KEY (process_id);


--
-- Name: lash_process_wake_deliveries lash_process_wake_deliveries_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_wake_deliveries
    ADD CONSTRAINT lash_process_wake_deliveries_pkey PRIMARY KEY (delivery_id);


--
-- Name: lash_processes lash_processes_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_processes
    ADD CONSTRAINT lash_processes_pkey PRIMARY KEY (process_id);


--
-- Name: lash_queued_run_members lash_queued_run_members_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_run_members
    ADD CONSTRAINT lash_queued_run_members_pkey PRIMARY KEY (session_id, scope_id, collection_kind, ordinal);


--
-- Name: lash_queued_run_members lash_queued_run_members_session_id_scope_id_collection_kind_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_run_members
    ADD CONSTRAINT lash_queued_run_members_session_id_scope_id_collection_kind_key UNIQUE (session_id, scope_id, collection_kind, member_kind, member_id);


--
-- Name: lash_queued_runs lash_queued_runs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_runs
    ADD CONSTRAINT lash_queued_runs_pkey PRIMARY KEY (session_id, scope_id);


--
-- Name: lash_queued_work_batches lash_queued_work_batches_batch_id_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_work_batches
    ADD CONSTRAINT lash_queued_work_batches_batch_id_key UNIQUE (batch_id);


--
-- Name: lash_queued_work_batches lash_queued_work_batches_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_work_batches
    ADD CONSTRAINT lash_queued_work_batches_pkey PRIMARY KEY (session_id, enqueue_seq);


--
-- Name: lash_queued_work_batches lash_queued_work_batches_session_id_source_key_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_work_batches
    ADD CONSTRAINT lash_queued_work_batches_session_id_source_key_key UNIQUE (session_id, source_key);


--
-- Name: lash_queued_work_items lash_queued_work_items_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_work_items
    ADD CONSTRAINT lash_queued_work_items_pkey PRIMARY KEY (batch_id, item_index);


--
-- Name: lash_release_stamp lash_release_stamp_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_release_stamp
    ADD CONSTRAINT lash_release_stamp_pkey PRIMARY KEY (singleton);


--
-- Name: lash_runtime_turn_commits lash_runtime_turn_commits_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_runtime_turn_commits
    ADD CONSTRAINT lash_runtime_turn_commits_pkey PRIMARY KEY (session_id, turn_id);


--
-- Name: lash_schema_versions lash_schema_versions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_schema_versions
    ADD CONSTRAINT lash_schema_versions_pkey PRIMARY KEY (component);


--
-- Name: lash_session_execution_leases lash_session_execution_leases_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_execution_leases
    ADD CONSTRAINT lash_session_execution_leases_pkey PRIMARY KEY (session_id);


--
-- Name: lash_session_ingress lash_session_ingress_item_id_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_ingress
    ADD CONSTRAINT lash_session_ingress_item_id_key UNIQUE (item_id);


--
-- Name: lash_session_ingress lash_session_ingress_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_ingress
    ADD CONSTRAINT lash_session_ingress_pkey PRIMARY KEY (session_id, enqueue_seq);


--
-- Name: lash_session_ingress_sequence lash_session_ingress_sequence_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_ingress_sequence
    ADD CONSTRAINT lash_session_ingress_sequence_pkey PRIMARY KEY (session_id);


--
-- Name: lash_session_ingress lash_session_ingress_session_id_source_key_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_ingress
    ADD CONSTRAINT lash_session_ingress_session_id_source_key_key UNIQUE (session_id, source_key);


--
-- Name: lash_session_meta_pending_observer_intents lash_session_meta_pending_observer_intents_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_meta_pending_observer_intents
    ADD CONSTRAINT lash_session_meta_pending_observer_intents_pkey PRIMARY KEY (session_id, process_id);


--
-- Name: lash_session_meta_pending_observer_intents lash_session_meta_pending_observer_session_id_process_index_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_meta_pending_observer_intents
    ADD CONSTRAINT lash_session_meta_pending_observer_session_id_process_index_key UNIQUE (session_id, process_index);


--
-- Name: lash_session_meta lash_session_meta_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_meta
    ADD CONSTRAINT lash_session_meta_pkey PRIMARY KEY (session_id);


--
-- Name: lash_session_root_inputs lash_session_root_inputs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_root_inputs
    ADD CONSTRAINT lash_session_root_inputs_pkey PRIMARY KEY (session_id, input_id);


--
-- Name: lash_session_roots lash_session_roots_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_roots
    ADD CONSTRAINT lash_session_roots_pkey PRIMARY KEY (session_id, root);


--
-- Name: lash_sessions lash_sessions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_sessions
    ADD CONSTRAINT lash_sessions_pkey PRIMARY KEY (session_id);


--
-- Name: lash_tool_intent_submissions lash_tool_intent_submissions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_tool_intent_submissions
    ADD CONSTRAINT lash_tool_intent_submissions_pkey PRIMARY KEY (replay_key);


--
-- Name: lash_trigger_deliveries lash_trigger_deliveries_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_deliveries
    ADD CONSTRAINT lash_trigger_deliveries_pkey PRIMARY KEY (occurrence_id, subscription_id);


--
-- Name: lash_trigger_mutation_receipts lash_trigger_mutation_receipts_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_mutation_receipts
    ADD CONSTRAINT lash_trigger_mutation_receipts_pkey PRIMARY KEY (operation_id);


--
-- Name: lash_trigger_occurrences lash_trigger_occurrences_idempotency_key_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_occurrences
    ADD CONSTRAINT lash_trigger_occurrences_idempotency_key_key UNIQUE (idempotency_key);


--
-- Name: lash_trigger_occurrences lash_trigger_occurrences_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_occurrences
    ADD CONSTRAINT lash_trigger_occurrences_pkey PRIMARY KEY (occurrence_id);


--
-- Name: lash_trigger_subscriptions lash_trigger_subscriptions_owner_scope_subscription_key_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_subscriptions
    ADD CONSTRAINT lash_trigger_subscriptions_owner_scope_subscription_key_key UNIQUE (owner_scope, subscription_key);


--
-- Name: lash_trigger_subscriptions lash_trigger_subscriptions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_subscriptions
    ADD CONSTRAINT lash_trigger_subscriptions_pkey PRIMARY KEY (subscription_id);


--
-- Name: lash_turn_cancel_affected_inputs lash_turn_cancel_affected_input_session_id_turn_id_input_id_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_affected_inputs
    ADD CONSTRAINT lash_turn_cancel_affected_input_session_id_turn_id_input_id_key UNIQUE (session_id, turn_id, input_id);


--
-- Name: lash_turn_cancel_affected_inputs lash_turn_cancel_affected_inputs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_affected_inputs
    ADD CONSTRAINT lash_turn_cancel_affected_inputs_pkey PRIMARY KEY (session_id, turn_id, ordinal);


--
-- Name: lash_turn_cancel_closure_authorizations lash_turn_cancel_closure_authorizations_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_closure_authorizations
    ADD CONSTRAINT lash_turn_cancel_closure_authorizations_pkey PRIMARY KEY (session_id, turn_id);


--
-- Name: lash_turn_cancel_requests lash_turn_cancel_requests_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_requests
    ADD CONSTRAINT lash_turn_cancel_requests_pkey PRIMARY KEY (session_id, turn_id);


--
-- Name: lash_turn_cancel_retired_scopes lash_turn_cancel_retired_scopes_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_retired_scopes
    ADD CONSTRAINT lash_turn_cancel_retired_scopes_pkey PRIMARY KEY (scope_id);


--
-- Name: lash_turn_cancellation_bindings lash_turn_cancellation_bindings_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancellation_bindings
    ADD CONSTRAINT lash_turn_cancellation_bindings_pkey PRIMARY KEY (session_id);


--
-- Name: lash_turn_park_clock lash_turn_park_clock_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_park_clock
    ADD CONSTRAINT lash_turn_park_clock_pkey PRIMARY KEY (singleton);


--
-- Name: lash_turn_park_events lash_turn_park_events_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_park_events
    ADD CONSTRAINT lash_turn_park_events_pkey PRIMARY KEY (seq);


--
-- Name: lash_turn_parks lash_turn_parks_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_parks
    ADD CONSTRAINT lash_turn_parks_pkey PRIMARY KEY (session_id);


--
-- Name: lash_usage_deltas lash_usage_deltas_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_deltas
    ADD CONSTRAINT lash_usage_deltas_pkey PRIMARY KEY (seq);


--
-- Name: lash_usage_deltas lash_usage_deltas_session_id_operation_storage_key_entry_or_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_deltas
    ADD CONSTRAINT lash_usage_deltas_session_id_operation_storage_key_entry_or_key UNIQUE (session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash);


--
-- Name: lash_wake_allocation_floors lash_wake_allocation_floors_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_wake_allocation_floors
    ADD CONSTRAINT lash_wake_allocation_floors_pkey PRIMARY KEY (target_session_id, process_id);


--
-- Name: lash_wake_redelivery_fences lash_wake_redelivery_fences_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_wake_redelivery_fences
    ADD CONSTRAINT lash_wake_redelivery_fences_pkey PRIMARY KEY (session_id, process_id);


--
-- Name: idx_lash_artifact_owners_owner; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_artifact_owners_owner ON lash_durable_read_fixture.lash_artifact_owners USING btree (owner_kind, owner_id);


--
-- Name: idx_lash_attachment_manifest_owner; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_manifest_owner ON lash_durable_read_fixture.lash_attachment_manifest USING btree (session_id, owner_kind, owner_id, committed_at_ms);


--
-- Name: idx_lash_attachment_manifest_uncommitted; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_manifest_uncommitted ON lash_durable_read_fixture.lash_attachment_manifest USING btree (committed_at_ms) WHERE (committed_at_ms IS NULL);


--
-- Name: idx_lash_attachment_manifest_written; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_manifest_written ON lash_durable_read_fixture.lash_attachment_manifest USING btree (attachment_id, written_at_ms);


--
-- Name: idx_lash_checkpoint_blob_refs_blob_ref; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_checkpoint_blob_refs_blob_ref ON lash_durable_read_fixture.lash_checkpoint_blob_refs USING btree (blob_ref, checkpoint_ref);


--
-- Name: idx_lash_control_intents_open; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_control_intents_open ON lash_durable_read_fixture.lash_control_intents USING btree (intent_id) WHERE (state = ANY (ARRAY['pending'::text, 'failed_retryable'::text]));


--
-- Name: idx_lash_control_intents_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_control_intents_session ON lash_durable_read_fixture.lash_control_intents USING btree (session_id, kind);


--
-- Name: idx_lash_graph_nodes_parent; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_graph_nodes_parent ON lash_durable_read_fixture.lash_graph_nodes USING btree (parent_node_id);


--
-- Name: idx_lash_node_anchors_checkpoint_ref; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_node_anchors_checkpoint_ref ON lash_durable_read_fixture.lash_node_anchors USING btree (checkpoint_ref);


--
-- Name: idx_lash_parent_end_plans_pending; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_parent_end_plans_pending ON lash_durable_read_fixture.lash_parent_end_plans USING btree (ended_at_ms, parent_kind, parent_id) WHERE (settled_at_ms IS NULL);


--
-- Name: idx_lash_pending_turn_input_order; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_input_order ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (session_id, state, enqueued_at_ms, enqueue_seq);


--
-- Name: idx_lash_pending_turn_inputs_claim; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_claim ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (session_id, claim_id, claim_token);


--
-- Name: idx_lash_pending_turn_inputs_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_session ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (session_id, state, enqueue_seq);


--
-- Name: idx_lash_process_definitions_change; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_definitions_change ON lash_durable_read_fixture.lash_process_definitions USING btree (change_seq);


--
-- Name: idx_lash_process_definitions_registrant; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_definitions_registrant ON lash_durable_read_fixture.lash_process_definitions USING btree (owner_scope, name);


--
-- Name: idx_lash_process_events_key; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_process_events_key ON lash_durable_read_fixture.lash_process_events USING btree (process_id, idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: idx_lash_process_observers_process; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_observers_process ON lash_durable_read_fixture.lash_process_observers USING btree (process_id, session_id);


--
-- Name: idx_lash_process_tombstones_change; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_tombstones_change ON lash_durable_read_fixture.lash_process_tombstones USING btree (pruned_change_seq);


--
-- Name: idx_lash_processes_change_seq; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_change_seq ON lash_durable_read_fixture.lash_processes USING btree (change_seq);


--
-- Name: idx_lash_processes_created; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_created ON lash_durable_read_fixture.lash_processes USING btree (created_at_ms);


--
-- Name: idx_lash_processes_identity; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_identity ON lash_durable_read_fixture.lash_processes USING btree (identity_kind, identity_label);


--
-- Name: idx_lash_processes_lifetime_pending; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_lifetime_pending ON lash_durable_read_fixture.lash_processes USING btree (lifetime_scope_kind, lifetime_scope_id, process_id) WHERE ((lifetime = 'until'::text) AND (cancel_requested_at_ms IS NULL) AND (status = ANY (ARRAY['running'::text, 'waiting'::text])));


--
-- Name: idx_lash_processes_lifetime_scope; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_lifetime_scope ON lash_durable_read_fixture.lash_processes USING btree (lifetime_scope_kind, lifetime_scope_id, process_id);


--
-- Name: idx_lash_processes_live_worklist; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_live_worklist ON lash_durable_read_fixture.lash_processes USING btree (process_id) WHERE (status = ANY (ARRAY['running'::text, 'waiting'::text]));


--
-- Name: idx_lash_processes_originator; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_originator ON lash_durable_read_fixture.lash_processes USING btree (originator_id);


--
-- Name: idx_lash_processes_park_executable_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_park_executable_generation ON lash_durable_read_fixture.lash_processes USING btree (park_executable_generation) WHERE (park_executable_generation IS NOT NULL);


--
-- Name: idx_lash_processes_parked; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_parked ON lash_durable_read_fixture.lash_processes USING btree (parked_since_ms, process_id) WHERE (parked_since_ms IS NOT NULL);


--
-- Name: idx_lash_processes_pending_cancel; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_pending_cancel ON lash_durable_read_fixture.lash_processes USING btree (cancel_requested_at_ms, process_id) WHERE ((cancel_requested_at_ms IS NOT NULL) AND (status <> ALL (ARRAY['completed'::text, 'failed'::text, 'cancelled'::text, 'abandoned'::text])));


--
-- Name: idx_lash_processes_start_key; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_processes_start_key ON lash_durable_read_fixture.lash_processes USING btree (start_key) WHERE (start_key IS NOT NULL);


--
-- Name: idx_lash_processes_status; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_status ON lash_durable_read_fixture.lash_processes USING btree (status);


--
-- Name: idx_lash_processes_updated; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_updated ON lash_durable_read_fixture.lash_processes USING btree (updated_at_ms);


--
-- Name: idx_lash_processes_wake_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_wake_session ON lash_durable_read_fixture.lash_processes USING btree (wake_session_id);


--
-- Name: idx_lash_queued_work_claim; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_claim ON lash_durable_read_fixture.lash_queued_work_batches USING btree (session_id, claim_id, enqueue_seq);


--
-- Name: idx_lash_queued_work_ready; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_ready ON lash_durable_read_fixture.lash_queued_work_batches USING btree (session_id, available_at_ms, enqueue_seq);


--
-- Name: idx_lash_queued_work_session_command_order; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_session_command_order ON lash_durable_read_fixture.lash_queued_work_batches USING btree (session_id, work_kind, enqueued_at_ms, enqueue_seq);


--
-- Name: idx_lash_session_ingress_addressed; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_ingress_addressed ON lash_durable_read_fixture.lash_session_ingress USING btree (session_id, delivery_turn_id, enqueue_seq) WHERE ((delivery_turn_id IS NOT NULL) AND (state = ANY (ARRAY['open'::text, 'accepted'::text])));


--
-- Name: idx_lash_session_ingress_claim; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_ingress_claim ON lash_durable_read_fixture.lash_session_ingress USING btree (session_id, claim_id) WHERE (claim_id IS NOT NULL);


--
-- Name: idx_lash_session_ingress_open; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_ingress_open ON lash_durable_read_fixture.lash_session_ingress USING btree (session_id, lane, enqueue_seq) WHERE (state = ANY (ARRAY['open'::text, 'accepted'::text]));


--
-- Name: idx_lash_session_meta_catalog; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_meta_catalog ON lash_durable_read_fixture.lash_session_meta USING btree (created_at_ms, session_id);


--
-- Name: idx_lash_session_meta_state_version; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_meta_state_version ON lash_durable_read_fixture.lash_session_meta USING btree (session_state_version, session_id);


--
-- Name: idx_lash_sessions_checkpoint_ref; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_sessions_checkpoint_ref ON lash_durable_read_fixture.lash_sessions USING btree (checkpoint_ref);


--
-- Name: idx_lash_sessions_leaf; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_sessions_leaf ON lash_durable_read_fixture.lash_sessions USING btree (leaf_node_id);


--
-- Name: idx_lash_tool_intent_submissions_scope; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_tool_intent_submissions_scope ON lash_durable_read_fixture.lash_tool_intent_submissions USING btree (session_id, execution_scope_id, intent_index);


--
-- Name: idx_lash_trigger_deliveries_process; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_deliveries_process ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (process_id);


--
-- Name: idx_lash_trigger_deliveries_subscription; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_deliveries_subscription ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (subscription_id);


--
-- Name: idx_lash_trigger_occurrences_reclaimable; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_occurrences_reclaimable ON lash_durable_read_fixture.lash_trigger_occurrences USING btree (reclaimable_at_ms, occurrence_id) WHERE (reclaimable_at_ms IS NOT NULL);


--
-- Name: idx_lash_trigger_occurrences_source; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_occurrences_source ON lash_durable_read_fixture.lash_trigger_occurrences USING btree (source_type, source_key, occurred_at_ms);


--
-- Name: idx_lash_trigger_subscriptions_registrant; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_subscriptions_registrant ON lash_durable_read_fixture.lash_trigger_subscriptions USING btree (owner_scope, subscription_key);


--
-- Name: idx_lash_trigger_subscriptions_source; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_subscriptions_source ON lash_durable_read_fixture.lash_trigger_subscriptions USING btree (source_type, source_key, lifecycle);


--
-- Name: idx_lash_turn_parks_executable_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_turn_parks_executable_generation ON lash_durable_read_fixture.lash_turn_parks USING btree (park_executable_generation) WHERE (park_executable_generation IS NOT NULL);


--
-- Name: idx_lash_turn_parks_since; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_turn_parks_since ON lash_durable_read_fixture.lash_turn_parks USING btree (since_ms, session_id);


--
-- Name: idx_lash_wake_deliveries_group_sequence; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_wake_deliveries_group_sequence ON lash_durable_read_fixture.lash_process_wake_deliveries USING btree (target_session_id, process_id, sequence) WHERE (state <> 'enqueued'::text);


--
-- Name: idx_lash_wake_deliveries_pending; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_wake_deliveries_pending ON lash_durable_read_fixture.lash_process_wake_deliveries USING btree (next_attempt_at_ms, target_session_id, process_id, sequence) WHERE (state = ANY (ARRAY['pending'::text, 'enqueuing'::text]));


--
-- Name: lash_queued_runs_pending; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX lash_queued_runs_pending ON lash_durable_read_fixture.lash_queued_runs USING btree (session_id) WHERE (status = 'pending'::text);


--
-- Name: lash_artifact_owners lash_artifact_owners_namespace_artifact_ref_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_artifact_owners
    ADD CONSTRAINT lash_artifact_owners_namespace_artifact_ref_fkey FOREIGN KEY (namespace, artifact_ref) REFERENCES lash_durable_read_fixture.lash_lashlang_artifacts(namespace, artifact_ref) ON DELETE CASCADE;


--
-- Name: lash_checkpoint_blob_refs lash_checkpoint_blob_refs_blob_ref_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_checkpoint_blob_refs
    ADD CONSTRAINT lash_checkpoint_blob_refs_blob_ref_fkey FOREIGN KEY (blob_ref) REFERENCES lash_durable_read_fixture.lash_blobs(hash);


--
-- Name: lash_checkpoint_blob_refs lash_checkpoint_blob_refs_checkpoint_ref_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_checkpoint_blob_refs
    ADD CONSTRAINT lash_checkpoint_blob_refs_checkpoint_ref_fkey FOREIGN KEY (checkpoint_ref) REFERENCES lash_durable_read_fixture.lash_blobs(hash) ON DELETE CASCADE;


--
-- Name: lash_process_artifact_cleanup lash_process_artifact_cleanup_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_artifact_cleanup
    ADD CONSTRAINT lash_process_artifact_cleanup_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_process_tombstones(process_id) ON DELETE RESTRICT;


--
-- Name: lash_process_events lash_process_events_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_events
    ADD CONSTRAINT lash_process_events_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


--
-- Name: lash_process_leases lash_process_leases_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_leases
    ADD CONSTRAINT lash_process_leases_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


--
-- Name: lash_process_observers lash_process_observers_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_observers
    ADD CONSTRAINT lash_process_observers_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


--
-- Name: lash_process_segment_handovers lash_process_segment_handovers_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_segment_handovers
    ADD CONSTRAINT lash_process_segment_handovers_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


--
-- Name: lash_process_wake_deliveries lash_process_wake_deliveries_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_wake_deliveries
    ADD CONSTRAINT lash_process_wake_deliveries_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


--
-- Name: lash_queued_run_members lash_queued_run_members_session_id_scope_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_run_members
    ADD CONSTRAINT lash_queued_run_members_session_id_scope_id_fkey FOREIGN KEY (session_id, scope_id) REFERENCES lash_durable_read_fixture.lash_queued_runs(session_id, scope_id);


--
-- Name: lash_queued_work_items lash_queued_work_items_batch_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_work_items
    ADD CONSTRAINT lash_queued_work_items_batch_id_fkey FOREIGN KEY (batch_id) REFERENCES lash_durable_read_fixture.lash_queued_work_batches(batch_id) ON DELETE CASCADE;


--
-- Name: lash_session_meta_pending_observer_intents lash_session_meta_pending_observer_intents_session_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_meta_pending_observer_intents
    ADD CONSTRAINT lash_session_meta_pending_observer_intents_session_id_fkey FOREIGN KEY (session_id) REFERENCES lash_durable_read_fixture.lash_session_meta(session_id) ON DELETE CASCADE;


--
-- Name: lash_trigger_deliveries lash_trigger_deliveries_occurrence_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_deliveries
    ADD CONSTRAINT lash_trigger_deliveries_occurrence_id_fkey FOREIGN KEY (occurrence_id) REFERENCES lash_durable_read_fixture.lash_trigger_occurrences(occurrence_id) ON DELETE CASCADE;


--
-- Name: lash_turn_cancel_affected_inputs lash_turn_cancel_affected_inputs_session_id_turn_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_affected_inputs
    ADD CONSTRAINT lash_turn_cancel_affected_inputs_session_id_turn_id_fkey FOREIGN KEY (session_id, turn_id) REFERENCES lash_durable_read_fixture.lash_turn_cancel_requests(session_id, turn_id) ON DELETE CASCADE;


--
-- PostgreSQL database dump complete
--
