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
-- Name: lash_attachment_condemnations; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_condemnations (
    attachment_id text NOT NULL,
    phase text NOT NULL,
    CONSTRAINT lash_attachment_condemnations_phase_check CHECK ((phase = ANY (ARRAY['condemned'::text, 'deleting'::text])))
);


--
-- Name: lash_attachment_manifest; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_attachment_manifest (
    attachment_id text NOT NULL,
    session_id text NOT NULL,
    canonical_uri text NOT NULL,
    intent_at_ms bigint NOT NULL,
    committed_at_ms bigint,
    owner_kind text,
    owner_id text,
    CONSTRAINT lash_attachment_manifest_check CHECK (((owner_kind IS NULL) = (owner_id IS NULL))),
    CONSTRAINT lash_attachment_manifest_owner_kind_check CHECK ((owner_kind = ANY (ARRAY['turn'::text, 'process'::text])))
);


--
-- Name: lash_await_event_meta; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_await_event_meta (
    singleton boolean DEFAULT true NOT NULL,
    signing_secret bytea NOT NULL,
    CONSTRAINT lash_await_event_meta_singleton_check CHECK (singleton)
);


--
-- Name: lash_await_event_revoked_sessions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_await_event_revoked_sessions (
    session_id text NOT NULL,
    revoked_at_ms bigint NOT NULL
);


--
-- Name: lash_await_event_waits; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_await_event_waits (
    key_id text NOT NULL,
    scope_json text NOT NULL,
    wait_json text NOT NULL,
    session_id text,
    turn_control boolean NOT NULL,
    terminal_json text,
    created_at_ms bigint NOT NULL,
    resolved_at_ms bigint
);


--
-- Name: lash_blobs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_blobs (
    hash text NOT NULL,
    content bytea NOT NULL
);


--
-- Name: lash_checkpoint_blob_refs; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_checkpoint_blob_refs (
    checkpoint_ref text NOT NULL,
    blob_ref text NOT NULL
);


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
-- Name: lash_fork_lineage; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_fork_lineage (
    session_id text NOT NULL,
    ancestor_session_id text NOT NULL,
    fork_node_id text NOT NULL,
    fork_generation bigint NOT NULL,
    CONSTRAINT lash_fork_lineage_fork_generation_check CHECK ((fork_generation >= 0))
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
    CONSTRAINT lash_graph_nodes_generation_check CHECK ((generation >= 0))
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
-- Name: lash_node_anchors; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_node_anchors (
    node_id text NOT NULL,
    checkpoint_ref text NOT NULL,
    source_session_id text NOT NULL
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
    enqueued_at_ms bigint NOT NULL,
    claim_id text,
    claim_owner_id text,
    claim_owner_incarnation_id text,
    claim_owner_liveness_json text,
    claim_token text,
    claim_fencing_token bigint DEFAULT 0 NOT NULL,
    claim_session_lease_generation bigint DEFAULT 0 NOT NULL
);


--
-- Name: lash_pending_turn_inputs_enqueue_seq_seq; Type: SEQUENCE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE SEQUENCE lash_durable_read_fixture.lash_pending_turn_inputs_enqueue_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: lash_pending_turn_inputs_enqueue_seq_seq; Type: SEQUENCE OWNED BY; Schema: lash_durable_read_fixture; Owner: -
--

ALTER SEQUENCE lash_durable_read_fixture.lash_pending_turn_inputs_enqueue_seq_seq OWNED BY lash_durable_read_fixture.lash_pending_turn_inputs.enqueue_seq;


--
-- Name: lash_process_change_clock; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_change_clock (
    singleton boolean DEFAULT true NOT NULL,
    current_seq bigint NOT NULL,
    CONSTRAINT lash_process_change_clock_singleton_check CHECK (singleton)
);


--
-- Name: lash_process_events; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_events (
    process_id text NOT NULL,
    sequence bigint NOT NULL,
    event_type text NOT NULL,
    idempotency_key text,
    occurred_at_ms bigint NOT NULL,
    event_json text NOT NULL
);


--
-- Name: lash_process_leases; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_leases (
    process_id text NOT NULL,
    lease_owner_id text,
    lease_owner_incarnation_id text,
    lease_owner_liveness_json text,
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
    process_id text NOT NULL
);


--
-- Name: lash_process_parent_end_plans; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_parent_end_plans (
    process_id text NOT NULL,
    actions_json text NOT NULL
);


--
-- Name: lash_process_segment_handovers; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_segment_handovers (
    process_id text NOT NULL,
    segment_ordinal bigint NOT NULL,
    handover_json text NOT NULL
);


--
-- Name: lash_process_tombstones; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_tombstones (
    process_id text NOT NULL,
    terminal_label text NOT NULL,
    pruned_at_ms bigint NOT NULL,
    pruned_change_seq bigint NOT NULL
);


--
-- Name: lash_process_wake_deliveries; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_process_wake_deliveries (
    delivery_id text NOT NULL,
    process_id text NOT NULL,
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
    process_id text NOT NULL,
    registration_fingerprint text NOT NULL,
    originator_id text NOT NULL,
    wake_session_id text,
    identity_kind text NOT NULL,
    identity_label text,
    is_waiting boolean NOT NULL,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    change_seq bigint NOT NULL,
    status text NOT NULL,
    record_json text NOT NULL,
    CONSTRAINT ck_processes_status CHECK ((status = ANY (ARRAY['running'::text, 'waiting'::text, 'completed'::text, 'failed'::text, 'cancelled'::text, 'abandoned'::text, 'caller_departed'::text])))
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
-- Name: lash_queued_work_batches_enqueue_seq_seq; Type: SEQUENCE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE SEQUENCE lash_durable_read_fixture.lash_queued_work_batches_enqueue_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: lash_queued_work_batches_enqueue_seq_seq; Type: SEQUENCE OWNED BY; Schema: lash_durable_read_fixture; Owner: -
--

ALTER SEQUENCE lash_durable_read_fixture.lash_queued_work_batches_enqueue_seq_seq OWNED BY lash_durable_read_fixture.lash_queued_work_batches.enqueue_seq;


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
-- Name: lash_runtime_effect_group; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_runtime_effect_group (
    group_key text NOT NULL,
    scope_id text NOT NULL,
    session_id text,
    wake text NOT NULL,
    loser_disposition text NOT NULL,
    children bigint NOT NULL,
    next_seq bigint DEFAULT 0 NOT NULL,
    created_at_ms bigint NOT NULL
);


--
-- Name: lash_runtime_effect_replay; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_runtime_effect_replay (
    scope_id text NOT NULL,
    session_id text,
    replay_key text NOT NULL,
    envelope_hash text NOT NULL,
    envelope_json text NOT NULL,
    status text NOT NULL,
    outcome_json text,
    error_json text,
    lease_owner_id text,
    lease_token text,
    lease_expires_at_ms bigint DEFAULT 0 NOT NULL,
    due_at_ms bigint,
    group_key text,
    settlement_seq bigint,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    CONSTRAINT ck_runtime_effect_replay_status CHECK ((status = ANY (ARRAY['in_progress'::text, 'completed'::text, 'failed'::text])))
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
    CONSTRAINT lash_runtime_turn_commits_check CHECK ((((request_identity_hash IS NULL) = (requested_node_count IS NULL)) AND ((request_identity_hash IS NULL) = (identity_encoding_version IS NULL))))
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
    observer_inheritance_kind text,
    CONSTRAINT ck_session_meta_caused_by_kind CHECK ((caused_by_kind = ANY (ARRAY['turn'::text, 'effect'::text, 'tool_call'::text, 'process'::text, 'process_event'::text, 'trigger_occurrence'::text, 'session_node'::text]))),
    CONSTRAINT ck_session_meta_observer_inheritance_kind CHECK ((observer_inheritance_kind = ANY (ARRAY['all'::text, 'none'::text, 'only'::text]))),
    CONSTRAINT ck_session_meta_relation_kind CHECK ((relation_kind = ANY (ARRAY['root'::text, 'child'::text, 'fork'::text])))
);


--
-- Name: lash_session_meta_fork_inheritance_processes; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_meta_fork_inheritance_processes (
    session_id text NOT NULL,
    process_index bigint NOT NULL,
    process_id text NOT NULL
);


--
-- Name: lash_session_meta_pending_observer_intents; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_session_meta_pending_observer_intents (
    session_id text NOT NULL,
    process_index bigint NOT NULL,
    process_id text NOT NULL,
    process_incarnation bigint,
    attribution text NOT NULL,
    CONSTRAINT lash_session_meta_pending_observer_intents_attribution_check CHECK ((attribution = ANY (ARRAY['host_requested'::text, 'fork_inherited'::text])))
);


--
-- Name: lash_sessions; Type: TABLE; Schema: lash_durable_read_fixture; Owner: -
--

CREATE TABLE lash_durable_read_fixture.lash_sessions (
    session_id text NOT NULL,
    head_revision bigint DEFAULT 0 NOT NULL,
    head_json text NOT NULL,
    checkpoint_ref text,
    leaf_node_id text
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
    process_id text NOT NULL,
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
    request_fingerprint text NOT NULL,
    result_json text NOT NULL,
    created_at_ms bigint NOT NULL
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
    enabled boolean NOT NULL,
    tombstoned boolean NOT NULL,
    created_at_ms bigint NOT NULL,
    updated_at_ms bigint NOT NULL,
    record_json text NOT NULL,
    CONSTRAINT ck_trigger_subscriptions_live_enabled CHECK ((NOT (enabled AND tombstoned)))
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
    affected_input_ids text[] DEFAULT '{}'::text[] NOT NULL,
    affected_dispositions text[] DEFAULT '{}'::text[] NOT NULL
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
    entry_json text NOT NULL
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
    process_id text NOT NULL,
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
-- Name: lash_pending_turn_inputs enqueue_seq; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_pending_turn_inputs ALTER COLUMN enqueue_seq SET DEFAULT nextval('lash_durable_read_fixture.lash_pending_turn_inputs_enqueue_seq_seq'::regclass);


--
-- Name: lash_queued_work_batches enqueue_seq; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_queued_work_batches ALTER COLUMN enqueue_seq SET DEFAULT nextval('lash_durable_read_fixture.lash_queued_work_batches_enqueue_seq_seq'::regclass);


--
-- Name: lash_usage_deltas seq; Type: DEFAULT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_usage_deltas ALTER COLUMN seq SET DEFAULT nextval('lash_durable_read_fixture.lash_usage_deltas_seq_seq'::regclass);


--
-- Data for Name: lash_attachment_condemnations; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_attachment_manifest; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_attachment_manifest VALUES ('durable-read-attachment', 'durable-read-fixture', 'session:durable-read-fixture:sha256:durable-read-attachment', 100, 1700000000000, NULL, NULL);


--
-- Data for Name: lash_await_event_meta; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_await_event_meta VALUES (true, '\x8888888888888888888888888888888888888888888888888888888888888888');


--
-- Data for Name: lash_await_event_revoked_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_await_event_revoked_sessions VALUES ('durable-read-revoked-session', 1700000000000);


--
-- Data for Name: lash_await_event_waits; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_await_event_waits VALUES ('await-event:v3:blake3:492da35020886dbeed12174eb9aebf74f00a41d6fc0cec515dc823b68147850e', '{"type":"turn","session_id":"durable-read-fixture","turn_id":"durable-read-turn"}', '{"type":"tool_completion","tool_call_id":"durable-read-tool-call"}', 'durable-read-fixture', false, '{"status":"ok","payload":{"fixture":"resolved"}}', 1700000000000, 1700000000000);


--
-- Data for Name: lash_blobs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('1b83f0a98f0171ac40b8509328d6dad748bdb12fafa05412a63ec2cbe04c4cc1', '\x82ae736368656d615f76657273696f6e02aa7475726e5f737461746583aa7475726e5f696e64657800ab746f6b656e5f757361676585ac696e7075745f746f6b656e7300ad6f75747075745f746f6b656e7300b763616368655f726561645f696e7075745f746f6b656e7300b863616368655f77726974655f696e7075745f746f6b656e7300b7726561736f6e696e675f6f75747075745f746f6b656e7300b570726f746f636f6c5f7475726e5f6f7074696f6e7382ae736368656d615f76657273696f6e01a77061796c6f616480');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b', '\x84ae736368656d615f76657273696f6e02aa7475726e5f737461746583aa7475726e5f696e64657807ab746f6b656e5f757361676585ac696e7075745f746f6b656e730dad6f75747075745f746f6b656e7308b763616368655f726561645f696e7075745f746f6b656e7305b863616368655f77726974655f696e7075745f746f6b656e7303b7726561736f6e696e675f6f75747075745f746f6b656e7302b570726f746f636f6c5f7475726e5f6f7074696f6e7382ae736368656d615f76657273696f6e01a77061796c6f616480aa636f6d706f6e656e747383af657865637574696f6e5f737461746582a8626c6f625f726566d94037366133316561393765313333616537656432333365333433353564386562353330386465613861623033316231623339613637393336666238626339396338b0656e636f64696e675f76657273696f6e02af706c7567696e5f736e617073686f7482a8626c6f625f726566d94036303564643637366231386431343565656234313430343963626362626333656333663665626264613735303331376263366434363864636532386435626131b0656e636f64696e675f76657273696f6e02aa746f6f6c5f737461746582a8626c6f625f726566d94039623332393338663139643030636538653363633031313637363961383635393065396138623936333562646138346162353632373637316361363261353164b0656e636f64696e675f76657273696f6e02b8706c7567696e5f736e617073686f745f7265766973696f6e04');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('605dd676b18d145eeb414049cbcbbc3ec3f6ebbda750317bc6d468dce28d5ba1', '\x81a7706c7567696e7381bc64757261626c652d726561642d736e617073686f742d706c7567696e82a46d65746184a9706c7567696e5f6964bc64757261626c652d726561642d736e617073686f742d706c7567696eae706c7567696e5f76657273696f6ea5382e382e37a87265766973696f6ecd0377a5737461746582a766697874757265ac706c7567696e2d7374617465a576616c7565cd0377a96172746966616374739182a46e616d65b964757261626c652d726561642d61727469666163742e62696ea46461746193080807');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8', '\x464947383837');
INSERT INTO lash_durable_read_fixture.lash_blobs VALUES ('9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d', '\x82aa67656e65726174696f6ecd0377a5746f6f6c7380');


--
-- Data for Name: lash_checkpoint_blob_refs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b', '76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8');
INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b', '605dd676b18d145eeb414049cbcbbc3ec3f6ebbda750317bc6d468dce28d5ba1');
INSERT INTO lash_durable_read_fixture.lash_checkpoint_blob_refs VALUES ('063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b', '9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d');


--
-- Data for Name: lash_deleted_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_deleted_sessions VALUES ('durable-read-deleted-session', 1700000000000, NULL, 0, 'root', NULL);


--
-- Data for Name: lash_fork_lineage; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_graph_nodes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a', NULL, 0, 'frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a', '{"schema_version":5,"timestamp":"2023-11-14T22:13:20+00:00","kind":"frame_open","frame_key":"initial-frame","reason":"initial","assignment":{"policy":{"model":{"id":"","variant":"provider_default","limits":{"context_window_tokens":1}},"provider_id":"","session_id":null,"autonomous":false,"turn_budget":"unbounded"},"plugin_options":{}},"protocol_turn_options":{"schema_version":1,"payload":{}}}', false);
INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08', 'frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a', 1, 'frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a', '{"schema_version":5,"timestamp":"2023-11-14T22:13:20+00:00","kind":"event","event":{"Conversation":{"id":"m_append_9304aea3269b249d9b8e240f046976a16b2a95e618eb374edf1eded586a60e3c","role":"User","parts":[{"id":"m_append_9304aea3269b249d9b8e240f046976a16b2a95e618eb374edf1eded586a60e3c.p0","kind":"Text","content":"durable read user message","prune_state":"Intact"}],"origin":{"kind":"plugin","plugin_id":"plugin"}}}}', false);
INSERT INTO lash_durable_read_fixture.lash_graph_nodes VALUES ('durable-read-fixture', 'n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', 'n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08', 2, 'frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a', '{"schema_version":5,"timestamp":"2023-11-14T22:13:20+00:00","kind":"plugin","plugin_type":"durable-read-plugin","body":{"fixture":true,"order":2}}', false);


--
-- Data for Name: lash_lashlang_artifacts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_lashlang_artifacts VALUES ('process_execution_env', 'process-env:v4:blake3:daf76b156fd4e60c481117453fb793e488e495df6d82e16c190df609fb200377', '\x7b22706c7567696e5f6f7074696f6e73223a7b7d2c22706f6c696379223a7b226d6f64656c223a7b226964223a22222c2276617269616e74223a2270726f76696465725f64656661756c74222c226c696d697473223a7b22636f6e746578745f77696e646f775f746f6b656e73223a317d7d2c2270726f76696465725f6964223a22222c2273657373696f6e5f6964223a6e756c6c2c226175746f6e6f6d6f7573223a66616c73652c227475726e5f627564676574223a22756e626f756e646564227d7d');


--
-- Data for Name: lash_node_anchors; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_node_anchors VALUES ('n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a', '063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b', 'durable-read-fixture');


--
-- Data for Name: lash_pending_turn_inputs; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_pending_turn_inputs VALUES (1, 'durable-read-pending-input', 'durable-read-fixture', 'durable-read-input-source', '{"scope":"next_turn"}', 'deferred_next_turn', '{"items":[{"type":"text","text":"durable read pending input"}]}', 1700000000000, NULL, NULL, NULL, NULL, NULL, 0, 0);


--
-- Data for Name: lash_process_change_clock; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_change_clock VALUES (true, 8);


--
-- Data for Name: lash_process_events; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('durable-read-waiting-process', 1, 'process.observer_added', 'process:durable-read-waiting-process:observer:durable-read-fixture:add:registration', 1700000000000, '{"process_id":"durable-read-waiting-process","sequence":1,"event_type":"process.observer_added","payload":{"by":{"kind":"host","operation_id":"registration"},"session":"durable-read-fixture"},"invocation":{"scope":{"session_id":"runtime"},"subject":{"type":"process_event","process_id":"durable-read-waiting-process","sequence":1,"event_type":"process.observer_added"},"caused_by":{"type":"process","process_id":"durable-read-waiting-process"},"replay":{"key":"process:durable-read-waiting-process:observer:durable-read-fixture:add:registration"}},"semantics":{},"occurred_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0}}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('durable-read-waiting-process', 2, 'process.waiting', 'process:durable-read-waiting-process:wait:durable-read-wait-key:since:123:entered', 1700000000000, '{"process_id":"durable-read-waiting-process","sequence":2,"event_type":"process.waiting","payload":{"wait":{"kind":{"event_type":"process.signal.fixture-ready","key":"durable-read-wait-key","kind":"signal","name":"fixture-ready","ordinal":1},"since_ms":123}},"invocation":{"scope":{"session_id":"runtime"},"subject":{"type":"process_event","process_id":"durable-read-waiting-process","sequence":2,"event_type":"process.waiting"},"caused_by":{"type":"process","process_id":"durable-read-waiting-process"},"replay":{"key":"process:durable-read-waiting-process:wait:durable-read-wait-key:since:123:entered"}},"semantics":{},"occurred_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0}}');
INSERT INTO lash_durable_read_fixture.lash_process_events VALUES ('durable-read-wake-process', 1, 'fixture.wake', NULL, 1700000000000, '{"process_id":"durable-read-wake-process","sequence":1,"event_type":"fixture.wake","payload":{"wake_input":"durable read wake"},"invocation":{"scope":{"session_id":"runtime"},"subject":{"type":"process_event","process_id":"durable-read-wake-process","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"durable-read-wake-process"}},"semantics":{"wake":{"input":"durable read wake"}},"occurred_at":{"secs_since_epoch":1700000000,"nanos_since_epoch":0}}');


--
-- Data for Name: lash_process_leases; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_leases VALUES ('durable-read-waiting-process', 'durable-read-owner', 'durable-read-incarnation', NULL, 'c24ed156949132698a1d17c2e4766521cfd18ab9b7e09b95c44210164e9271cd', 1, 1700000000000, 1700000000100);


--
-- Data for Name: lash_process_observers; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_observers VALUES ('durable-read-fixture', 'durable-read-waiting-process');


--
-- Data for Name: lash_process_parent_end_plans; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_process_segment_handovers; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_segment_handovers VALUES ('durable-read-waiting-process', 1, '{"segment_ordinal":1,"handover":{"reason":"journal_budget","program_hash":"durable-read-program-v1","engine_state":[8,8,7]}}');


--
-- Data for Name: lash_process_tombstones; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_tombstones VALUES ('durable-read-retired-process', 'completed', 1700000000000, 8);


--
-- Data for Name: lash_process_wake_deliveries; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_process_wake_deliveries VALUES ('wake:v1:blake3:c7a090afe2d3e8c770fa5cbb13211e777855eac33f2daf6550c9d1b7061069d0', 'durable-read-wake-process', 'durable-read-fixture', 1, 'pending', NULL, 0, NULL, 1700000000000, 1700604800000, NULL, '{"version":1,"wake_id":"wake:v1:blake3:c7a090afe2d3e8c770fa5cbb13211e777855eac33f2daf6550c9d1b7061069d0","target_session_id":"durable-read-fixture","process_id":"durable-read-wake-process","sequence":1,"event_type":"fixture.wake","event_invocation":{"scope":{"session_id":"runtime"},"subject":{"type":"process_event","process_id":"durable-read-wake-process","sequence":1,"event_type":"fixture.wake"},"caused_by":{"type":"process","process_id":"durable-read-wake-process"}},"authority":{"principal":"host"},"input":"durable read wake","created_at_ms":1700000000000}');


--
-- Data for Name: lash_processes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_processes VALUES ('durable-read-waiting-process', 'process-registration-definition:v2:blake3:396df2f86298e5e827fe4e8b6b9454b41be352700a945849b476a1b2fabdfbbe', 'host', NULL, 'durable-read-engine', 'Durable read fixture', true, 1700000000000, 1700000000000, 3, 'waiting', '{"id":"durable-read-waiting-process","registration_fingerprint":"process-registration-definition:v2:blake3:396df2f86298e5e827fe4e8b6b9454b41be352700a945849b476a1b2fabdfbbe","input":{"type":"engine","kind":"durable-read-engine","payload":{"fixture":"process"}},"disposition":"rerunnable","identity":{"kind":"durable-read-engine","label":"Durable read fixture","definition":{"fixture":"process"}},"event_types":[{"name":"process.cancel_requested","payload_schema":{"schema":{}},"semantics":{}},{"name":"process.completed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"completed","await_output":{"pointer":"/await_output"}}}},{"name":"process.failed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"failed","await_output":{"pointer":"/await_output"}}}},{"name":"process.cancelled","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"cancelled","await_output":{"pointer":"/await_output"}}}},{"name":"process.abandoned","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"abandoned","await_output":{"pointer":"/await_output"}}}}],"provenance":{"originator":{"type":"host"}},"env_ref":"process-env:v4:blake3:daf76b156fd4e60c481117453fb793e488e495df6d82e16c190df609fb200377","created_at_ms":1700000000000,"updated_at_ms":1700000000000,"wait":{"kind":{"kind":"signal","name":"fixture-ready","event_type":"process.signal.fixture-ready","key":"durable-read-wait-key","ordinal":1},"since_ms":123},"status":"waiting"}');
INSERT INTO lash_durable_read_fixture.lash_processes VALUES ('durable-read-wake-process', 'process-registration-definition:v2:blake3:3e75f30f64551515e4a6d4df0d9364f54d7d06f3bcda6df09ca8952455df93c3', 'host', 'durable-read-fixture', 'external', NULL, false, 1700000000000, 1700000000000, 5, 'running', '{"id":"durable-read-wake-process","registration_fingerprint":"process-registration-definition:v2:blake3:3e75f30f64551515e4a6d4df0d9364f54d7d06f3bcda6df09ca8952455df93c3","input":{"type":"external","metadata":{"fixture":"wake"}},"disposition":"externally_owned","identity":{"kind":"external"},"event_types":[{"name":"process.cancel_requested","payload_schema":{"schema":{}},"semantics":{}},{"name":"process.completed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"completed","await_output":{"pointer":"/await_output"}}}},{"name":"process.failed","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"failed","await_output":{"pointer":"/await_output"}}}},{"name":"process.cancelled","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"cancelled","await_output":{"pointer":"/await_output"}}}},{"name":"process.abandoned","payload_schema":{"schema":{}},"semantics":{"terminal":{"status":"abandoned","await_output":{"pointer":"/await_output"}}}},{"name":"fixture.wake","payload_schema":{"schema":{}},"semantics":{"wake":{"when":{"present":"/wake_input"},"input":{"pointer":"/wake_input"}}}}],"provenance":{"originator":{"type":"host"}},"created_at_ms":1700000000000,"updated_at_ms":1700000000000,"status":"running"}');


--
-- Data for Name: lash_queued_work_batches; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_queued_work_batches VALUES (1, 'qwb:ef3744a04c94fb45e8b79895843cd983d065b8ed4f70c75105a65e8454265d38', 'durable-read-fixture', 'durable-read-queue-source', 'earliest_safe_boundary', 'turn', '{}', NULL, 0, 1700000000000, NULL, NULL, 0, 0);


--
-- Data for Name: lash_queued_work_items; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_queued_work_items VALUES ('qwb:ef3744a04c94fb45e8b79895843cd983d065b8ed4f70c75105a65e8454265d38', 0, 'qwb:ef3744a04c94fb45e8b79895843cd983d065b8ed4f70c75105a65e8454265d38:item:0', '{"type":"agent_frame_task","frame_id":"frame-node/v3/bb1e66ed2f9432a8c209fe5005e0cd25d2c32d462e27bb4d4c6a6009d31d4ef5","task":"durable read queued task"}');


--
-- Data for Name: lash_runtime_effect_group; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_runtime_effect_replay; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_runtime_effect_replay VALUES ('{"version":2,"kind":"turn","session_id":"durable-read-fixture","execution_id":"durable-read-effect-turn"}', 'durable-read-fixture', 'durable-read-exec-replay', '9394d839f85d7d3079ca2afd7b11b5aaebf0b5b44ef28235ca59195a46d4b8b5', '{"json":"{\"invocation\":{\"scope\":{\"session_id\":\"durable-read-fixture\",\"turn_id\":\"durable-read-effect-turn\",\"turn_index\":7,\"protocol_iteration\":0},\"subject\":{\"type\":\"effect\",\"effect_id\":\"durable-read-exec-effect\",\"kind\":\"exec_code\"},\"replay\":{\"key\":\"durable-read-exec-replay\"}},\"command\":{\"type\":\"exec_code\",\"language\":\"fixture\",\"code\":\"return 887\"}}","hash":"9394d839f85d7d3079ca2afd7b11b5aaebf0b5b44ef28235ca59195a46d4b8b5"}', 'completed', '{"type":"exec_code","result":{"Ok":{"observations":[{"text":"durable read effect","projection":{"truncated":false,"original_chars":19,"projected_chars":19,"original_lines":1,"projected_lines":1,"limit":51200,"limit_mode":"bytes","max_lines":2000}}],"tool_calls":[],"executed_calls":[],"printed_images":[],"error":null,"duration_ms":887,"terminal_finish":{"fixture":887}}}}', NULL, NULL, NULL, 0, NULL, NULL, NULL, 1700000000000, 1700000000000);


--
-- Data for Name: lash_runtime_turn_commits; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"append-session-nodes","scope":{"operation_id":"session:durable-read-fixture:boundary:durable-read-current-append","type":"runtime_operation"}}', '8fa8bae5749cb4901fbf690d20a5592f791dd09f3445394f41d5a73a438bb9c1', '{"head_revision":1,"checkpoint_ref":"1b83f0a98f0171ac40b8509328d6dad748bdb12fafa05412a63ec2cbe04c4cc1","manifest":{"schema_version":2,"turn_state":{"turn_index":0,"token_usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_write_input_tokens":0,"reasoning_output_tokens":0},"protocol_turn_options":{"schema_version":1,"payload":{}}}},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[{"node_id":"frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a","timestamp":"2023-11-14T22:13:20+00:00"},{"node_id":"n_3246dccf4a810defd9cc125efda53f1ac7be7acd0a13c98aa3b3e4d1c7f4bb08","timestamp":"2023-11-14T22:13:20+00:00"},{"node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","timestamp":"2023-11-14T22:13:20+00:00"}]}', 1700000000000, 'ff5ba53866337825fa36fdbf24c5935f8f98eca4b5d0578290d12bf09dad93a9', 2, 1);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-legacy-commit","type":"runtime_operation"}}', 'e87e3bc9170e0fdfe6fb0d738730e523ce3e8d3fbce9c18b08a4449357171da1', '{"head_revision":2,"checkpoint_ref":"063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b","manifest":{"schema_version":2,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2},"protocol_turn_options":{"schema_version":1,"payload":{}}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":2},"plugin_snapshot":{"blob_ref":"605dd676b18d145eeb414049cbcbbc3ec3f6ebbda750317bc6d468dce28d5ba1","encoding_version":2},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":2}},"plugin_snapshot_revision":4},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[],"committed_usage_delta_identities":[{"operation_storage_key":"{\"key\":\"commit\",\"scope\":{\"operation_id\":\"durable-read-legacy-commit\",\"type\":\"runtime_operation\"}}","entry_ordinal":0,"payload_encoding_version":2,"payload_hash":"ca5ddfc5c7345352017d9a18bb96a41b9524c46f7d89dc103abe1809b90e20d5"}]}', 1700000000000, NULL, NULL, NULL);
INSERT INTO lash_durable_read_fixture.lash_runtime_turn_commits VALUES ('durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-wake-settlement","type":"runtime_operation"}}', '1b4a9006df0cacf2cf932e80efda6c9c8f8af1356fc75ce4d7dee45a5a275917', '{"head_revision":3,"checkpoint_ref":"063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b","manifest":{"schema_version":2,"turn_state":{"turn_index":7,"token_usage":{"input_tokens":13,"output_tokens":8,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2},"protocol_turn_options":{"schema_version":1,"payload":{}}},"components":{"execution_state":{"blob_ref":"76a31ea97e133ae7ed233e34355d8eb5308dea8ab031b1b39a67936fb8bc99c8","encoding_version":2},"plugin_snapshot":{"blob_ref":"605dd676b18d145eeb414049cbcbbc3ec3f6ebbda750317bc6d468dce28d5ba1","encoding_version":2},"tool_state":{"blob_ref":"9b32938f19d00ce8e3cc0116769a86590e9a8b9635bda84ab5627671ca62a51d","encoding_version":2}},"plugin_snapshot_revision":4},"committed_leaf_node_id":"n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a","realized_node_timestamps":[]}', 1700000000000, NULL, NULL, NULL);


--
-- Data for Name: lash_schema_versions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_schema_versions VALUES ('lash-postgres-store', 68);


--
-- Data for Name: lash_session_execution_leases; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_execution_leases VALUES ('durable-read-fixture', 'durable-read-session-owner', 'durable-read-session-incarnation', 'durable-read-retained-executor', 'durable-read-retained-session-lease', 2, 1700000000000, 100, 1700000000100);


--
-- Data for Name: lash_session_meta; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_session_meta VALUES ('durable-read-fixture', 0, 1700000000000, 1700000000000, 'root', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);


--
-- Data for Name: lash_session_meta_fork_inheritance_processes; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_session_meta_pending_observer_intents; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_sessions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_sessions VALUES ('durable-read-fixture', 3, '{"schema_version":4,"session_id":"durable-read-fixture","config":{"provider_id":"","model":{"id":"","variant":"provider_default","limits":{"context_window_tokens":1}},"turn_budget":"unbounded","prompt":{},"generation":{}},"current_frame_node_id":"frame-node/v3/1048507f59612bb72e9ffd582198122e898b23fb20afe19ffc9aa78968f2fe8a"}', '063823e2efde8ce58c9ab019925e380999138880738aa240c49a92f771bf499b', 'n_03531bbc4371c54580f1b7874194d0d85964dba1d26654a91b77dc19b6b1c19a');


--
-- Data for Name: lash_tool_intent_submissions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_trigger_deliveries; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_deliveries VALUES ('trigger:durable-read-occurrence', 'trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74', 'process:trigger-delivery:v1:blake3:9932bd78e2df19c10adb727ad5f46efd5efcbbb553aa79394832150943c67875', 'durable-read-trigger-incarnation', 1, '{"subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v2:blake3:c99b3bc25e6d449aa65f8ec821f15dcb26217ff51514a058f392020d887196ed","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v4:blake3:daf76b156fd4e60c481117453fb793e488e495df6d82e16c190df609fb200377","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"fixture":"trigger"}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","enabled":true,"tombstoned":false,"created_at_ms":1700000000000,"updated_at_ms":1700000000000}', 1700000000000);


--
-- Data for Name: lash_trigger_mutation_receipts; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_mutation_receipts VALUES ('trigger-operation:v2:blake3:d53259dcf40a55e423a5392198cae3383e86089d874f1294ec29183c9a874a4f', 'trigger-command:v2:blake3:9efa39a532b6f24df3abe843b0b6a0103b4c9d68b6eb20fc336e9cd8b951c6f5', '{"Ok":{"_owner_scope_namespace":"session:durable-read-fixture","receipt":{"definition_fingerprint":"trigger-definition:v2:blake3:c99b3bc25e6d449aa65f8ec821f15dcb26217ff51514a058f392020d887196ed","disposition":"created","enabled":true,"incarnation":"durable-read-trigger-incarnation","owner_scope":{"session_id":"durable-read-fixture","type":"session"},"record_snapshot":{"created_at_ms":1700000000000,"definition_fingerprint":"trigger-definition:v2:blake3:c99b3bc25e6d449aa65f8ec821f15dcb26217ff51514a058f392020d887196ed","enabled":true,"env_ref":"process-env:v4:blake3:daf76b156fd4e60c481117453fb793e488e495df6d82e16c190df609fb200377","event_types":[],"incarnation":"durable-read-trigger-incarnation","input_template":{"event":{"type":"event"}},"name":"Durable read trigger","owner_scope":{"session_id":"durable-read-fixture","type":"session"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"registrant":{"session_id":"durable-read-fixture","type":"session"},"revision":1,"source":{"fixture":"source"},"source_key":"fixture-source","source_type":"fixture.event","subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","subscription_key":"durable-read-trigger","target":{"kind":"durable-read-trigger-target","payload":{"fixture":"trigger"},"type":"engine"},"target_identity":{"definition":{"fixture":"trigger"},"kind":"durable-read-trigger-target","label":"Durable read trigger target"},"target_label":"Durable read trigger target","tombstoned":false,"updated_at_ms":1700000000000,"wake_target":{"session_id":"durable-read-fixture"}},"revision":1,"subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","subscription_key":"durable-read-trigger"},"type":"mutation"}}', 1700000000000);


--
-- Data for Name: lash_trigger_occurrences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_occurrences VALUES ('trigger:durable-read-occurrence', 'durable-read-occurrence', 'fixture.event', 'fixture-source', 1700000000000, NULL, '{"occurrence_id":"trigger:durable-read-occurrence","source_type":"fixture.event","source_key":"fixture-source","payload":{"value":42},"idempotency_key":"durable-read-occurrence","occurred_at_ms":1700000000000}');


--
-- Data for Name: lash_trigger_subscriptions; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_trigger_subscriptions VALUES ('trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74', 'session:durable-read-fixture', 'durable-read-trigger', 'durable-read-trigger-incarnation', 1, 'trigger-definition:v2:blake3:c99b3bc25e6d449aa65f8ec821f15dcb26217ff51514a058f392020d887196ed', 'fixture.event', 'fixture-source', true, false, 1700000000000, 1700000000000, '{"subscription_id":"trigger-subscription:v2:blake3:65d03d5aa96e165d6e48392576cb9dba7e571ae2df15f9947dae596f902e1d74","owner_scope":{"type":"session","session_id":"durable-read-fixture"},"subscription_key":"durable-read-trigger","incarnation":"durable-read-trigger-incarnation","revision":1,"definition_fingerprint":"trigger-definition:v2:blake3:c99b3bc25e6d449aa65f8ec821f15dcb26217ff51514a058f392020d887196ed","registrant":{"type":"session","session_id":"durable-read-fixture"},"env_ref":"process-env:v4:blake3:daf76b156fd4e60c481117453fb793e488e495df6d82e16c190df609fb200377","wake_target":{"session_id":"durable-read-fixture"},"name":"Durable read trigger","source_type":"fixture.event","source_key":"fixture-source","source":{"fixture":"source"},"payload_schema":{"schema":{"additionalProperties":false,"properties":{"value":{"type":"integer"}},"required":["value"],"type":"object"}},"target":{"type":"engine","kind":"durable-read-trigger-target","payload":{"fixture":"trigger"}},"target_identity":{"kind":"durable-read-trigger-target","label":"Durable read trigger target","definition":{"fixture":"trigger"}},"event_types":[],"input_template":{"event":{"type":"event"}},"target_label":"Durable read trigger target","enabled":true,"tombstoned":false,"created_at_ms":1700000000000,"updated_at_ms":1700000000000}');


--
-- Data for Name: lash_turn_cancel_requests; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--



--
-- Data for Name: lash_usage_deltas; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_usage_deltas VALUES (1, 'durable-read-fixture', '{"key":"commit","scope":{"operation_id":"durable-read-legacy-commit","type":"runtime_operation"}}', 0, 2, 'ca5ddfc5c7345352017d9a18bb96a41b9524c46f7d89dc103abe1809b90e20d5', '{"source":"durable-read-turn","model":"durable-read-model","usage":{"input_tokens":21,"output_tokens":12,"cache_read_input_tokens":5,"cache_write_input_tokens":3,"reasoning_output_tokens":2}}');


--
-- Data for Name: lash_wake_allocation_floors; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_wake_allocation_floors VALUES ('durable-read-fixture', 'durable-read-wake-process', 1);


--
-- Data for Name: lash_wake_redelivery_fences; Type: TABLE DATA; Schema: lash_durable_read_fixture; Owner: -
--

INSERT INTO lash_durable_read_fixture.lash_wake_redelivery_fences VALUES ('durable-read-fixture', 'durable-read-wake-process', 1);


--
-- Name: lash_pending_turn_inputs_enqueue_seq_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_pending_turn_inputs_enqueue_seq_seq', 1, true);


--
-- Name: lash_queued_work_batches_enqueue_seq_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_queued_work_batches_enqueue_seq_seq', 2, true);


--
-- Name: lash_usage_deltas_seq_seq; Type: SEQUENCE SET; Schema: lash_durable_read_fixture; Owner: -
--

SELECT pg_catalog.setval('lash_durable_read_fixture.lash_usage_deltas_seq_seq', 1, true);


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
-- Name: lash_await_event_meta lash_await_event_meta_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_await_event_meta
    ADD CONSTRAINT lash_await_event_meta_pkey PRIMARY KEY (singleton);


--
-- Name: lash_await_event_revoked_sessions lash_await_event_revoked_sessions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_await_event_revoked_sessions
    ADD CONSTRAINT lash_await_event_revoked_sessions_pkey PRIMARY KEY (session_id);


--
-- Name: lash_await_event_waits lash_await_event_waits_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_await_event_waits
    ADD CONSTRAINT lash_await_event_waits_pkey PRIMARY KEY (key_id);


--
-- Name: lash_blobs lash_blobs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_blobs
    ADD CONSTRAINT lash_blobs_pkey PRIMARY KEY (hash);


--
-- Name: lash_checkpoint_blob_refs lash_checkpoint_blob_refs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_checkpoint_blob_refs
    ADD CONSTRAINT lash_checkpoint_blob_refs_pkey PRIMARY KEY (checkpoint_ref, blob_ref);


--
-- Name: lash_deleted_sessions lash_deleted_sessions_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_deleted_sessions
    ADD CONSTRAINT lash_deleted_sessions_pkey PRIMARY KEY (session_id);


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
-- Name: lash_node_anchors lash_node_anchors_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_node_anchors
    ADD CONSTRAINT lash_node_anchors_pkey PRIMARY KEY (node_id);


--
-- Name: lash_pending_turn_inputs lash_pending_turn_inputs_input_id_key; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_pending_turn_inputs
    ADD CONSTRAINT lash_pending_turn_inputs_input_id_key UNIQUE (input_id);


--
-- Name: lash_pending_turn_inputs lash_pending_turn_inputs_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_pending_turn_inputs
    ADD CONSTRAINT lash_pending_turn_inputs_pkey PRIMARY KEY (enqueue_seq);


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
-- Name: lash_process_parent_end_plans lash_process_parent_end_plans_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_parent_end_plans
    ADD CONSTRAINT lash_process_parent_end_plans_pkey PRIMARY KEY (process_id);


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
    ADD CONSTRAINT lash_queued_work_batches_pkey PRIMARY KEY (enqueue_seq);


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
-- Name: lash_runtime_effect_group lash_runtime_effect_group_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_runtime_effect_group
    ADD CONSTRAINT lash_runtime_effect_group_pkey PRIMARY KEY (group_key);


--
-- Name: lash_runtime_effect_replay lash_runtime_effect_replay_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_runtime_effect_replay
    ADD CONSTRAINT lash_runtime_effect_replay_pkey PRIMARY KEY (scope_id, replay_key);


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
-- Name: lash_session_meta_fork_inheritance_processes lash_session_meta_fork_inheritance_processes_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_meta_fork_inheritance_processes
    ADD CONSTRAINT lash_session_meta_fork_inheritance_processes_pkey PRIMARY KEY (session_id, process_index);


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
-- Name: lash_turn_cancel_requests lash_turn_cancel_requests_pkey; Type: CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_turn_cancel_requests
    ADD CONSTRAINT lash_turn_cancel_requests_pkey PRIMARY KEY (session_id, turn_id);


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
-- Name: idx_lash_attachment_manifest_owner; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_manifest_owner ON lash_durable_read_fixture.lash_attachment_manifest USING btree (session_id, owner_kind, owner_id, committed_at_ms);


--
-- Name: idx_lash_attachment_manifest_uncommitted; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_attachment_manifest_uncommitted ON lash_durable_read_fixture.lash_attachment_manifest USING btree (committed_at_ms) WHERE (committed_at_ms IS NULL);


--
-- Name: idx_lash_await_event_waits_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_await_event_waits_session ON lash_durable_read_fixture.lash_await_event_waits USING btree (session_id);


--
-- Name: idx_lash_checkpoint_blob_refs_blob_ref; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_checkpoint_blob_refs_blob_ref ON lash_durable_read_fixture.lash_checkpoint_blob_refs USING btree (blob_ref, checkpoint_ref);


--
-- Name: idx_lash_graph_nodes_parent; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_graph_nodes_parent ON lash_durable_read_fixture.lash_graph_nodes USING btree (parent_node_id);


--
-- Name: idx_lash_node_anchors_checkpoint_ref; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_node_anchors_checkpoint_ref ON lash_durable_read_fixture.lash_node_anchors USING btree (checkpoint_ref);


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
-- Name: idx_lash_processes_live_worklist; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_live_worklist ON lash_durable_read_fixture.lash_processes USING btree (process_id) WHERE (status = ANY (ARRAY['running'::text, 'waiting'::text]));


--
-- Name: idx_lash_processes_originator; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_originator ON lash_durable_read_fixture.lash_processes USING btree (originator_id);


--
-- Name: idx_lash_processes_status; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_status ON lash_durable_read_fixture.lash_processes USING btree (status);


--
-- Name: idx_lash_processes_updated; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_updated ON lash_durable_read_fixture.lash_processes USING btree (updated_at_ms);


--
-- Name: idx_lash_processes_waiting; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_processes_waiting ON lash_durable_read_fixture.lash_processes USING btree (is_waiting);


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
-- Name: idx_lash_runtime_effect_group_scope; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_runtime_effect_group_scope ON lash_durable_read_fixture.lash_runtime_effect_group USING btree (scope_id);


--
-- Name: idx_lash_runtime_effect_group_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_runtime_effect_group_session ON lash_durable_read_fixture.lash_runtime_effect_group USING btree (session_id);


--
-- Name: idx_lash_runtime_effect_replay_group_unsettled; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_runtime_effect_replay_group_unsettled ON lash_durable_read_fixture.lash_runtime_effect_replay USING btree (group_key, replay_key) WHERE ((group_key IS NOT NULL) AND (settlement_seq IS NULL));


--
-- Name: idx_lash_runtime_effect_replay_lease; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_runtime_effect_replay_lease ON lash_durable_read_fixture.lash_runtime_effect_replay USING btree (status, lease_expires_at_ms);


--
-- Name: idx_lash_runtime_effect_replay_session; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_runtime_effect_replay_session ON lash_durable_read_fixture.lash_runtime_effect_replay USING btree (session_id);


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

CREATE INDEX idx_lash_trigger_subscriptions_source ON lash_durable_read_fixture.lash_trigger_subscriptions USING btree (source_type, source_key, enabled);


--
-- Name: idx_lash_wake_deliveries_group_sequence; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_wake_deliveries_group_sequence ON lash_durable_read_fixture.lash_process_wake_deliveries USING btree (target_session_id, process_id, sequence) WHERE (state <> 'enqueued'::text);


--
-- Name: idx_lash_wake_deliveries_pending; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE INDEX idx_lash_wake_deliveries_pending ON lash_durable_read_fixture.lash_process_wake_deliveries USING btree (next_attempt_at_ms, target_session_id, process_id, sequence) WHERE (state = ANY (ARRAY['pending'::text, 'enqueuing'::text]));


--
-- Name: uq_lash_runtime_effect_replay_group_seq; Type: INDEX; Schema: lash_durable_read_fixture; Owner: -
--

CREATE UNIQUE INDEX uq_lash_runtime_effect_replay_group_seq ON lash_durable_read_fixture.lash_runtime_effect_replay USING btree (group_key, settlement_seq) WHERE ((group_key IS NOT NULL) AND (settlement_seq IS NOT NULL));


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
-- Name: lash_process_parent_end_plans lash_process_parent_end_plans_process_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_process_parent_end_plans
    ADD CONSTRAINT lash_process_parent_end_plans_process_id_fkey FOREIGN KEY (process_id) REFERENCES lash_durable_read_fixture.lash_processes(process_id) ON DELETE CASCADE;


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
-- Name: lash_session_meta_fork_inheritance_processes lash_session_meta_fork_inheritance_processes_session_id_fkey; Type: FK CONSTRAINT; Schema: lash_durable_read_fixture; Owner: -
--

ALTER TABLE ONLY lash_durable_read_fixture.lash_session_meta_fork_inheritance_processes
    ADD CONSTRAINT lash_session_meta_fork_inheritance_processes_session_id_fkey FOREIGN KEY (session_id) REFERENCES lash_durable_read_fixture.lash_session_meta(session_id) ON DELETE CASCADE;


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
-- PostgreSQL database dump complete
--
