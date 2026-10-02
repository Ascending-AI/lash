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
-- Name: lash_abandoned_consumer_holds; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_abandoned_consumer_holds (
    hold_key text NOT NULL COLLATE pg_catalog."C",
    owner_scope_kind text NOT NULL,
    owner_scope_id text NOT NULL COLLATE pg_catalog."C",
    abandoned_at_ms bigint NOT NULL
);


--
-- Name: lash_artifact_cleanup_obligations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_artifact_cleanup_obligations (
    referrer_kind text NOT NULL,
    referrer_id text NOT NULL,
    cleanup_json text NOT NULL,
    obligation_id text NOT NULL,
    obligation_state text DEFAULT 'due'::text NOT NULL,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_artifact_cleanup_obligations_id CHECK ((char_length(referrer_id) > 0)),
    CONSTRAINT ck_artifact_cleanup_obligations_kind CHECK ((referrer_kind = ANY (ARRAY['frame_environment'::text, 'process_record'::text, 'subscription_revision'::text, 'start'::text, 'start_input'::text, 'execution'::text, 'host_pin'::text, 'session'::text, 'upload'::text]))),
    CONSTRAINT ck_artifact_cleanup_obligations_obligation CHECK (((((obligation_state = 'due'::text) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE))
);


--
-- Name: lash_artifact_referrer_edges; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_artifact_referrer_edges (
    namespace text NOT NULL,
    artifact_ref text NOT NULL,
    referrer_kind text NOT NULL,
    referrer_id text NOT NULL,
    CONSTRAINT ck_artifact_referrer_edges_id CHECK ((char_length(referrer_id) > 0)),
    CONSTRAINT ck_artifact_referrer_edges_kind CHECK ((referrer_kind = ANY (ARRAY['frame_environment'::text, 'process_record'::text, 'subscription_revision'::text, 'start'::text, 'execution'::text, 'host_pin'::text])))
);


--
-- Name: lash_attachment_condemnations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_condemnations (
    attachment_id text NOT NULL,
    phase text NOT NULL,
    write_token text,
    next_delete_at_ms bigint DEFAULT 0 NOT NULL,
    sweep_generation bigint NOT NULL,
    delete_attempts integer DEFAULT 0 NOT NULL,
    last_delete_error text,
    stall_reason text,
    CONSTRAINT ck_attachment_condemnations_delete_attempts CHECK ((delete_attempts >= 0)),
    CONSTRAINT ck_attachment_condemnations_failure_pairing CHECK (((delete_attempts = 0) = (last_delete_error IS NULL))),
    CONSTRAINT ck_attachment_condemnations_next_delete CHECK ((next_delete_at_ms >= 0)),
    CONSTRAINT ck_attachment_condemnations_phase CHECK ((phase = ANY (ARRAY['condemned'::text, 'deleting'::text]))),
    CONSTRAINT ck_attachment_condemnations_stall_attempts CHECK (((stall_reason IS NULL) OR (delete_attempts > 0))),
    CONSTRAINT ck_attachment_condemnations_stall_reason CHECK ((stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text]))),
    CONSTRAINT ck_attachment_condemnations_write_token_phase CHECK (((write_token IS NULL) OR (phase = 'condemned'::text)))
);


--
-- Name: lash_attachment_pending_writes; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_pending_writes (
    write_id text NOT NULL,
    attachment_id text NOT NULL,
    referrer_kind text NOT NULL,
    referrer_id text NOT NULL,
    begun_at_ms bigint NOT NULL,
    CONSTRAINT ck_attachment_pending_writes_attachment CHECK ((char_length(attachment_id) > 0)),
    CONSTRAINT ck_attachment_pending_writes_id CHECK ((char_length(referrer_id) > 0)),
    CONSTRAINT ck_attachment_pending_writes_kind CHECK ((referrer_kind = ANY (ARRAY['session'::text, 'upload'::text, 'execution'::text, 'start_input'::text, 'process_record'::text]))),
    CONSTRAINT ck_attachment_pending_writes_write_id CHECK ((char_length(write_id) = 32))
);


--
-- Name: lash_attachment_referrer_edges; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_referrer_edges (
    attachment_id text NOT NULL,
    referrer_kind text NOT NULL,
    referrer_id text NOT NULL,
    CONSTRAINT ck_attachment_referrer_edges_attachment CHECK ((char_length(attachment_id) > 0)),
    CONSTRAINT ck_attachment_referrer_edges_id CHECK ((char_length(referrer_id) > 0)),
    CONSTRAINT ck_attachment_referrer_edges_kind CHECK ((referrer_kind = ANY (ARRAY['session'::text, 'upload'::text, 'execution'::text, 'start_input'::text, 'process_record'::text])))
);


--
-- Name: lash_attachment_sweep_clock; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_sweep_clock (
    singleton boolean DEFAULT true NOT NULL,
    generation bigint NOT NULL,
    CONSTRAINT ck_attachment_sweep_clock_singleton CHECK (singleton)
);


--
-- Name: lash_attachment_uploads; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_uploads (
    attachment_id text NOT NULL,
    written_at_ms bigint NOT NULL,
    CONSTRAINT ck_attachment_uploads_attachment CHECK ((char_length(attachment_id) > 0))
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
    created_at_ms bigint NOT NULL,
    engine_ref text,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_control_intents_kind CHECK ((kind = ANY (ARRAY['redrive'::text, 'cancel'::text, 'fork'::text, 'close_session'::text]))),
    CONSTRAINT ck_control_intents_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
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
-- Name: lash_draining_generations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_draining_generations (
    generation text NOT NULL,
    marked_at_ms bigint NOT NULL
);


--
-- Name: lash_fleet_format; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_fleet_format (
    singleton boolean DEFAULT true NOT NULL,
    format_version integer NOT NULL,
    finalize_hold_reason text,
    finalize_held_at_ms bigint,
    CONSTRAINT ck_fleet_format_finalize_hold CHECK (((finalize_hold_reason IS NULL) = (finalize_held_at_ms IS NULL))),
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
    body_bytes bigint NOT NULL,
    tombstoned boolean DEFAULT false NOT NULL,
    CONSTRAINT ck_graph_nodes_body_bytes CHECK ((body_bytes >= 0)),
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
    backfill_cursor text,
    backfill_rows bigint,
    CONSTRAINT ck_lash_migrations_backfill_progress CHECK ((((phase = 'backfill'::text) AND (backfill_rows IS NOT NULL) AND (backfill_rows >= 0)) OR ((phase <> 'backfill'::text) AND (backfill_cursor IS NULL) AND (backfill_rows IS NULL)))),
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
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_parent_end_plans_kind CHECK ((parent_kind = ANY (ARRAY['turn'::text, 'session_operation'::text, 'process'::text, 'session'::text]))),
    CONSTRAINT ck_parent_end_plans_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE))
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
    submission_digest text NOT NULL,
    enqueued_at_ms bigint NOT NULL,
    admitted_root text,
    admitted_by text,
    run_spec_hash text,
    terminal_at_ms bigint,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_pending_turn_inputs_admission_all_or_none CHECK (((admitted_root IS NULL) = (admitted_by IS NULL))),
    CONSTRAINT ck_pending_turn_inputs_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
    CONSTRAINT ck_pending_turn_inputs_settled_unadmitted CHECK (((admitted_root IS NULL) OR (state <> ALL (ARRAY['cancelled'::text, 'completed'::text])))),
    CONSTRAINT ck_pending_turn_inputs_state CHECK ((state = ANY (ARRAY['pending_active'::text, 'deferred_next_turn'::text, 'accepted'::text, 'cancelled'::text, 'completed'::text]))),
    CONSTRAINT ck_pending_turn_inputs_state_ingress CHECK ((((((ingress_json)::jsonb ->> 'scope'::text) = 'active_turn'::text) AND (state = ANY (ARRAY['pending_active'::text, 'accepted'::text, 'cancelled'::text, 'completed'::text]))) OR ((((ingress_json)::jsonb ->> 'scope'::text) = 'next_turn'::text) AND (state = ANY (ARRAY['deferred_next_turn'::text, 'cancelled'::text, 'completed'::text]))))),
    CONSTRAINT ck_pending_turn_inputs_terminal_at CHECK (((state = ANY (ARRAY['cancelled'::text, 'completed'::text])) = (terminal_at_ms IS NOT NULL)))
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
    park_build_generation text,
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
    started_json text,
    written_generation text,
    route text NOT NULL
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
    CONSTRAINT ck_process_wake_deliveries_discard_reason CHECK ((discard_reason = ANY (ARRAY['expired'::text, 'target_gone'::text, 'retargeted'::text, 'sequence_rewound'::text, 'source_unreadable'::text, 'content_conflict'::text]))),
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
    park_build_generation text,
    segment_generation text,
    record_json text NOT NULL,
    start_obligation_id text,
    start_obligation_state text,
    start_obligation_attempts integer DEFAULT 0 NOT NULL,
    start_obligation_due_at_ms bigint,
    start_obligation_claim_token text,
    start_obligation_stall_reason text,
    start_obligation_last_error text,
    start_obligation_settled_at_ms bigint,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    consumer_hold_key text,
    consumer_hold_scope_kind text,
    consumer_hold_scope_id text COLLATE pg_catalog."C",
    consumer_hold_cancels boolean,
    trigger_delivery_pin_occurrence_id text,
    trigger_delivery_pin_subscription_id text,
    CONSTRAINT ck_processes_consumer_hold CHECK ((((consumer_hold_key IS NULL) = (consumer_hold_scope_kind IS NULL)) AND ((consumer_hold_key IS NULL) = (consumer_hold_scope_id IS NULL)))),
    CONSTRAINT ck_processes_lifetime CHECK ((lifetime = ANY (ARRAY['until'::text, 'detached'::text]))),
    CONSTRAINT ck_processes_lifetime_scope CHECK ((((lifetime = 'detached'::text) AND (lifetime_scope_kind IS NULL) AND (lifetime_scope_id IS NULL)) OR ((lifetime = 'until'::text) AND (lifetime_scope_kind = ANY (ARRAY['turn'::text, 'session_operation'::text, 'process'::text, 'session'::text])) AND (lifetime_scope_id IS NOT NULL)))),
    CONSTRAINT ck_processes_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
    CONSTRAINT ck_processes_parked CHECK (((parked_since_ms IS NULL) = (parked_reason_code IS NULL))),
    CONSTRAINT ck_processes_start_obligation CHECK (((((start_obligation_state IS NULL) AND (start_obligation_id IS NULL) AND (start_obligation_due_at_ms IS NULL) AND (start_obligation_claim_token IS NULL) AND (start_obligation_stall_reason IS NULL) AND (start_obligation_settled_at_ms IS NULL)) OR ((start_obligation_state = 'due'::text) AND (start_obligation_id IS NOT NULL) AND (start_obligation_due_at_ms IS NOT NULL) AND (start_obligation_claim_token IS NULL) AND (start_obligation_stall_reason IS NULL) AND (start_obligation_settled_at_ms IS NULL)) OR ((start_obligation_state = 'claimed'::text) AND (start_obligation_id IS NOT NULL) AND (start_obligation_due_at_ms IS NOT NULL) AND (start_obligation_claim_token IS NOT NULL) AND (start_obligation_stall_reason IS NULL) AND (start_obligation_settled_at_ms IS NULL)) OR ((start_obligation_state = 'delivered'::text) AND (start_obligation_id IS NOT NULL) AND (start_obligation_due_at_ms IS NULL) AND (start_obligation_claim_token IS NULL) AND (start_obligation_stall_reason IS NULL) AND (start_obligation_settled_at_ms IS NOT NULL)) OR ((start_obligation_state = 'stalled'::text) AND (start_obligation_id IS NOT NULL) AND (start_obligation_due_at_ms IS NULL) AND (start_obligation_claim_token IS NULL) AND (start_obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (start_obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
    CONSTRAINT ck_processes_status CHECK ((status = ANY (ARRAY['running'::text, 'waiting'::text, 'completed'::text, 'failed'::text, 'cancelled'::text, 'abandoned'::text, 'caller_departed'::text]))),
    CONSTRAINT ck_processes_trigger_delivery_pin CHECK (((trigger_delivery_pin_occurrence_id IS NULL) = (trigger_delivery_pin_subscription_id IS NULL)))
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
    enqueued_at_ms bigint NOT NULL,
    submission_digest text NOT NULL,
    settled_operation_key text,
    admitted_root text,
    admitted_by text,
    terminal_cause text,
    terminal_at_ms bigint,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_queued_work_batches_admission_all_or_none CHECK (((admitted_root IS NULL) = (admitted_by IS NULL))),
    CONSTRAINT ck_queued_work_batches_delivery_policy CHECK ((delivery_policy = ANY (ARRAY['earliest_safe_boundary'::text, 'after_current_turn_commit'::text]))),
    CONSTRAINT ck_queued_work_batches_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
    CONSTRAINT ck_queued_work_batches_terminal CHECK ((((terminal_cause IS NULL) AND (terminal_at_ms IS NULL)) OR ((terminal_cause = ANY (ARRAY['delivered'::text, 'applied'::text, 'cancelled'::text, 'stale_config_revision'::text])) AND (terminal_at_ms IS NOT NULL) AND (admitted_root IS NULL)))),
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
-- Name: lash_recovery_leader; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_recovery_leader (
    name text NOT NULL,
    holder_id text NOT NULL,
    generation_rank bigint NOT NULL,
    term bigint NOT NULL,
    elected_at_ms bigint NOT NULL,
    expires_at_ms bigint NOT NULL
);


--
-- Name: lash_referrer_fences; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_referrer_fences (
    referrer_kind text NOT NULL,
    referrer_id text NOT NULL,
    ended_at_ms bigint NOT NULL,
    CONSTRAINT ck_referrer_fences_id CHECK ((char_length(referrer_id) > 0)),
    CONSTRAINT ck_referrer_fences_kind CHECK ((referrer_kind = ANY (ARRAY['frame_environment'::text, 'process_record'::text, 'subscription_revision'::text, 'start'::text, 'start_input'::text, 'execution'::text, 'host_pin'::text, 'session'::text, 'upload'::text])))
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
    outcome_code text,
    committed_at_ms bigint NOT NULL,
    failure_evidence boolean NOT NULL,
    request_identity_hash text,
    requested_node_count bigint,
    identity_encoding_version integer,
    CONSTRAINT ck_runtime_turn_commits_identity CHECK ((((request_identity_hash IS NULL) = (identity_encoding_version IS NULL)) AND ((requested_node_count IS NULL) OR (request_identity_hash IS NOT NULL)))),
    CONSTRAINT ck_runtime_turn_commits_outcome CHECK ((outcome_code = ANY (ARRAY['completed'::text, 'frame_switch'::text, 'cancelled'::text, 'failed_incomplete'::text, 'failed_invalid_input'::text, 'failed_max_turns'::text, 'failed_tool_failure'::text, 'failed_provider_error'::text, 'failed_context_overflow'::text, 'failed_plugin_abort'::text, 'failed_runtime_error'::text, 'failed_submitted_error'::text, 'failed_tool_error'::text])))
);


--
-- Name: lash_schema_versions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_schema_versions (
    component text NOT NULL,
    version integer NOT NULL,
    min_reader integer NOT NULL,
    CONSTRAINT ck_lash_schema_versions_stamp CHECK (((version >= 1) AND (min_reader >= 1) AND (min_reader <= version)))
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
    owning_process_id text,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_session_meta_caused_by_family CHECK ((((caused_by_kind IS NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'turn'::text) AND (caused_by_session_id IS NOT NULL) AND (caused_by_turn_id IS NOT NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'effect_address'::text) AND (caused_by_effect_id IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'tool_call'::text) AND (caused_by_session_id IS NOT NULL) AND (caused_by_call_id IS NOT NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'process'::text) AND (caused_by_process_id IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'process_event'::text) AND (caused_by_process_id IS NOT NULL) AND (caused_by_process_event_sequence IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'trigger_occurrence'::text) AND (caused_by_occurrence_id IS NOT NULL) AND (caused_by_session_id IS NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_node_id IS NULL)) OR ((caused_by_kind = 'session_node'::text) AND (caused_by_session_id IS NOT NULL) AND (caused_by_node_id IS NOT NULL) AND (caused_by_turn_id IS NULL) AND (caused_by_effect_id IS NULL) AND (caused_by_call_id IS NULL) AND (caused_by_process_id IS NULL) AND (caused_by_process_event_sequence IS NULL) AND (caused_by_occurrence_id IS NULL) AND (caused_by_subscription_id IS NULL) AND (caused_by_subscription_incarnation IS NULL) AND (caused_by_subscription_revision IS NULL)) OR ((caused_by_kind IS NOT NULL) AND (NOT (caused_by_kind = ANY (ARRAY['turn'::text, 'effect_address'::text, 'tool_call'::text, 'process'::text, 'process_event'::text, 'trigger_occurrence'::text, 'session_node'::text])))))),
    CONSTRAINT ck_session_meta_caused_by_kind CHECK ((caused_by_kind = ANY (ARRAY['turn'::text, 'effect_address'::text, 'tool_call'::text, 'process'::text, 'process_event'::text, 'trigger_occurrence'::text, 'session_node'::text]))),
    CONSTRAINT ck_session_meta_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
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
    admission_json text,
    admitted_generation text,
    terminal_kind text,
    terminal_cause_json text,
    terminal_head_revision bigint,
    terminal_at_ms bigint,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_session_roots_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE)),
    CONSTRAINT ck_session_roots_terminal CHECK ((((terminal_kind IS NULL) AND (terminal_cause_json IS NULL) AND (terminal_head_revision IS NULL) AND (terminal_at_ms IS NULL)) OR ((terminal_kind = ANY (ARRAY['answered'::text, 'failed'::text, 'cancelled'::text])) AND (terminal_cause_json IS NOT NULL) AND (terminal_at_ms IS NOT NULL))))
);


--
-- Name: lash_session_run_specs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_run_specs (
    session_id text NOT NULL,
    spec_hash text NOT NULL,
    spec_json text NOT NULL
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
    owner text NOT NULL,
    execution_scope_id text NOT NULL,
    tool_call_id text NOT NULL,
    intent_index bigint NOT NULL,
    kind text NOT NULL,
    payload_hash text NOT NULL,
    submission_json text NOT NULL,
    CONSTRAINT ck_tool_intent_submissions_kind CHECK ((kind = ANY (ARRAY['start_process'::text, 'signal_process'::text, 'cancel_process'::text, 'emit_process_event'::text, 'emit_trigger'::text, 'publish_definition'::text, 'get_definition'::text, 'register_trigger'::text])))
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
    created_at_ms bigint NOT NULL,
    obligation_id text,
    obligation_state text,
    obligation_attempts integer DEFAULT 0 NOT NULL,
    obligation_due_at_ms bigint,
    obligation_claim_token text,
    obligation_stall_reason text,
    obligation_last_error text,
    obligation_settled_at_ms bigint,
    CONSTRAINT ck_trigger_deliveries_obligation CHECK (((((obligation_state IS NULL) AND (obligation_id IS NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'due'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'claimed'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NOT NULL) AND (obligation_claim_token IS NOT NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NULL)) OR ((obligation_state = 'delivered'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason IS NULL) AND (obligation_settled_at_ms IS NOT NULL)) OR ((obligation_state = 'stalled'::text) AND (obligation_id IS NOT NULL) AND (obligation_due_at_ms IS NULL) AND (obligation_claim_token IS NULL) AND (obligation_stall_reason = ANY (ARRAY['attempts_exhausted'::text, 'refused'::text, 'undecodable'::text])) AND (obligation_settled_at_ms IS NOT NULL))) IS TRUE))
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
-- Name: lash_trigger_occurrence_tombstones; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_trigger_occurrence_tombstones (
    occurrence_id text NOT NULL,
    reclaimed_at_ms bigint NOT NULL
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
    item_kind text NOT NULL,
    batch_id text,
    CONSTRAINT ck_turn_cancel_affected_inputs_disposition CHECK ((disposition = ANY (ARRAY['defer'::text, 'drop'::text]))),
    CONSTRAINT ck_turn_cancel_affected_inputs_item_kind CHECK ((((item_kind = 'input'::text) AND (batch_id IS NULL)) OR ((item_kind = 'process_wake'::text) AND (batch_id IS NOT NULL))))
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
    park_build_generation text,
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
    park_build_generation text,
    CONSTRAINT ck_turn_parks_attempts CHECK ((attempts >= 1))
);


--
-- Name: lash_usage_facts; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_usage_facts (
    seq bigint NOT NULL,
    owner_kind text NOT NULL,
    owner_id text NOT NULL,
    effect_key text NOT NULL,
    call_ordinal bigint NOT NULL,
    provider_attempt bigint NOT NULL,
    fact_kind text NOT NULL,
    disposition text NOT NULL,
    run_id text,
    llm_call_id text NOT NULL,
    source text NOT NULL,
    model_key text NOT NULL,
    requested_model text NOT NULL,
    served_model text,
    input_tokens bigint NOT NULL,
    output_tokens bigint NOT NULL,
    cache_read_input_tokens bigint NOT NULL,
    cache_write_input_tokens bigint NOT NULL,
    reasoning_output_tokens bigint NOT NULL,
    generation_id text,
    payload_hash text NOT NULL,
    recorded_at_ms bigint NOT NULL,
    CONSTRAINT ck_usage_facts_call_ordinal CHECK ((call_ordinal >= 0)),
    CONSTRAINT ck_usage_facts_kind CHECK ((((fact_kind = 'attempt'::text) AND (disposition = ANY (ARRAY['reported'::text, 'unreported'::text])) AND (run_id IS NOT NULL)) OR ((fact_kind = 'correction'::text) AND (disposition = 'reconciled'::text) AND (run_id IS NULL) AND (generation_id IS NOT NULL)))),
    CONSTRAINT ck_usage_facts_owner_kind CHECK ((owner_kind = ANY (ARRAY['session'::text, 'process'::text]))),
    CONSTRAINT ck_usage_facts_provider_attempt CHECK ((provider_attempt >= 0)),
    CONSTRAINT ck_usage_facts_unreported_zero CHECK (((disposition <> 'unreported'::text) OR ((input_tokens = 0) AND (output_tokens = 0) AND (cache_read_input_tokens = 0) AND (cache_write_input_tokens = 0) AND (reasoning_output_tokens = 0))))
);


--
-- Name: lash_usage_facts_seq_seq; Type: SEQUENCE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE SEQUENCE lash_durable_read_fixture.lash_usage_facts_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: lash_usage_facts_seq_seq; Type: SEQUENCE OWNED BY; Schema: lash_durable_read_fixture; Owner: -
--

ALTER SEQUENCE lash_durable_read_fixture.lash_usage_facts_seq_seq OWNED BY lash_durable_read_fixture.lash_usage_facts.seq;


--
-- Name: lash_usage_owner_retirements; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_usage_owner_retirements (
    owner_kind text NOT NULL,
    owner_id text NOT NULL,
    retired_at_ms bigint NOT NULL,
    CONSTRAINT ck_usage_owner_retirements_owner_kind CHECK ((owner_kind = ANY (ARRAY['session'::text, 'process'::text])))
);


--
-- Name: lash_usage_runs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_usage_runs (
    owner_kind text NOT NULL,
    owner_id text NOT NULL,
    effect_key text NOT NULL,
    run_id text NOT NULL,
    execution_scope_key text NOT NULL,
    source text NOT NULL,
    model_key text NOT NULL,
    requested_model text NOT NULL,
    admitted_at_ms bigint NOT NULL,
    state text NOT NULL,
    unknown_reason text,
    conflict_detail text,
    resolved_at_ms bigint,
    CONSTRAINT ck_usage_runs_owner_kind CHECK ((owner_kind = ANY (ARRAY['session'::text, 'process'::text]))),
    CONSTRAINT ck_usage_runs_state CHECK ((((state = 'open'::text) AND (unknown_reason IS NULL) AND (conflict_detail IS NULL) AND (resolved_at_ms IS NULL)) OR ((state = 'settled'::text) AND (unknown_reason IS NULL) AND (conflict_detail IS NULL) AND (resolved_at_ms IS NOT NULL)) OR ((state = 'unknown'::text) AND (conflict_detail IS NULL) AND (resolved_at_ms IS NOT NULL) AND (unknown_reason = ANY (ARRAY['superseded_run'::text, 'call_without_record'::text, 'facts_unjournalable'::text, 'execution_ended'::text, 'owner_retired'::text]))) OR ((state = 'conflicted'::text) AND (unknown_reason IS NULL) AND (conflict_detail IS NOT NULL) AND (resolved_at_ms IS NOT NULL))))
);


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
-- Name: lash_worker_recovery; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_worker_recovery (
    scope_id text NOT NULL,
    revision bigint NOT NULL,
    attempts integer NOT NULL,
    cpu_nanos bigint NOT NULL,
    replacement integer NOT NULL,
    unknown_cpu_attempts integer NOT NULL,
    in_flight integer NOT NULL
);


--
-- Name: lash_control_intents intent_id; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_control_intents ALTER COLUMN intent_id SET DEFAULT nextval('lash_durable_read_fixture.lash_control_intents_intent_id_seq'::regclass);


--
-- Name: lash_usage_facts seq; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_facts ALTER COLUMN seq SET DEFAULT nextval('lash_durable_read_fixture.lash_usage_facts_seq_seq'::regclass);


--
-- Data for Name: lash_abandoned_consumer_holds; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_artifact_cleanup_obligations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_artifact_cleanup_obligations VALUES ('session', 'durable-read-deleted-session', '{"referrer":{"kind":"session","id":"durable-read-deleted-session"},"plan":{"plan":"await_session_graph_retired"},"gate":null}', 'artifact_cleanup:0fe6e3988fc046abae32a0f2c9ceb7ba', 'due', 0, 1790908621661, NULL, NULL, NULL, NULL);
INSERT INTO lash_durable_read_fixture.lash_artifact_cleanup_obligations VALUES ('process_record', 'p_00000000000070008000000000000003', '{"referrer":{"kind":"process_record","id":"p_00000000000070008000000000000003"},"plan":{"plan":"ended","carries":[]},"gate":null}', 'artifact_cleanup:54033b5d0fa34a8586458fa5a2d80b00', 'due', 0, 1700000000000, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_artifact_referrer_edges; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_artifact_referrer_edges VALUES ('process_execution_env', 'process-env:v6:blake3:a2be1511dbfc20a2df62205822240227fb1c8a05705837fdc0db9aacd95dc45e', 'host_pin', 'host-pin:v1:8b30350c0ff04a81b4560896e4f43442');


--
-- Data for Name: lash_attachment_condemnations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_attachment_pending_writes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_attachment_pending_writes VALUES ('88888888888848888888888888888888', 'durable-read-attachment', 'session', 'durable-read-fixture', 1700000000000);


--
-- Data for Name: lash_attachment_referrer_edges; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_attachment_referrer_edges VALUES ('durable-read-attachment', 'session', 'durable-read-fixture');


--
-- Data for Name: lash_attachment_sweep_clock; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_attachment_sweep_clock VALUES (true, 0);


--
-- Data for Name: lash_attachment_uploads; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_attachment_uploads VALUES ('durable-read-attachment', 1700000000000);


--
-- Data for Name: lash_blobs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('4b6f50c438715ac5352323ecc47b93309f9bca88916c6dfd6fc3a53bda42b891', '\x82ae736368656d615f76657273696f6e01aa7475726e5f737461746582aa7475726e5f696e64657800ab746f6b656e5f757361676585ac696e7075745f746f6b656e7300ad6f75747075745f746f6b656e7300b763616368655f726561645f696e7075745f746f6b656e7300b863616368655f77726974655f696e7075745f746f6b656e7300b7726561736f6e696e675f6f75747075745f746f6b656e7300');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918', '\x83ae736368656d615f76657273696f6e01aa7475726e5f737461746582aa7475726e5f696e64657807ab746f6b656e5f757361676585ac696e7075745f746f6b656e730dad6f75747075745f746f6b656e7308b763616368655f726561645f696e7075745f746f6b656e7305b863616368655f77726974655f696e7075745f746f6b656e7303b7726561736f6e696e675f6f75747075745f746f6b656e7302aa636f6d706f6e656e747383af657865637574696f6e5f737461746582a8626c6f625f726566d94037366133316561393765313333616537656432333365333433353564386562353330386465613861623033316231623339613637393336666238626339396338b0656e636f64696e675f76657273696f6e01ac706c7567696e5f737461746582a8626c6f625f726566d94063363135356664663164333731613130613731613030373333373630366564356535616137386262653666346636373363303264353234363065323062323439b0656e636f64696e675f76657273696f6e01aa746f6f6c5f737461746582a8626c6f625f726566d94039623332393338663139643030636538653363633031313637363961383635393065396138623936333562646138346162353632373637316361363261353164b0656e636f64696e675f76657273696f6e01');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8', '\x464947383837');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d', '\x82aa67656e65726174696f6ecd0377a5746f6f6c7380');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249', '\x81bc64757261626c652d726561642d736e617073686f742d706c7567696e82aa67656e65726174696f6ecd0377a676616c75657381a5737461746582a766697874757265ac706c7567696e2d7374617465a576616c7565cd0377');


--
-- Data for Name: lash_catalog_identity; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_catalog_identity VALUES (true, '00000000-0000-4000-8000-000000000887');


--
-- Data for Name: lash_checkpoint_blob_refs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918', '76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8');
INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918', 'c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249');
INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918', '9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d');


--
-- Data for Name: lash_control_intents; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_deleted_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_deleted_sessions VALUES ('durable-read-deleted-session', 1700000000000, NULL, 0, 'root', NULL);


--
-- Data for Name: lash_draining_generations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_fleet_format; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_fleet_format VALUES (true, 1, NULL, NULL);


--
-- Data for Name: lash_fork_lineage; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_graph_nodes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f', NULL, 0, 'frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f', '{"schema_version":1,"timestamp":"2023-11-14T22:13:20+00:00","kind":"frame_open","frame_key":"frame-key/v2/3d5efb3ba3c6ff75d7f6ef7d84727fe3070416f76c9e8c0f7b055d946b343e2d","reason":"initial","assignment":{"policy":{"session_id":null,"autonomous":false,"turn_budget":"unbounded"},"plugin_config":{}}}', 299, false);
INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08', 'frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f', 1, 'frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f', '{"schema_version":1,"timestamp":"2023-11-14T22:13:20+00:00","kind":"event","event":{"Conversation":{"id":"m_append_9304aea3269b249d9b8e240f046976a16b2a95e618eb374edf1eded586a60e3c","role":"User","parts":[{"id":"m_append_9304aea3269b249d9b8e240f046976a16b2a95e618eb374edf1eded586a60e3c.p0","kind":"Text","content":"durable read user message"}],"origin":{"kind":"plugin","plugin_id":"plugin"}}}}', 393, false);
INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', 'n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08', 2, 'frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f', '{"schema_version":1,"timestamp":"2023-11-14T22:13:20+00:00","kind":"plugin","plugin_type":"durable-read-plugin","body":{"fixture":true,"order":2,"output":{"outcome":{"payload":{"$lash_tool_value":"untrusted_json","value":{"fixture":"raw"}},"status":"success"},"view":{"blocks":[{"text":"durable read authored view","type":"text"}]}}}}', 334, false);


--
-- Data for Name: lash_lashlang_artifacts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_lashlang_artifacts VALUES ('process_execution_env', 'process-env:v6:blake3:a2be1511dbfc20a2df62205822240227fb1c8a05705837fdc0db9aacd95dc45e', '\x7b22706c7567696e5f636f6e666967223a7b227265766973696f6e223a302c22636f6e666967223a7b7d7d2c22706f6c696379223a7b2273657373696f6e5f6964223a6e756c6c2c226175746f6e6f6d6f7573223a66616c73652c227475726e5f627564676574223a22756e626f756e646564227d2c2272656e646572223a7b2272656e64657265725f6964223a227374616e64617264222c22706172616d73223a7b226d61785f6368617273223a3132307d7d7d');


--
-- Data for Name: lash_migrations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_node_anchors; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_node_anchors VALUES ('n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', '36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918', 'durable-read-fixture');


--
-- Data for Name: lash_parent_end_plans; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_parent_end_plans VALUES ('process', 'process:34:p_00000000000070008000000000000003', '{"version":1,"scope":{"kind":"opener","scope":{"kind":"process","process_id":"p_00000000000070008000000000000003"}}}', 1700000000000, NULL, 'parent_end:00000000000040008000000000000887', 'due', 0, 0, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_pending_turn_inputs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_pending_turn_inputs VALUES (2, 'durable-read-pending-input', 'durable-read-fixture', 'durable-read-input-source', '{"scope":"next_turn"}', 'deferred_next_turn', '{"items":[{"type":"text","text":"durable read pending input"}]}', 'turn-input-submission:v1:blake3:cfa33cf885994ad5ebc91e08f8f36422a792cda3baab46dff1f6f4433e6bc20a', 1700000000000, NULL, NULL, NULL, NULL, 'ingress:durable-read-pending-input', 'due', 0, 1700000000000, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_process_change_clock; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_change_clock VALUES (true, 11, 0);


--
-- Data for Name: lash_process_events; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 1, 'process.observer_added', 'process:p_00000000000070008000000000000001:observer:durable-read-fixture:add:registration', '{"process_id":"p_00000000000070008000000000000001","sequence":1,"event_type":"process.observer_added","payload":{"by":{"kind":"host","operation_id":"registration"},"session":"durable-read-fixture"},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":1,"event_type":"process.observer_added"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"process:p_00000000000070008000000000000001:observer:durable-read-fixture:add:registration"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 3, 'process.waiting', 'process:p_00000000000070008000000000000001:wait:durable-read-wait-key:since:123:entered', '{"process_id":"p_00000000000070008000000000000001","sequence":3,"event_type":"process.waiting","payload":{"wait":{"kind":{"event_type":"process.signal.fixture-ready","key":"durable-read-wait-key","kind":"signal","name":"fixture-ready","ordinal":1},"since_ms":123}},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":3,"event_type":"process.waiting"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"process:p_00000000000070008000000000000001:wait:durable-read-wait-key:since:123:entered"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 4, 'process.effect_outcome', 'durable-read-tool-effect:1', '{"process_id":"p_00000000000070008000000000000001","sequence":4,"event_type":"process.effect_outcome","payload":{"code":"lash:trigger_invalid","node_id":"durable-read-tool-node","occurrence":1,"operation":"tool:fixture","outcome_class":"failure","replay_key":"durable-read-tool-effect:1","vocabulary_version":1},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":4,"event_type":"process.effect_outcome"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"durable-read-tool-effect:1"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 5, 'process.effect_omissions', 'durable-read-effect-omissions', '{"process_id":"p_00000000000070008000000000000001","sequence":5,"event_type":"process.effect_omissions","payload":{"nodes":{"durable-read-tool-node":{"cancelled":0,"failure":1,"success":3}},"occurrence_cap":8,"vocabulary_version":1},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000001","sequence":5,"event_type":"process.effect_omissions"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000001"},"replay":{"key":"durable-read-effect-omissions"}},"semantics":{},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000002', 1, 'fixture.wake', NULL, '{"process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake","payload":{"wake_input":"durable read wake"},"invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000002"}},"semantics":{"wake":{"input":"durable read wake"}},"occurred_at":1700000000000}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('p_00000000000070008000000000000001', 2, 'process.first_started', 'process:p_00000000000070008000000000000001:first-started:attempt:1', '{"payload": {"started": {"owner": {"owner_id": "restate:p_00000000000070008000000000000001", "incarnation_id": "durable-read-fixture"}, "attempt": 1, "started_at_ms": 0}}, "sequence": 2, "semantics": {}, "event_type": "process.first_started", "invocation": {"replay": {"key": "process:p_00000000000070008000000000000001:first-started:attempt:1"}, "subject": {"type": "process_event", "sequence": 2, "event_type": "process.first_started", "process_id": "p_00000000000070008000000000000001"}, "caused_by": {"type": "process", "process_id": "p_00000000000070008000000000000001"}, "attribution": {}}, "process_id": "p_00000000000070008000000000000001", "occurred_at": 1700000000000}');


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

INSERT INTO lash_durable_read_fixture.lash_process_segment_handovers VALUES ('p_00000000000070008000000000000001', 1, '{"segment_ordinal":1,"written_generation":"69f7ce6eab33","route":"LashProcessWorkflow","handover":{"reason":"journal_budget","program_hash":"durable-read-program-v1","engine_state":[8,8,7]}}', NULL, '69f7ce6eab33', 'LashProcessWorkflow');


--
-- Data for Name: lash_process_tombstones; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_tombstones VALUES ('p_00000000000070008000000000000003', 'completed', 1700000000000, 11);


--
-- Data for Name: lash_process_wake_deliveries; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_wake_deliveries VALUES ('wake:v1:blake3:5f8eb7fb2de74745b4f301023356a7a43ea1452d796158036ec9542fde33d409', 'p_00000000000070008000000000000002', 'durable-read-fixture', 1, 'pending', NULL, 0, NULL, 1700000000000, 1700604800000, NULL, '{"version":1,"wake_id":"wake:v1:blake3:5f8eb7fb2de74745b4f301023356a7a43ea1452d796158036ec9542fde33d409","target_session_id":"durable-read-fixture","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake","event_invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000002"}},"authority":{"principal":"host"},"input":"durable read wake","created_at_ms":1700000000000}');


--
-- Data for Name: lash_processes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_processes VALUES ('p_00000000000070008000000000000002', NULL, 'host', 'durable-read-fixture', 'external', NULL, 1700000000000, 1700000000000, 1, 8, 'running', 'detached', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, '{"id":"p_00000000000070008000000000000002","last_event_sequence":1,"input":{"type":"external","metadata":{"fixture":"wake"}},"lifetime":{"lifetime":"detached"},"ancestry":[],"identity":{"kind":"external"},"event_types":[{"name":"process.completed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"completed","await_output":{"pointer":"/await_output"}}}},{"name":"process.failed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"failed","await_output":{"pointer":"/await_output"}}}},{"name":"process.cancelled","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"cancelled","await_output":{"pointer":"/await_output"}}}},{"name":"process.abandoned","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"abandoned","await_output":{"pointer":"/await_output"}}}},{"name":"fixture.wake","payload_schema":{"schema":{}},"semantics":{"wake":{"when":{"present":"/wake_input"},"input":{"pointer":"/wake_input"}}}}],"provenance":{"originator":{"type":"host"}},"created_at_ms":1700000000000,"updated_at_ms":1700000000000,"status":"running"}', NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);
INSERT INTO lash_durable_read_fixture.lash_processes VALUES ('p_00000000000070008000000000000001', 'process-start-key:v1:host:blake3:87196b72f39a2af03893f3bf8000ead79de0d7245ef36918f505981372729001', 'host', NULL, 'durable-read-engine', 'Durable read fixture', 1700000000000, 1700000000000, 5, 6, 'waiting', 'detached', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, '{"id":"p_00000000000070008000000000000001","start_key":"process-start-key:v1:host:blake3:87196b72f39a2af03893f3bf8000ead79de0d7245ef36918f505981372729001","last_event_sequence":5,"input":{"type":"engine","kind":"durable-read-engine","payload":{"fixture":"process"}},"lifetime":{"lifetime":"detached"},"ancestry":[],"identity":{"kind":"durable-read-engine","label":"Durable read fixture","definition":{"engine_kind":"durable-read-engine","definition":{"fixture":"process"},"signature":{"signature":"unknown"}}},"event_types":[{"name":"process.completed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"completed","await_output":{"pointer":"/await_output"}}}},{"name":"process.failed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"failed","await_output":{"pointer":"/await_output"}}}},{"name":"process.cancelled","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"cancelled","await_output":{"pointer":"/await_output"}}}},{"name":"process.abandoned","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"abandoned","await_output":{"pointer":"/await_output"}}}}],"provenance":{"originator":{"type":"host"}},"env_ref":"process-env:v6:blake3:a2be1511dbfc20a2df62205822240227fb1c8a05705837fdc0db9aacd95dc45e","created_at_ms":1700000000000,"updated_at_ms":1700000000000,"first_started":{"owner":{"owner_id":"restate:p_00000000000070008000000000000001","incarnation_id":"durable-read-fixture"},"attempt":1,"started_at_ms":0},"wait":{"kind":{"kind":"signal","name":"fixture-ready","event_type":"process.signal.fixture-ready","key":"durable-read-wait-key","ordinal":1},"since_ms":123},"status":"waiting"}', 'process_start:p_00000000000070008000000000000001', 'due', 0, 0, NULL, NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_queued_work_batches; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_queued_work_batches VALUES (1, 'qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48', 'durable-read-fixture', 'process:p_69d943393ff87a56a6f44a7f58b2d465:event:1:wake', 'earliest_safe_boundary', 'turn', '{}', 'lash.process_wake', 1700000000000, 'queued-work-submission:v1:blake3:b2f6d51a416a949beaf688643612c397aea921543ed3895917c80fe0b24dff2c', NULL, NULL, NULL, NULL, NULL, 'ingress:qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48', 'due', 0, 1700000000000, NULL, NULL, NULL, NULL);
INSERT INTO lash_durable_read_fixture.lash_queued_work_batches VALUES (3, 'qwb:ae86c26bcf5bd2b36f6088776893e6fb555fd9aff540afad16a716385c37f667', 'durable-read-fixture', 'process:p_00000000000070008000000000000002:event:1:wake', 'earliest_safe_boundary', 'turn', '{"principal":"host"}', 'lash.process_wake', 1700000000000, 'queued-work-submission:v1:blake3:cda4909845dc90e73ed2462ed402ccd369685ea061bdeee2f095cbd72b68bf70', NULL, NULL, NULL, 'cancelled', 1700000000000, 'ingress:qwb:ae86c26bcf5bd2b36f6088776893e6fb555fd9aff540afad16a716385c37f667', 'delivered', 0, NULL, NULL, NULL, NULL, 1700000000000);


--
-- Data for Name: lash_queued_work_items; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_queued_work_items VALUES ('qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48', 0, 'qwb:bfc05c27215b02bef8f1709deee8e88cb5e0658c8e65f3ee4a9cd3b2df485a48:item:0', '{"type":"process_wake","wake":{"version":1,"wake_id":"durable-read-queue-wake","target_session_id":"durable-read-fixture","process_id":"p_69d943393ff87a56a6f44a7f58b2d465","sequence":1,"event_type":"process.wake","event_invocation":{"attribution":{"session_id":"durable-read-fixture"},"subject":{"type":"process_event","process_id":"p_69d943393ff87a56a6f44a7f58b2d465","sequence":1,"event_type":"process.wake"}},"input":"durable read queued task","created_at_ms":1700000000000}}');
INSERT INTO lash_durable_read_fixture.lash_queued_work_items VALUES ('qwb:ae86c26bcf5bd2b36f6088776893e6fb555fd9aff540afad16a716385c37f667', 0, 'qwb:ae86c26bcf5bd2b36f6088776893e6fb555fd9aff540afad16a716385c37f667:item:0', '{"type":"process_wake","wake":{"version":1,"wake_id":"wake:v1:blake3:5f8eb7fb2de74745b4f301023356a7a43ea1452d796158036ec9542fde33d409","target_session_id":"durable-read-fixture","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake","event_invocation":{"attribution":{},"subject":{"type":"process_event","process_id":"p_00000000000070008000000000000002","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"p_00000000000070008000000000000002"}},"authority":{"principal":"host"},"input":"durable read wake","created_at_ms":1700000000000}}');


--
-- Data for Name: lash_recovery_leader; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_referrer_fences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_referrer_fences VALUES ('process_record', 'p_00000000000070008000000000000003', 1700000000000);


--
-- Data for Name: lash_release_stamp; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_release_stamp VALUES (true, '0.0.0-dev', 'lash-postgres-store=1', 1700000000000);


--
-- Data for Name: lash_runtime_turn_commits; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"append-session-nodes","scope":{"operation_id":"session:durable-read-fixture:boundary:durable-read-current-append","type":"runtime_operation"}}', 'c376dbe991e70f42c03df8a9e1b5f698839e474e9f92f0c4002445abcb408a70', '{"schema_version":1,"head_revision":1,"checkpoint_ref":"4b6f50c438715ac5352323ecc47b93309f9bca88916c6dfd6fc3a53bda42b891","manifest":{"schema_version":1,"turn_state":{"turn_index":0,"token_usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_write_input_tokens":0,"reasoning_output_tokens":0}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[{"node_id":"frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f","timestamp":"2023-11-14T22:13:20+00:00"},{"node_id":"n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08","timestamp":"2023-11-14T22:13:20+00:00"},{"node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","timestamp":"2023-11-14T22:13:20+00:00"}]}', NULL, 1700000000000, false, '28cc110ed19c5c931910a75632069760f62879e14c4d8cca91b42311542d90d1', 2, 1);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-legacy-commit","type":"runtime_operation"}}', 'f2417c353a5a635c2fbb083912318252229118634b2b508813585ee3abedc998', '{"schema_version":1,"head_revision":2,"checkpoint_ref":"36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918","manifest":{"schema_version":1,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":1},"plugin_state":{"blob_ref":"c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249","encoding_version":1},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":1}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[]}', NULL, 1700000000000, false, NULL, NULL, NULL);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"record-config","scope":{"operation_id":"session:durable-read-fixture:boundary:protocol-materialization","type":"runtime_operation"}}', 'be036f0a2ba1937deb5c684b89b2533be4bbbb8d354093922488236ae38ebb91', '{"schema_version":1,"head_revision":3,"checkpoint_ref":"36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918","manifest":{"schema_version":1,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":1},"plugin_state":{"blob_ref":"c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249","encoding_version":1},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":1}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[]}', NULL, 1700000000000, false, '8eb19a7bce3f611c33b6e0cb7045a1dbd303e44ca29f7b87b2994913eebf87a9', NULL, 1);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-wake-settlement","type":"runtime_operation"}}', 'be036f0a2ba1937deb5c684b89b2533be4bbbb8d354093922488236ae38ebb91', '{"schema_version":1,"head_revision":4,"checkpoint_ref":"36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918","manifest":{"schema_version":1,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":1},"plugin_state":{"blob_ref":"c6155fdf1d371a10a71a007337606ed5e5aa78bbe6f4f673c02d52460e20b249","encoding_version":1},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":1}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[]}', NULL, 1700000000000, false, NULL, NULL, NULL);


--
-- Data for Name: lash_schema_versions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_schema_versions VALUES ('lash-postgres-store', 1, 1);


--
-- Data for Name: lash_session_ingress_sequence; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_ingress_sequence VALUES ('durable-read-fixture', 3);


--
-- Data for Name: lash_session_meta; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_meta VALUES ('durable-read-fixture', 1, 1700000000000, 1700000000000, 'root', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 2, 'durable-read-retained-admission', 'durable-read-retained-admission', NULL, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, NULL, NULL);


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
-- Data for Name: lash_session_run_specs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_sessions VALUES ('durable-read-fixture', 4, '{"schema_version":1,"session_id":"durable-read-fixture","config":{"turn_budget":"unbounded","autonomous":false,"no_progress_budget":{"bounded":12},"charge_safety":{"mode":"require_guarantee"},"generation":{},"tool_access":{"mode":"ambient"},"subagent":null,"plugin_config":{},"config_revision":0},"current_frame_node_id":"frame-node/v3/5d28b33821af555152375193c404a7714a5d3a982d7df9d5e38594163143731f"}', '36b454ed6a06a0954325b7565c23d4948f4f070436d4a6a9ee635f702be37918', 'n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', NULL);


--
-- Data for Name: lash_tool_intent_submissions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_trigger_deliveries; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_deliveries VALUES ('trigger:durable-read-occurrence', 'trigger-subscription:v1:blake3:888a801e9db17827835391b306f82939d62dd5df18a7468298853b6259ed4e11', NULL, 'durable-read-trigger-incarnation', 1, '{"subscription_id":"trigger-subscription:v1:blake3:888a801e9db17827835391b306f82939d62dd5df18a7468298853b6259ed4e11","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v1:blake3:1206c996d22316af384acc797d8fbd18788de313b734e47a46b70112dbca98e6","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v6:blake3:a2be1511dbfc20a2df62205822240227fb1c8a05705837fdc0db9aacd95dc45e","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"source_capture":{"constructor_path":["fixture","event"],"config_schema":{"schema":{"additionalProperties":false,"properties":{"fixture":{"type":"string"}},"type":"object"}},"route":{"kind":"provider","provider_id":"fixture-provider","route":{"account":"fixture"}}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"engine_kind":"durable-read-trigger-target","definition":{"fixture":"trigger"},"signature":{"signature":"unknown"}}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","lifecycle":{"lifecycle":"enabled"},"created_at_ms":1700000000000,"updated_at_ms":1700000000000}', 1700000000000, 'trigger_delivery:a0259003d0f54de58690036dc69e5449', 'due', 0, 1700000000000, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_trigger_mutation_receipts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_mutation_receipts VALUES ('trigger-operation:v1:blake3:b8b196e4289adf541fa702166bb85709729f32aa83b9278546b755172db68aee', 'session', 'durable-read-fixture', 'trigger-command:v6:blake3:5e678c8573148765330e5d0989146dcfa5d733f8cb532f015616eb4f5283b266', '{"Ok":{"type":"mutation","receipt":{"owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","subscription_id":"trigger-subscription:v1:blake3:888a801e9db17827835391b306f82939d62dd5df18a7468298853b6259ed4e11","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v1:blake3:1206c996d22316af384acc797d8fbd18788de313b734e47a46b70112dbca98e6","enabled":true,"disposition":"created","record_snapshot":{"subscription_id":"trigger-subscription:v1:blake3:888a801e9db17827835391b306f82939d62dd5df18a7468298853b6259ed4e11","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v1:blake3:1206c996d22316af384acc797d8fbd18788de313b734e47a46b70112dbca98e6","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v6:blake3:a2be1511dbfc20a2df62205822240227fb1c8a05705837fdc0db9aacd95dc45e","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"source_capture":{"constructor_path":["fixture","event"],"config_schema":{"schema":{"additionalProperties":false,"properties":{"fixture":{"type":"string"}},"type":"object"}},"route":{"kind":"provider","provider_id":"fixture-provider","route":{"account":"fixture"}}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"engine_kind":"durable-read-trigger-target","definition":{"fixture":"trigger"},"signature":{"signature":"unknown"}}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","lifecycle":{"lifecycle":"enabled"},"created_at_ms":1700000000000,"updated_at_ms":1700000000000}}}}', 1700000000000);


--
-- Data for Name: lash_trigger_occurrence_tombstones; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_trigger_occurrences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_occurrences VALUES ('trigger:durable-read-occurrence', 'durable-read-occurrence', 'fixture.event', 'fixture-source', 1700000000000, NULL, '{"occurrence_id":"trigger:durable-read-occurrence","source_type":"fixture.event","source_key":"fixture-source","payload":{"value":42},"idempotency_key":"durable-read-occurrence","occurred_at_ms":1700000000000}');


--
-- Data for Name: lash_trigger_subscriptions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_subscriptions VALUES ('trigger-subscription:v1:blake3:888a801e9db17827835391b306f82939d62dd5df18a7468298853b6259ed4e11', 'session:durable-read-fixture', 'durable-read-trigger', 'durable-read-trigger-incarnation', 1, 'trigger-definition:v1:blake3:1206c996d22316af384acc797d8fbd18788de313b734e47a46b70112dbca98e6', 'fixture.event', 'fixture-source', 'enabled', NULL, 1700000000000, 1700000000000, '{"subscription_id":"trigger-subscription:v1:blake3:888a801e9db17827835391b306f82939d62dd5df18a7468298853b6259ed4e11","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v1:blake3:1206c996d22316af384acc797d8fbd18788de313b734e47a46b70112dbca98e6","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v6:blake3:a2be1511dbfc20a2df62205822240227fb1c8a05705837fdc0db9aacd95dc45e","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"source_capture":{"constructor_path":["fixture","event"],"config_schema":{"schema":{"additionalProperties":false,"properties":{"fixture":{"type":"string"}},"type":"object"}},"route":{"kind":"provider","provider_id":"fixture-provider","route":{"account":"fixture"}}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"engine_kind":"durable-read-trigger-target","definition":{"fixture":"trigger"},"signature":{"signature":"unknown"}}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","lifecycle":{"lifecycle":"enabled"},"created_at_ms":1700000000000,"updated_at_ms":1700000000000}');


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
-- Data for Name: lash_usage_facts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_usage_facts VALUES (1, 'session', 'durable-read-fixture', 'effect:{"version":2,"kind":"turn","session_id":"durable-read-fixture","execution_id":"durable-read-turn"}:"durable-read-llm-call"', 0, 1, 'attempt', 'reported', 'run:9a1c27791ee641779d5905de984ed63b', 'durable-read-call', 'durable-read-turn', 'durable-read-model-key', 'durable-read-model', NULL, 21, 12, 5, 3, 2, NULL, 'ddbd9a8d7c6ae1491e416edca237bbc79df510884cc7c3b265faef15006062dc', 1700000000000);


--
-- Data for Name: lash_usage_owner_retirements; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_usage_owner_retirements VALUES ('session', 'durable-read-retired-usage-owner', 1700000000000);


--
-- Data for Name: lash_usage_runs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_usage_runs VALUES ('session', 'durable-read-fixture', 'effect:{"version":2,"kind":"turn","session_id":"durable-read-fixture","execution_id":"durable-read-turn"}:"durable-read-llm-call"', 'run:9a1c27791ee641779d5905de984ed63b', 'durable-read-turn-scope', 'durable-read-turn', 'durable-read-model-key', 'durable-read-model', 1700000000000, 'settled', NULL, NULL, 1700000000000);


--
-- Data for Name: lash_wake_allocation_floors; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_wake_allocation_floors VALUES ('durable-read-fixture', 'p_00000000000070008000000000000002', 1);


--
-- Data for Name: lash_wake_redelivery_fences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_wake_redelivery_fences VALUES ('durable-read-fixture', 'p_00000000000070008000000000000002', 1);


--
-- Data for Name: lash_worker_recovery; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Name: lash_control_intents_intent_id_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_control_intents_intent_id_seq', 1, false);


--
-- Name: lash_usage_facts_seq_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_usage_facts_seq_seq', 1, true);


--
-- Name: lash_abandoned_consumer_holds lash_abandoned_consumer_holds_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_abandoned_consumer_holds
    ADD CONSTRAINT lash_abandoned_consumer_holds_pkey PRIMARY KEY (hold_key);


--
-- Name: lash_artifact_cleanup_obligations lash_artifact_cleanup_obligations_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_artifact_cleanup_obligations
    ADD CONSTRAINT lash_artifact_cleanup_obligations_pkey PRIMARY KEY (referrer_kind, referrer_id);


--
-- Name: lash_artifact_referrer_edges lash_artifact_referrer_edges_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_artifact_referrer_edges
    ADD CONSTRAINT lash_artifact_referrer_edges_pkey PRIMARY KEY (namespace, artifact_ref, referrer_kind, referrer_id);


--
-- Name: lash_attachment_condemnations lash_attachment_condemnations_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_condemnations
    ADD CONSTRAINT lash_attachment_condemnations_pkey PRIMARY KEY (attachment_id);


--
-- Name: lash_attachment_pending_writes lash_attachment_pending_writes_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_pending_writes
    ADD CONSTRAINT lash_attachment_pending_writes_pkey PRIMARY KEY (write_id);


--
-- Name: lash_attachment_referrer_edges lash_attachment_referrer_edges_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_referrer_edges
    ADD CONSTRAINT lash_attachment_referrer_edges_pkey PRIMARY KEY (attachment_id, referrer_kind, referrer_id);


--
-- Name: lash_attachment_sweep_clock lash_attachment_sweep_clock_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_sweep_clock
    ADD CONSTRAINT lash_attachment_sweep_clock_pkey PRIMARY KEY (singleton);


--
-- Name: lash_attachment_uploads lash_attachment_uploads_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_uploads
    ADD CONSTRAINT lash_attachment_uploads_pkey PRIMARY KEY (attachment_id);


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
-- Name: lash_draining_generations lash_draining_generations_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_draining_generations
    ADD CONSTRAINT lash_draining_generations_pkey PRIMARY KEY (generation);


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
-- Name: lash_process_change_clock lash_process_change_clock_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_change_clock
    ADD CONSTRAINT lash_process_change_clock_pkey PRIMARY KEY (singleton);


--
-- Name: lash_process_events lash_process_events_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_events
    ADD CONSTRAINT lash_process_events_pkey PRIMARY KEY (process_id, sequence);


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
-- Name: lash_recovery_leader lash_recovery_leader_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_recovery_leader
    ADD CONSTRAINT lash_recovery_leader_pkey PRIMARY KEY (name);


--
-- Name: lash_referrer_fences lash_referrer_fences_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_referrer_fences
    ADD CONSTRAINT lash_referrer_fences_pkey PRIMARY KEY (referrer_kind, referrer_id);


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
-- Name: lash_session_ingress_sequence lash_session_ingress_sequence_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_ingress_sequence
    ADD CONSTRAINT lash_session_ingress_sequence_pkey PRIMARY KEY (session_id);


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
-- Name: lash_session_run_specs lash_session_run_specs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_run_specs
    ADD CONSTRAINT lash_session_run_specs_pkey PRIMARY KEY (session_id, spec_hash);


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
-- Name: lash_trigger_occurrence_tombstones lash_trigger_occurrence_tombstones_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_trigger_occurrence_tombstones
    ADD CONSTRAINT lash_trigger_occurrence_tombstones_pkey PRIMARY KEY (occurrence_id);


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
-- Name: lash_usage_facts lash_usage_facts_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_facts
    ADD CONSTRAINT lash_usage_facts_pkey PRIMARY KEY (seq);


--
-- Name: lash_usage_owner_retirements lash_usage_owner_retirements_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_owner_retirements
    ADD CONSTRAINT lash_usage_owner_retirements_pkey PRIMARY KEY (owner_kind, owner_id);


--
-- Name: lash_usage_runs lash_usage_runs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_runs
    ADD CONSTRAINT lash_usage_runs_pkey PRIMARY KEY (owner_kind, owner_id, effect_key, run_id);


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
-- Name: lash_worker_recovery lash_worker_recovery_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_worker_recovery
    ADD CONSTRAINT lash_worker_recovery_pkey PRIMARY KEY (scope_id);


--
-- Name: lash_usage_facts uq_usage_facts_identity; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_facts
    ADD CONSTRAINT uq_usage_facts_identity UNIQUE (owner_kind, owner_id, effect_key, call_ordinal, provider_attempt, fact_kind);


--
-- Name: idx_lash_abandoned_consumer_holds_owner; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_abandoned_consumer_holds_owner ON lash_durable_read_fixture.lash_abandoned_consumer_holds USING btree (owner_scope_kind, owner_scope_id);


--
-- Name: idx_lash_artifact_cleanup_obligations_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_artifact_cleanup_obligations_due ON lash_durable_read_fixture.lash_artifact_cleanup_obligations USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_artifact_cleanup_obligations_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_artifact_cleanup_obligations_id ON lash_durable_read_fixture.lash_artifact_cleanup_obligations USING btree (obligation_id);


--
-- Name: idx_lash_artifact_cleanup_obligations_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_artifact_cleanup_obligations_stalled ON lash_durable_read_fixture.lash_artifact_cleanup_obligations USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_artifact_referrer_edges_referrer; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_artifact_referrer_edges_referrer ON lash_durable_read_fixture.lash_artifact_referrer_edges USING btree (referrer_kind, referrer_id);


--
-- Name: idx_lash_attachment_pending_writes_attachment; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_pending_writes_attachment ON lash_durable_read_fixture.lash_attachment_pending_writes USING btree (attachment_id);


--
-- Name: idx_lash_attachment_pending_writes_referrer; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_pending_writes_referrer ON lash_durable_read_fixture.lash_attachment_pending_writes USING btree (referrer_kind, referrer_id);


--
-- Name: idx_lash_attachment_referrer_edges_referrer; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_referrer_edges_referrer ON lash_durable_read_fixture.lash_attachment_referrer_edges USING btree (referrer_kind, referrer_id);


--
-- Name: idx_lash_checkpoint_blob_refs_blob_ref; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_checkpoint_blob_refs_blob_ref ON lash_durable_read_fixture.lash_checkpoint_blob_refs USING btree (blob_ref, checkpoint_ref);


--
-- Name: idx_lash_control_intents_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_control_intents_obligation_due ON lash_durable_read_fixture.lash_control_intents USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_control_intents_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_control_intents_obligation_id ON lash_durable_read_fixture.lash_control_intents USING btree (obligation_id);


--
-- Name: idx_lash_control_intents_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_control_intents_obligation_stalled ON lash_durable_read_fixture.lash_control_intents USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


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
-- Name: idx_lash_parent_end_plans_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_parent_end_plans_obligation_due ON lash_durable_read_fixture.lash_parent_end_plans USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_parent_end_plans_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_parent_end_plans_obligation_id ON lash_durable_read_fixture.lash_parent_end_plans USING btree (obligation_id);


--
-- Name: idx_lash_parent_end_plans_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_parent_end_plans_obligation_stalled ON lash_durable_read_fixture.lash_parent_end_plans USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_pending_turn_inputs_accepted_state; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_accepted_state ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (session_id, enqueue_seq) WHERE (state = 'accepted'::text);


--
-- Name: idx_lash_pending_turn_inputs_bound_root; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_bound_root ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (session_id, admitted_root) WHERE (admitted_root IS NOT NULL);


--
-- Name: idx_lash_pending_turn_inputs_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_obligation_due ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_pending_turn_inputs_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_pending_turn_inputs_obligation_id ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (obligation_id);


--
-- Name: idx_lash_pending_turn_inputs_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_obligation_stalled ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_pending_turn_inputs_open_state; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_pending_turn_inputs_open_state ON lash_durable_read_fixture.lash_pending_turn_inputs USING btree (session_id, enqueue_seq) WHERE (state = ANY (ARRAY['pending_active'::text, 'deferred_next_turn'::text]));


--
-- Name: idx_lash_process_events_key; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_process_events_key ON lash_durable_read_fixture.lash_process_events USING btree (process_id, idempotency_key) WHERE (idempotency_key IS NOT NULL);


--
-- Name: idx_lash_process_observers_process; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_observers_process ON lash_durable_read_fixture.lash_process_observers USING btree (process_id, session_id);


--
-- Name: idx_lash_process_segment_handovers_route; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_segment_handovers_route ON lash_durable_read_fixture.lash_process_segment_handovers USING btree (route);


--
-- Name: idx_lash_process_tombstones_change; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_process_tombstones_change ON lash_durable_read_fixture.lash_process_tombstones USING btree (pruned_change_seq);


--
-- Name: idx_lash_processes_change_seq; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_change_seq ON lash_durable_read_fixture.lash_processes USING btree (change_seq);


--
-- Name: idx_lash_processes_consumer_hold_owner; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_consumer_hold_owner ON lash_durable_read_fixture.lash_processes USING btree (consumer_hold_scope_kind, consumer_hold_scope_id) WHERE (consumer_hold_key IS NOT NULL);


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
-- Name: idx_lash_processes_live_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_live_generation ON lash_durable_read_fixture.lash_processes USING btree (segment_generation) WHERE ((status = ANY (ARRAY['running'::text, 'waiting'::text])) AND (segment_generation IS NOT NULL));


--
-- Name: idx_lash_processes_non_terminal; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_non_terminal ON lash_durable_read_fixture.lash_processes USING btree (process_id) WHERE (status = ANY (ARRAY['running'::text, 'waiting'::text]));


--
-- Name: idx_lash_processes_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_obligation_due ON lash_durable_read_fixture.lash_processes USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_processes_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_processes_obligation_id ON lash_durable_read_fixture.lash_processes USING btree (obligation_id);


--
-- Name: idx_lash_processes_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_obligation_stalled ON lash_durable_read_fixture.lash_processes USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_processes_originator; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_originator ON lash_durable_read_fixture.lash_processes USING btree (originator_id);


--
-- Name: idx_lash_processes_park_build_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_park_build_generation ON lash_durable_read_fixture.lash_processes USING btree (park_build_generation) WHERE (park_build_generation IS NOT NULL);


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
-- Name: idx_lash_processes_start_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_start_obligation_due ON lash_durable_read_fixture.lash_processes USING btree (start_obligation_due_at_ms, start_obligation_id) WHERE (start_obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_processes_start_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_processes_start_obligation_id ON lash_durable_read_fixture.lash_processes USING btree (start_obligation_id);


--
-- Name: idx_lash_processes_start_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_start_obligation_stalled ON lash_durable_read_fixture.lash_processes USING btree (start_obligation_id) WHERE (start_obligation_state = 'stalled'::text);


--
-- Name: idx_lash_processes_status; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_status ON lash_durable_read_fixture.lash_processes USING btree (status);


--
-- Name: idx_lash_processes_trigger_delivery_pin; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_trigger_delivery_pin ON lash_durable_read_fixture.lash_processes USING btree (process_id) WHERE (trigger_delivery_pin_occurrence_id IS NOT NULL);


--
-- Name: idx_lash_processes_updated; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_updated ON lash_durable_read_fixture.lash_processes USING btree (updated_at_ms);


--
-- Name: idx_lash_processes_wake_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_wake_session ON lash_durable_read_fixture.lash_processes USING btree (wake_session_id);


--
-- Name: idx_lash_queued_work_admission_order; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_admission_order ON lash_durable_read_fixture.lash_queued_work_batches USING btree (session_id, admitted_root, enqueue_seq) WHERE (terminal_cause IS NULL);


--
-- Name: idx_lash_queued_work_batches_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_batches_obligation_due ON lash_durable_read_fixture.lash_queued_work_batches USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_queued_work_batches_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_queued_work_batches_obligation_id ON lash_durable_read_fixture.lash_queued_work_batches USING btree (obligation_id);


--
-- Name: idx_lash_queued_work_batches_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_batches_obligation_stalled ON lash_durable_read_fixture.lash_queued_work_batches USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_queued_work_session_command_order; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_queued_work_session_command_order ON lash_durable_read_fixture.lash_queued_work_batches USING btree (session_id, work_kind, enqueued_at_ms, enqueue_seq) WHERE (terminal_cause IS NULL);


--
-- Name: idx_lash_runtime_turn_commits_failure_evidence; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_runtime_turn_commits_failure_evidence ON lash_durable_read_fixture.lash_runtime_turn_commits USING btree (session_id, committed_at_ms, turn_id) WHERE failure_evidence;


--
-- Name: idx_lash_session_meta_catalog; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_meta_catalog ON lash_durable_read_fixture.lash_session_meta USING btree (created_at_ms, session_id);


--
-- Name: idx_lash_session_meta_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_meta_obligation_due ON lash_durable_read_fixture.lash_session_meta USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_session_meta_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_session_meta_obligation_id ON lash_durable_read_fixture.lash_session_meta USING btree (obligation_id);


--
-- Name: idx_lash_session_meta_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_meta_obligation_stalled ON lash_durable_read_fixture.lash_session_meta USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_session_meta_state_version; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_meta_state_version ON lash_durable_read_fixture.lash_session_meta USING btree (session_state_version, session_id);


--
-- Name: idx_lash_session_roots_admitted_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_roots_admitted_generation ON lash_durable_read_fixture.lash_session_roots USING btree (admitted_generation) WHERE ((admission_json IS NOT NULL) AND (terminal_kind IS NULL));


--
-- Name: idx_lash_session_roots_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_roots_obligation_due ON lash_durable_read_fixture.lash_session_roots USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_session_roots_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_session_roots_obligation_id ON lash_durable_read_fixture.lash_session_roots USING btree (obligation_id);


--
-- Name: idx_lash_session_roots_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_roots_obligation_stalled ON lash_durable_read_fixture.lash_session_roots USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_session_roots_open; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_session_roots_open ON lash_durable_read_fixture.lash_session_roots USING btree (session_id, root) WHERE (terminal_kind IS NULL);


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

CREATE INDEX idx_lash_tool_intent_submissions_scope ON lash_durable_read_fixture.lash_tool_intent_submissions USING btree (owner, execution_scope_id, intent_index);


--
-- Name: idx_lash_trigger_deliveries_obligation_due; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_deliveries_obligation_due ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (obligation_due_at_ms, obligation_id) WHERE (obligation_state = ANY (ARRAY['due'::text, 'claimed'::text]));


--
-- Name: idx_lash_trigger_deliveries_obligation_id; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX idx_lash_trigger_deliveries_obligation_id ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (obligation_id);


--
-- Name: idx_lash_trigger_deliveries_obligation_stalled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_deliveries_obligation_stalled ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (obligation_id) WHERE (obligation_state = 'stalled'::text);


--
-- Name: idx_lash_trigger_deliveries_process; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_deliveries_process ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (process_id);


--
-- Name: idx_lash_trigger_deliveries_subscription; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_deliveries_subscription ON lash_durable_read_fixture.lash_trigger_deliveries USING btree (subscription_id);


--
-- Name: idx_lash_trigger_occurrence_tombstones_reclaimed; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_trigger_occurrence_tombstones_reclaimed ON lash_durable_read_fixture.lash_trigger_occurrence_tombstones USING btree (reclaimed_at_ms);


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
-- Name: idx_lash_turn_parks_build_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_turn_parks_build_generation ON lash_durable_read_fixture.lash_turn_parks USING btree (park_build_generation) WHERE (park_build_generation IS NOT NULL);


--
-- Name: idx_lash_turn_parks_executable_generation; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_turn_parks_executable_generation ON lash_durable_read_fixture.lash_turn_parks USING btree (park_executable_generation) WHERE (park_executable_generation IS NOT NULL);


--
-- Name: idx_lash_turn_parks_since; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_turn_parks_since ON lash_durable_read_fixture.lash_turn_parks USING btree (since_ms, session_id);


--
-- Name: idx_lash_usage_facts_owner_seq; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_usage_facts_owner_seq ON lash_durable_read_fixture.lash_usage_facts USING btree (owner_kind, owner_id, seq);


--
-- Name: idx_lash_usage_facts_unreported; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_usage_facts_unreported ON lash_durable_read_fixture.lash_usage_facts USING btree (owner_kind, owner_id, effect_key, call_ordinal, provider_attempt) WHERE (disposition = 'unreported'::text);


--
-- Name: idx_lash_usage_runs_open_owner; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_usage_runs_open_owner ON lash_durable_read_fixture.lash_usage_runs USING btree (owner_kind, owner_id, admitted_at_ms) WHERE (state = 'open'::text);


--
-- Name: idx_lash_usage_runs_open_scope; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_usage_runs_open_scope ON lash_durable_read_fixture.lash_usage_runs USING btree (owner_kind, owner_id, execution_scope_key) WHERE (state = 'open'::text);


--
-- Name: idx_lash_usage_runs_unresolved; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_usage_runs_unresolved ON lash_durable_read_fixture.lash_usage_runs USING btree (owner_kind, owner_id, effect_key, run_id) WHERE (state = ANY (ARRAY['unknown'::text, 'conflicted'::text]));


--
-- Name: idx_lash_wake_deliveries_group_sequence; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_wake_deliveries_group_sequence ON lash_durable_read_fixture.lash_process_wake_deliveries USING btree (target_session_id, process_id, sequence) WHERE (state <> 'enqueued'::text);


--
-- Name: idx_lash_wake_deliveries_pending; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_wake_deliveries_pending ON lash_durable_read_fixture.lash_process_wake_deliveries USING btree (next_attempt_at_ms, target_session_id, process_id, sequence) WHERE (state = ANY (ARRAY['pending'::text, 'enqueuing'::text]));


--
-- Name: ux_lash_session_roots_unfinished; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX ux_lash_session_roots_unfinished ON lash_durable_read_fixture.lash_session_roots USING btree (session_id) WHERE ((admission_json IS NOT NULL) AND (terminal_kind IS NULL));


--
-- Name: lash_artifact_referrer_edges lash_artifact_referrer_edges_namespace_artifact_ref_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_artifact_referrer_edges
    ADD CONSTRAINT lash_artifact_referrer_edges_namespace_artifact_ref_fkey FOREIGN KEY (namespace, artifact_ref) REFERENCES lash_durable_read_fixture.lash_lashlang_artifacts(namespace, artifact_ref) ON DELETE CASCADE;


--
-- Name: lash_attachment_condemnations lash_attachment_condemnations_write_token_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_attachment_condemnations
    ADD CONSTRAINT lash_attachment_condemnations_write_token_fkey FOREIGN KEY (write_token) REFERENCES lash_durable_read_fixture.lash_attachment_pending_writes(write_id) ON DELETE SET NULL;


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
-- Name: lash_process_events lash_process_events_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_events
    ADD CONSTRAINT lash_process_events_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


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
