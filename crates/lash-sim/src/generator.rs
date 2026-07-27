use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::provider_mutations::TRANSPORT_PROVIDER_MUTATIONS;
use crate::runtime_providers::{
    MIGRATED_RUNTIME_PROVIDER_KINDS, runtime_provider_kind_for_session,
    runtime_script_name_for_kind,
};
use crate::scheduler::{BoundaryEvent, BoundaryKind, next_seed};
use crate::trace::StableAliases;

pub const GENERATOR_VERSION: &str = "lash-sim.generated-workload.v9";
pub const WORKLOAD_FAMILY: &str = "deterministic-runtime-state-machine";
const ACTIVE_TURN_QUEUE_OFFSET: u64 = 15;
pub const VALID_WORKLOAD_PROFILES: &[&str] = &[
    "fast",
    "fast-random",
    "default",
    "default-random",
    "full",
    "full-random",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkloadProfileError {
    profile: String,
}

impl std::fmt::Display for WorkloadProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown workload profile `{}`; expected one of: {}",
            self.profile,
            VALID_WORKLOAD_PROFILES.join(", ")
        )
    }
}

impl std::error::Error for WorkloadProfileError {}

pub fn validate_workload_profile(profile: &str) -> Result<(), WorkloadProfileError> {
    WorkloadProfile::parse(profile).map(|_| ())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GeneratedSession {
    pub alias: String,
    pub raw_session_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GeneratedWorkload {
    pub seed: u64,
    pub profile: String,
    pub generator_version: String,
    pub workload_family: String,
    pub workload_id: String,
    pub sessions: Vec<GeneratedSession>,
    pub boundaries: Vec<BoundaryEvent>,
    pub aliases: StableAliases,
}

pub fn generate_workload(
    seed: u64,
    profile: &str,
    max_boundaries: usize,
) -> Result<GeneratedWorkload, WorkloadProfileError> {
    let profile_kind = WorkloadProfile::parse(profile)?;
    let mut aliases = StableAliases::default();
    let mut rng = seed;
    let session_count = profile_kind.session_count(&mut rng);
    let sessions = (0..session_count)
        .map(|index| {
            let raw_session_id = format!("generated-session-{seed}-{index}");
            let alias = aliases.alias("session", raw_session_id.clone());
            let provider_kind = runtime_provider_kind_for_session(index);
            let provider_script =
                runtime_script_name_for_kind(provider_kind).expect("static provider kind");
            SessionPlan::new(alias, raw_session_id, provider_kind, provider_script)
        })
        .collect::<Vec<_>>();

    let mut planner = StateMachinePlanner {
        seed,
        profile: profile.to_string(),
        profile_kind,
        rng,
        sessions,
        operations: Vec::new(),
    };
    planner.plan_required_contracts();
    // Reserve budget for the suspend-session ingress boundaries so the total
    // generated boundary count still lands on `max_boundaries`.
    planner.plan_extra_transitions(max_boundaries.saturating_sub(SUSPEND_KINDS.len()));
    let (sessions, boundaries) = planner.into_workload_boundaries(max_boundaries);

    let workload_id = workload_id(seed, profile, &boundaries);
    Ok(GeneratedWorkload {
        seed,
        profile: profile.to_string(),
        generator_version: GENERATOR_VERSION.to_string(),
        workload_family: WORKLOAD_FAMILY.to_string(),
        workload_id,
        sessions: sessions
            .into_iter()
            .map(|session| GeneratedSession {
                alias: session.alias,
                raw_session_id: session.raw_session_id,
            })
            .collect(),
        boundaries,
        aliases,
    })
}

pub fn default_seed_count(profile: &str) -> Result<usize, WorkloadProfileError> {
    Ok(match WorkloadProfile::parse(profile)? {
        WorkloadProfile::Fast => 2,
        WorkloadProfile::Default => 8,
        WorkloadProfile::Full => 32,
    })
}

pub fn default_max_boundaries(profile: &str) -> Result<usize, WorkloadProfileError> {
    Ok(match WorkloadProfile::parse(profile)? {
        WorkloadProfile::Fast => 72,
        WorkloadProfile::Default => 96,
        WorkloadProfile::Full => 384,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SimShardError {
    shard: String,
    reason: String,
}

impl std::fmt::Display for SimShardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid sim shard `{}`: {}", self.shard, self.reason)
    }
}

impl std::error::Error for SimShardError {}

/// One shard of a count-based generated run: shard `i/n` owns every seed index
/// where `index % n == i - 1`, so the union of all `n` shards is exactly the
/// unsharded seed set and no seed is run twice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SimShard {
    index: usize,
    total: usize,
}

impl SimShard {
    pub const FULL: SimShard = SimShard { index: 1, total: 1 };

    pub fn new(index: usize, total: usize) -> Result<Self, SimShardError> {
        if total == 0 {
            return Err(SimShardError {
                shard: format!("{index}/{total}"),
                reason: "total shard count must be at least 1".to_string(),
            });
        }
        if index == 0 || index > total {
            return Err(SimShardError {
                shard: format!("{index}/{total}"),
                reason: format!("shard index must be between 1 and {total}"),
            });
        }
        Ok(Self { index, total })
    }

    pub fn parse(raw: &str) -> Result<Self, SimShardError> {
        let Some((index, total)) = raw.split_once('/') else {
            return Err(SimShardError {
                shard: raw.to_string(),
                reason: "expected `<index>/<total>`, e.g. `1/8`".to_string(),
            });
        };
        let parse_part = |part: &str, name: &str| {
            part.parse::<usize>().map_err(|err| SimShardError {
                shard: raw.to_string(),
                reason: format!("invalid {name} `{part}`: {err}"),
            })
        };
        Self::new(
            parse_part(index, "shard index")?,
            parse_part(total, "shard total")?,
        )
    }

    pub fn selects(&self, seed_index: usize) -> bool {
        seed_index % self.total == self.index - 1
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.index, self.total)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkloadProfile {
    Fast,
    Default,
    Full,
}

impl WorkloadProfile {
    fn parse(profile: &str) -> Result<Self, WorkloadProfileError> {
        match profile {
            "full" | "full-random" => Ok(Self::Full),
            "default" | "default-random" => Ok(Self::Default),
            "fast" | "fast-random" => Ok(Self::Fast),
            _ => Err(WorkloadProfileError {
                profile: profile.to_string(),
            }),
        }
    }

    fn session_count(self, rng: &mut u64) -> usize {
        match self {
            Self::Fast => MIGRATED_RUNTIME_PROVIDER_KINDS.len(),
            Self::Default => 4 + (next_seed(rng) as usize % 2),
            Self::Full => 4 + (next_seed(rng) as usize % 3),
        }
    }
}

#[derive(Clone, Debug)]
struct SessionPlan {
    alias: String,
    raw_session_id: String,
    provider_kind: &'static str,
    provider_script: &'static str,
    provider_turns: Vec<ProviderTurnPlan>,
    queued_ingress_count: usize,
    trigger_count: usize,
    backend_failure_count: usize,
    provider_mutation_count: usize,
    tool_count: usize,
    exec_code_count: usize,
    process_wake_count: usize,
    process_lifecycle_count: usize,
    durable_effect_count: usize,
    worker_stale_count: usize,
    lease_time_count: usize,
    observer_reconnect_count: usize,
}

impl SessionPlan {
    fn new(
        alias: String,
        raw_session_id: String,
        provider_kind: &'static str,
        provider_script: &'static str,
    ) -> Self {
        Self {
            alias,
            raw_session_id,
            provider_kind,
            provider_script,
            provider_turns: Vec::new(),
            queued_ingress_count: 0,
            trigger_count: 0,
            backend_failure_count: 0,
            provider_mutation_count: 0,
            tool_count: 0,
            exec_code_count: 0,
            process_wake_count: 0,
            process_lifecycle_count: 0,
            durable_effect_count: 0,
            worker_stale_count: 0,
            lease_time_count: 0,
            observer_reconnect_count: 0,
        }
    }

    fn next_provider_turn(&mut self, seed: u64, profile: &str, rng: &mut u64) -> ProviderTurnRef {
        let turn_index = self.provider_turns.len() + 1;
        let discriminator = next_seed(rng) & 0xffff;
        let text = format!(
            "answer {turn_index} for {} seed {seed} profile {profile} path {discriminator:04x}",
            self.alias
        );
        self.provider_turns.push(ProviderTurnPlan {
            turn_index,
            text,
            provider_kind: self.provider_kind,
            script: self.provider_script,
        });
        ProviderTurnRef { turn_index }
    }

    fn next_queue(&mut self) -> usize {
        self.queued_ingress_count += 1;
        self.queued_ingress_count
    }

    fn next_trigger(&mut self) -> usize {
        self.trigger_count += 1;
        self.trigger_count
    }

    fn next_backend_failure(&mut self) -> usize {
        self.backend_failure_count += 1;
        self.backend_failure_count
    }

    fn next_provider_mutation(&mut self) -> usize {
        self.provider_mutation_count += 1;
        self.provider_mutation_count
    }

    fn next_tool(&mut self) -> usize {
        self.tool_count += 1;
        self.tool_count
    }

    fn next_exec_code(&mut self) -> usize {
        self.exec_code_count += 1;
        self.exec_code_count
    }

    fn next_process_wake(&mut self) -> usize {
        self.process_wake_count += 1;
        self.process_wake_count
    }

    fn next_process_lifecycle(&mut self) -> usize {
        self.process_lifecycle_count += 1;
        self.process_lifecycle_count
    }

    fn next_durable_effect(&mut self) -> usize {
        self.durable_effect_count += 1;
        self.durable_effect_count
    }

    fn next_worker_stale(&mut self) -> usize {
        self.worker_stale_count += 1;
        self.worker_stale_count
    }

    fn next_lease_time(&mut self) -> usize {
        self.lease_time_count += 1;
        self.lease_time_count
    }

    fn next_observer_reconnect(&mut self) -> usize {
        self.observer_reconnect_count += 1;
        self.observer_reconnect_count
    }
}

#[derive(Clone, Debug)]
struct ProviderTurnPlan {
    turn_index: usize,
    text: String,
    provider_kind: &'static str,
    script: &'static str,
}

#[derive(Clone, Copy, Debug)]
struct ProviderTurnRef {
    turn_index: usize,
}

#[derive(Clone, Debug)]
enum PlannedOperation {
    ProviderTurn {
        session: usize,
        turn_index: usize,
    },
    Observer {
        session: usize,
        turn_index: usize,
        reconnect: bool,
        observer_index: usize,
    },
    QueuedIngress {
        session: usize,
        queue_index: usize,
        mode: QueuedIngressMode,
        active_turn_index: Option<usize>,
    },
    Cancellation {
        session: usize,
        queue_index: usize,
    },
    Trigger {
        session: usize,
        trigger_index: usize,
    },
    BackendFailure {
        session: usize,
        failure_index: usize,
        operation_index: usize,
        retryable: bool,
    },
    ProviderMutation {
        session: usize,
        mutation_index: usize,
        mutation: &'static str,
    },
    Tool {
        session: usize,
        tool_index: usize,
    },
    ExecCode {
        session: usize,
        exec_index: usize,
        exit_code: i64,
    },
    ProcessWake {
        session: usize,
        wake_index: usize,
        dedupe_index: usize,
    },
    ProcessLifecycle {
        session: usize,
        lifecycle_index: usize,
    },
    DurableEffect {
        session: usize,
        durable_index: usize,
        replay: bool,
    },
    WorkerStaleCompletion {
        session: usize,
        worker_index: usize,
    },
    LeaseTime {
        session: usize,
        lease_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedIngressMode {
    ActiveTurn,
    NextTurn,
}

impl QueuedIngressMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ActiveTurn => "active_turn",
            Self::NextTurn => "next_turn",
        }
    }
}

struct StateMachinePlanner {
    seed: u64,
    profile: String,
    profile_kind: WorkloadProfile,
    rng: u64,
    sessions: Vec<SessionPlan>,
    operations: Vec<PlannedOperation>,
}

impl StateMachinePlanner {
    fn plan_required_contracts(&mut self) {
        let provider_error_repair_session = 0usize;
        let provider_error_operation =
            self.plan_backend_failure(provider_error_repair_session, true);
        self.plan_backend_retry(
            provider_error_repair_session,
            provider_error_operation,
            true,
        );
        self.plan_backend_retry(
            provider_error_repair_session,
            provider_error_operation,
            false,
        );
        let first_provider_turn = self.plan_provider_turn(provider_error_repair_session);
        self.plan_queue_cancel_pair(
            provider_error_repair_session,
            first_provider_turn,
            QueuedIngressMode::ActiveTurn,
        );
        self.plan_observer_snapshot(provider_error_repair_session, first_provider_turn);
        self.plan_tool(provider_error_repair_session);
        self.plan_exec_code(provider_error_repair_session, 0);
        self.plan_provider_turn_with_observer(provider_error_repair_session);
        if self.profile_kind != WorkloadProfile::Fast {
            self.plan_provider_turn_with_observer(provider_error_repair_session);
        }
        self.plan_lease_time(provider_error_repair_session);

        for session in 1..self.sessions.len() {
            self.plan_provider_turn_with_observer(session);
            self.plan_provider_turn_with_observer(session);
            if self.profile_kind != WorkloadProfile::Fast {
                self.plan_provider_turn_with_observer(session);
            }
            self.plan_lease_time(session);
        }
        if self.sessions.len() > 1 {
            let next_turn_session = 1;
            let next_turn_ref = self.first_provider_turn_ref(next_turn_session);
            self.plan_queue_cancel_pair(
                next_turn_session,
                next_turn_ref,
                QueuedIngressMode::NextTurn,
            );
        }

        let primary = (self.next_usize() % self.sessions.len()).min(self.sessions.len() - 1);
        let secondary = (primary + 1) % self.sessions.len();
        self.plan_observer_reconnect(primary);
        self.plan_trigger(primary);
        let backend_retry_operation = self.plan_backend_failure(primary, true);
        self.plan_backend_retry(primary, backend_retry_operation, true);
        self.plan_backend_failure(secondary, false);
        self.plan_provider_mutation(primary, "malformed_sse_chunk");
        self.plan_provider_mutation(secondary, "rate_limit_error_envelope");
        self.plan_provider_mutation(primary, "dropped_terminal_event");
        // The three classes above are required across all four provider parsers
        // by the coverage and protocol-terminal-state oracles, so they stay
        // anchored. Beyond them, seed-select one transport/HTTP perturbation
        // class so every generated run also drives a socket disconnect, a
        // response-start/chunk timeout, or a retryable 5xx through a live
        // provider request.
        let transport_mutation =
            TRANSPORT_PROVIDER_MUTATIONS[self.next_usize() % TRANSPORT_PROVIDER_MUTATIONS.len()];
        self.plan_provider_mutation(secondary, transport_mutation);
        self.plan_tool(primary);
        self.plan_exec_code(primary, 0);
        let process_wake = self.plan_process_wake(primary);
        self.plan_duplicate_process_wake(primary, process_wake);
        self.plan_durable_effect_pair(primary);
        self.plan_worker_stale_completion(secondary);
        // One disposition-driven recovery scenario per workload: spawn / crash /
        // sweep / abandon-request against a real DurableProcessWorker (ADR 0019).
        self.plan_process_lifecycle(secondary);
    }

    fn plan_extra_transitions(&mut self, max_boundaries: usize) {
        let required = self.sessions.len() + self.operations.len();
        let target = max_boundaries.max(required);
        while self.sessions.len() + self.operations.len() < target {
            let remaining = target - self.sessions.len() - self.operations.len();
            let session = self.next_usize() % self.sessions.len();
            match self.next_usize() % 13 {
                0 if remaining >= 2 && self.can_plan_provider_turn(session) => {
                    self.plan_provider_turn_with_observer(session);
                }
                1 if remaining >= 2 && self.can_plan_queue_pair(session) => {
                    let active_turn = self.first_provider_turn_ref(session);
                    let mode = if self.next_usize() & 1 == 0 {
                        QueuedIngressMode::ActiveTurn
                    } else {
                        QueuedIngressMode::NextTurn
                    };
                    self.plan_queue_cancel_pair(session, active_turn, mode);
                }
                2 => self.plan_trigger(session),
                3 => {
                    let retryable = (self.next_usize() & 1) == 0;
                    self.plan_backend_failure(session, retryable);
                }
                4 => {
                    let mutation = PROVIDER_MUTATIONS[self.next_usize() % PROVIDER_MUTATIONS.len()];
                    self.plan_provider_mutation(session, mutation);
                }
                5 => self.plan_tool(session),
                6 => {
                    let exit_code = if self.profile_kind == WorkloadProfile::Fast {
                        0
                    } else {
                        self.next_usize().is_multiple_of(3) as i64
                    };
                    self.plan_exec_code(session, exit_code);
                }
                7 => {
                    self.plan_process_wake(session);
                }
                8 if remaining >= 2 => self.plan_durable_effect_pair(session),
                9 => self.plan_worker_stale_completion(session),
                10 => self.plan_lease_time(session),
                11 => self.plan_observer_reconnect(session),
                _ if remaining >= 2 && self.can_plan_provider_turn(session) => {
                    self.plan_provider_turn_with_observer(session);
                }
                _ => self.plan_lease_time(session),
            }
        }
    }

    fn can_plan_provider_turn(&self, session: usize) -> bool {
        self.sessions[session].queued_ingress_count == 0
    }

    fn can_plan_queue_pair(&self, session: usize) -> bool {
        self.sessions[session].queued_ingress_count == 0
    }

    fn plan_provider_turn(&mut self, session: usize) -> ProviderTurnRef {
        let turn_ref =
            self.sessions[session].next_provider_turn(self.seed, &self.profile, &mut self.rng);
        self.operations.push(PlannedOperation::ProviderTurn {
            session,
            turn_index: turn_ref.turn_index,
        });
        turn_ref
    }

    fn plan_observer_snapshot(&mut self, session: usize, turn_ref: ProviderTurnRef) {
        self.operations.push(PlannedOperation::Observer {
            session,
            turn_index: turn_ref.turn_index,
            reconnect: false,
            observer_index: turn_ref.turn_index,
        });
    }

    fn plan_provider_turn_with_observer(&mut self, session: usize) -> ProviderTurnRef {
        let turn_ref = self.plan_provider_turn(session);
        self.plan_observer_snapshot(session, turn_ref);
        turn_ref
    }

    fn first_provider_turn_ref(&self, session: usize) -> ProviderTurnRef {
        debug_assert!(
            !self.sessions[session].provider_turns.is_empty(),
            "queued ingress requires an existing provider turn"
        );
        ProviderTurnRef { turn_index: 1 }
    }

    fn plan_queue_cancel_pair(
        &mut self,
        session: usize,
        active_turn: ProviderTurnRef,
        mode: QueuedIngressMode,
    ) {
        let queue_index = self.sessions[session].next_queue();
        self.operations.push(PlannedOperation::QueuedIngress {
            session,
            queue_index,
            mode,
            active_turn_index: (mode == QueuedIngressMode::ActiveTurn)
                .then_some(active_turn.turn_index),
        });
        self.operations.push(PlannedOperation::Cancellation {
            session,
            queue_index,
        });
    }

    fn plan_trigger(&mut self, session: usize) {
        let trigger_index = self.sessions[session].next_trigger();
        self.operations.push(PlannedOperation::Trigger {
            session,
            trigger_index,
        });
    }

    fn plan_backend_failure(&mut self, session: usize, retryable: bool) -> usize {
        let failure_index = self.sessions[session].next_backend_failure();
        self.operations.push(PlannedOperation::BackendFailure {
            session,
            failure_index,
            operation_index: failure_index,
            retryable,
        });
        failure_index
    }

    fn plan_backend_retry(&mut self, session: usize, operation_index: usize, retryable: bool) {
        let failure_index = self.sessions[session].next_backend_failure();
        self.operations.push(PlannedOperation::BackendFailure {
            session,
            failure_index,
            operation_index,
            retryable,
        });
    }

    fn plan_provider_mutation(&mut self, session: usize, mutation: &'static str) {
        let mutation_index = self.sessions[session].next_provider_mutation();
        self.operations.push(PlannedOperation::ProviderMutation {
            session,
            mutation_index,
            mutation,
        });
    }

    fn plan_tool(&mut self, session: usize) {
        let tool_index = self.sessions[session].next_tool();
        self.operations.push(PlannedOperation::Tool {
            session,
            tool_index,
        });
    }

    fn plan_exec_code(&mut self, session: usize, exit_code: i64) {
        let exec_index = self.sessions[session].next_exec_code();
        self.operations.push(PlannedOperation::ExecCode {
            session,
            exec_index,
            exit_code,
        });
    }

    fn plan_process_wake(&mut self, session: usize) -> usize {
        let wake_index = self.sessions[session].next_process_wake();
        self.operations.push(PlannedOperation::ProcessWake {
            session,
            wake_index,
            dedupe_index: wake_index,
        });
        wake_index
    }

    fn plan_process_lifecycle(&mut self, session: usize) {
        let lifecycle_index = self.sessions[session].next_process_lifecycle();
        self.operations.push(PlannedOperation::ProcessLifecycle {
            session,
            lifecycle_index,
        });
    }

    fn plan_duplicate_process_wake(&mut self, session: usize, dedupe_index: usize) {
        let wake_index = self.sessions[session].next_process_wake();
        self.operations.push(PlannedOperation::ProcessWake {
            session,
            wake_index,
            dedupe_index,
        });
    }

    fn plan_durable_effect_pair(&mut self, session: usize) {
        let durable_index = self.sessions[session].next_durable_effect();
        self.operations.push(PlannedOperation::DurableEffect {
            session,
            durable_index,
            replay: false,
        });
        self.operations.push(PlannedOperation::DurableEffect {
            session,
            durable_index,
            replay: true,
        });
    }

    fn plan_worker_stale_completion(&mut self, session: usize) {
        let worker_index = self.sessions[session].next_worker_stale();
        self.operations
            .push(PlannedOperation::WorkerStaleCompletion {
                session,
                worker_index,
            });
    }

    fn plan_lease_time(&mut self, session: usize) {
        let lease_index = self.sessions[session].next_lease_time();
        self.operations.push(PlannedOperation::LeaseTime {
            session,
            lease_index,
        });
    }

    fn plan_observer_reconnect(&mut self, session: usize) {
        let observer_index = self.sessions[session].next_observer_reconnect();
        let turn_index = self.sessions[session].provider_turns.len();
        self.operations.push(PlannedOperation::Observer {
            session,
            turn_index,
            reconnect: true,
            observer_index,
        });
    }

    fn into_workload_boundaries(
        mut self,
        max_boundaries: usize,
    ) -> (Vec<SessionPlan>, Vec<BoundaryEvent>) {
        let mut boundaries = Vec::with_capacity(self.sessions.len() + self.operations.len());
        for (index, session) in self.sessions.iter().enumerate() {
            let provider_texts = session
                .provider_turns
                .iter()
                .map(|turn| turn.text.clone())
                .collect::<Vec<_>>();
            let provider_runtime_scripts = session
                .provider_turns
                .iter()
                .map(|turn| {
                    json!({
                        "turn_index": turn.turn_index,
                        "provider_kind": turn.provider_kind,
                        "script": turn.script,
                        "text": turn.text,
                    })
                })
                .collect::<Vec<_>>();
            boundaries.push(BoundaryEvent::new(
                format!("{}:ingress", session.alias),
                session.alias.clone(),
                BoundaryKind::Ingress,
                index as u64,
                "session.open",
                json!({
                    "raw_session_id": session.raw_session_id.clone(),
                    "provider_kind": session.provider_kind,
                    "provider_script": session.provider_script,
                    "provider_texts": provider_texts,
                    "provider_runtime_scripts": provider_runtime_scripts,
                    "state_machine": {
                        "seed": self.seed,
                        "profile": self.profile.clone(),
                        "planned_provider_turns": session.provider_turns.len(),
                    },
                }),
            ));
        }

        // Suspend sessions: real generated turns that park mid-flight on a
        // tool / durable-effect / exec-code await key and can only be resumed by
        // a scheduler-delivered completion boundary. The resume boundary itself
        // is scheduled by the world once it observes the turn parked, mirroring
        // how a finished provider turn schedules its completion.
        let suspend_session_count = self.sessions.len();
        for (offset, suspend_kind) in SUSPEND_KINDS.iter().copied().enumerate() {
            let alias = format!("suspend-{}", suspend_kind.replace('_', "-"));
            boundaries.push(BoundaryEvent::new(
                format!("{alias}:ingress"),
                alias.clone(),
                BoundaryKind::Ingress,
                (suspend_session_count + offset) as u64,
                "session.open.suspend",
                json!({
                    "suspend_kind": suspend_kind,
                    "raw_session_id": alias,
                }),
            ));
        }

        let mut timeline = TimelineCursor::new(10, self.seed ^ self.rng);
        let operations = std::mem::take(&mut self.operations);
        let mut provider_start_at = std::collections::BTreeMap::<(usize, usize), u64>::new();
        let mut queue_at = std::collections::BTreeMap::<(usize, usize), u64>::new();
        for operation in operations {
            let scheduled_at = timeline.next_tick(&mut self.rng);
            match operation {
                PlannedOperation::ProviderTurn {
                    session,
                    turn_index,
                } => {
                    provider_start_at.insert((session, turn_index), scheduled_at);
                    boundaries.push(self.event_for_operation(
                        PlannedOperation::ProviderTurn {
                            session,
                            turn_index,
                        },
                        scheduled_at,
                    ));
                }
                PlannedOperation::QueuedIngress {
                    session,
                    queue_index,
                    mode,
                    active_turn_index,
                } => {
                    let at = active_turn_index
                        .and_then(|active_turn_index| {
                            provider_start_at
                                .get(&(session, active_turn_index))
                                .copied()
                        })
                        .map(|provider_at| provider_at.saturating_add(ACTIVE_TURN_QUEUE_OFFSET))
                        .unwrap_or(scheduled_at);
                    queue_at.insert((session, queue_index), at);
                    boundaries.push(self.event_for_operation(
                        PlannedOperation::QueuedIngress {
                            session,
                            queue_index,
                            mode,
                            active_turn_index,
                        },
                        at,
                    ));
                }
                PlannedOperation::Cancellation {
                    session,
                    queue_index,
                } => {
                    let at = queue_at
                        .get(&(session, queue_index))
                        .copied()
                        .map(|queued_at| queued_at.saturating_add(1))
                        .unwrap_or(scheduled_at);
                    boundaries.push(self.event_for_operation(
                        PlannedOperation::Cancellation {
                            session,
                            queue_index,
                        },
                        at,
                    ));
                }
                operation => boundaries.push(self.event_for_operation(operation, scheduled_at)),
            }
        }

        let required = boundaries.len();
        boundaries.truncate(max_boundaries.max(required));
        (self.sessions, boundaries)
    }

    fn event_for_operation(&self, operation: PlannedOperation, at: u64) -> BoundaryEvent {
        match operation {
            PlannedOperation::ProviderTurn {
                session,
                turn_index,
            } => {
                let session = &self.sessions[session];
                let provider_turn = &session.provider_turns[turn_index - 1];
                BoundaryEvent::new(
                    format!("{}:provider:{turn_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::Provider,
                    at,
                    "provider.chat.stream",
                    json!({
                        "provider_kind": provider_turn.provider_kind,
                        "script": provider_turn.script,
                        "text": provider_turn.text,
                        "turn_index": turn_index,
                        "expected_provider_exchange_count": turn_index,
                        "expected_graph_node_count": turn_index * 2 + 1,
                        "expected_transcript_message_count": turn_index * 2,
                    }),
                )
            }
            PlannedOperation::Observer {
                session,
                turn_index,
                reconnect,
                observer_index,
            } => {
                let session = &self.sessions[session];
                let suffix = if reconnect {
                    format!("reconnect:{observer_index:03}")
                } else {
                    format!("{turn_index:03}")
                };
                BoundaryEvent::new(
                    format!("{}:observer:{suffix}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::Observer,
                    at,
                    if reconnect {
                        "observer.reconnect"
                    } else {
                        "observer.snapshot"
                    },
                    json!({
                        "turn_index": turn_index,
                        "expected_graph_node_count": turn_index * 2 + 1,
                        "expected_transcript_message_count": turn_index * 2,
                        "reconnect": reconnect,
                    }),
                )
            }
            PlannedOperation::QueuedIngress {
                session,
                queue_index,
                mode,
                active_turn_index,
            } => {
                let session = &self.sessions[session];
                let active_turn_id = active_turn_index
                    .map(|active_turn_index| {
                        format!("{}:provider:{active_turn_index:03}", session.alias)
                    })
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                BoundaryEvent::new(
                    queued_boundary_id(session, queue_index),
                    session.alias.clone(),
                    BoundaryKind::QueuedIngress,
                    at,
                    "queued-ingress.next-turn",
                    json!({
                        "text": format!("queued follow-up {queue_index} for {}", session.alias),
                        "source_key": format!("{}:queued-follow-up:{queue_index:03}", session.alias),
                        "ingress_mode": mode.as_str(),
                        "active_turn_id": active_turn_id,
                        "active_turn_provider_turn_index": active_turn_index,
                        "checkpoint_boundary": if mode == QueuedIngressMode::ActiveTurn {
                            Value::String("after_work".to_string())
                        } else {
                            Value::Null
                        },
                    }),
                )
            }
            PlannedOperation::Cancellation {
                session,
                queue_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:cancellation:{queue_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::Cancellation,
                    at,
                    "queued-ingress.cancel",
                    json!({
                        "target": queued_boundary_id(session, queue_index),
                    }),
                )
            }
            PlannedOperation::Trigger {
                session,
                trigger_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:trigger:{trigger_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::Trigger,
                    at,
                    "trigger.delivery",
                    json!({
                        "session": session.alias.clone(),
                        "source_key": format!("trigger/button/{}/{trigger_index:03}", session.alias),
                        "started_process": true,
                    }),
                )
            }
            PlannedOperation::BackendFailure {
                session,
                failure_index,
                operation_index,
                retryable,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:backend-failure:{failure_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::BackendFailure,
                    at,
                    if retryable {
                        "backend.failure.retryable"
                    } else {
                        "backend.failure.terminal"
                    },
                    json!({
                        "session": session.alias.clone(),
                        "operation": format!("commit_runtime_state:{operation_index:03}"),
                        "retryable": retryable,
                    }),
                )
            }
            PlannedOperation::ProviderMutation {
                session,
                mutation_index,
                mutation,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:provider-mutation:{mutation_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::ProviderMutation,
                    at,
                    "provider.script-mutation.rejected",
                    json!({
                        "mutation": mutation,
                        "oracle": "sim.oracle.provider-mutation-rejected.v1",
                    }),
                )
            }
            PlannedOperation::Tool {
                session,
                tool_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:tool:{tool_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::Tool,
                    at,
                    "tool.result.success",
                    json!({
                        "tool": "sim_lookup",
                        "output": format!("tool result {tool_index} for {}", session.alias),
                    }),
                )
            }
            PlannedOperation::ExecCode {
                session,
                exec_index,
                exit_code,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:exec-code:{exec_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::ExecCode,
                    at,
                    if exit_code == 0 {
                        "exec-code.result.success"
                    } else {
                        "exec-code.result.data-error"
                    },
                    json!({
                        "output": format!("exec result {exec_index} for {}", session.alias),
                        "exit_code": exit_code,
                    }),
                )
            }
            PlannedOperation::ProcessWake {
                session,
                wake_index,
                dedupe_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:process-wake:{wake_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::ProcessWake,
                    at,
                    "process.wake.delivery",
                    json!({
                        "session": session.alias.clone(),
                        "dedupe_key": format!("process/wake/{}/{dedupe_index:03}", session.alias),
                    }),
                )
            }
            PlannedOperation::ProcessLifecycle {
                session,
                lifecycle_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:process-lifecycle:{lifecycle_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::ProcessLifecycle,
                    at,
                    "process.lifecycle.recovery",
                    json!({
                        "session": session.alias.clone(),
                    }),
                )
            }
            PlannedOperation::DurableEffect {
                session,
                durable_index,
                replay,
            } => {
                let session = &self.sessions[session];
                let mode = if replay { "replay" } else { "first" };
                BoundaryEvent::new(
                    format!("{}:durable-effect:{durable_index:03}:{mode}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::DurableEffect,
                    at,
                    if replay {
                        "durable.sleep.replay"
                    } else {
                        "durable.sleep.complete"
                    },
                    json!({
                        "session": session.alias.clone(),
                        "durable_key": format!("sleep/{}/{durable_index:03}", session.alias),
                        "result": if replay {
                            json!({ "completed": false, "wake_tick": 0 })
                        } else {
                            json!({ "completed": true, "wake_tick": at })
                        },
                        "runtime_effect": {
                            "kind": "sleep",
                            "effect_id": format!("effect/sleep/{}/{durable_index:03}", session.alias),
                            "duration_ms": 1
                        },
                    }),
                )
            }
            PlannedOperation::WorkerStaleCompletion {
                session,
                worker_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("worker-{worker_index:03}:stale-completion"),
                    format!("worker-{worker_index:03}"),
                    BoundaryKind::Worker,
                    at,
                    "worker.stale-completion-rejected",
                    json!({
                        "session": session.alias.clone(),
                    }),
                )
            }
            PlannedOperation::LeaseTime {
                session,
                lease_index,
            } => {
                let session = &self.sessions[session];
                BoundaryEvent::new(
                    format!("{}:lease-time:{lease_index:03}", session.alias),
                    session.alias.clone(),
                    BoundaryKind::LeaseTime,
                    at,
                    "lease.clock.advance",
                    json!({
                        "tick": at,
                    }),
                )
            }
        }
    }

    fn next_usize(&mut self) -> usize {
        next_seed(&mut self.rng) as usize
    }
}

#[derive(Clone, Copy, Debug)]
struct TimelineCursor {
    next: u64,
}

impl TimelineCursor {
    fn new(start: u64, seed: u64) -> Self {
        Self {
            next: start + (seed & 1),
        }
    }

    fn next_tick(&mut self, rng: &mut u64) -> u64 {
        let tick = self.next;
        self.next += 1 + (next_seed(rng) & 1);
        tick
    }
}

/// Suspend-session kinds emitted per generated workload. Each opens a real turn
/// that parks on the named await kind and is resumed by a scheduler-delivered
/// completion boundary. Tool is enabled today; durable-effect and exec-code
/// follow the same mechanism.
const SUSPEND_KINDS: &[&str] = &["tool", "durable_effect", "exec_code"];

const PROVIDER_MUTATIONS: &[&str] = &[
    "malformed_sse_chunk",
    "rate_limit_error_envelope",
    "dropped_terminal_event",
    "duplicate_tool_call_delta",
    "wrong_provider_schema",
    "mid_stream_disconnect",
    "response_start_timeout",
    "stream_chunk_timeout",
    "retryable_server_error_sequence",
];

fn queued_boundary_id(session: &SessionPlan, queue_index: usize) -> String {
    format!("{}:queued-ingress:{queue_index:03}", session.alias)
}

fn workload_id(seed: u64, profile: &str, boundaries: &[BoundaryEvent]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(seed.to_le_bytes());
    hasher.update(profile.as_bytes());
    hasher.update(GENERATOR_VERSION.as_bytes());
    for boundary in boundaries {
        hasher.update(boundary.boundary_id.as_bytes());
        hasher.update(boundary.actor_alias.as_bytes());
        hasher.update(boundary.at.to_le_bytes());
    }
    let digest = hasher.finalize();
    hex_digest(&digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::BoundaryKind;

    #[test]
    fn workload_generation_is_seed_and_version_deterministic() {
        let first = generate_workload(42, "fast-random", 24).expect("workload");
        let second = generate_workload(42, "fast-random", 24).expect("workload");

        assert_eq!(first, second);
        assert_eq!(first.sessions.len(), MIGRATED_RUNTIME_PROVIDER_KINDS.len());
        assert_eq!(first.generator_version, GENERATOR_VERSION);
    }

    #[test]
    fn fast_profile_default_budget_runs_seeded_extra_transitions() {
        let max = default_max_boundaries("fast-random").expect("fast max");
        assert!(max > 24);
        let first = generate_workload(41, "fast-random", max).expect("first workload");
        let second = generate_workload(42, "fast-random", max).expect("second workload");

        assert_eq!(first.boundaries.len(), max);
        assert_eq!(second.boundaries.len(), max);
        assert_ne!(
            first.workload_id, second.workload_id,
            "fast-random seeds should alter the generated transition schedule"
        );
        assert!(
            first
                .boundaries
                .iter()
                .filter(|boundary| boundary.kind == BoundaryKind::ProviderMutation)
                .count()
                > 2,
            "fast-random default budget should have room beyond the required mutation pair"
        );
    }

    #[test]
    fn generated_queue_ingress_modes_vary_when_budget_allows() {
        let workload = generate_workload(42, "default-random", 160).expect("workload");
        let modes = workload
            .boundaries
            .iter()
            .filter(|boundary| boundary.kind == BoundaryKind::QueuedIngress)
            .filter_map(|boundary| boundary.payload.get("ingress_mode"))
            .filter_map(Value::as_str)
            .collect::<std::collections::BTreeSet<_>>();

        assert!(
            modes.contains("active_turn") && modes.contains("next_turn"),
            "expected both active_turn and next_turn queued ingress modes, got {modes:?}"
        );
    }

    #[test]
    fn sim_shards_partition_the_seed_index_space_exactly() {
        let total = 4;
        let seed_count = 26;
        let mut selected = Vec::new();
        for index in 1..=total {
            let shard = SimShard::new(index, total).expect("shard");
            selected.extend((0..seed_count).filter(|seed_index| shard.selects(*seed_index)));
        }
        selected.sort_unstable();

        assert_eq!(selected, (0..seed_count).collect::<Vec<_>>());
        assert_eq!(SimShard::parse("3/8").expect("parse").label(), "3/8");
        assert_eq!(SimShard::FULL.label(), "1/1");
        assert!((0..seed_count).all(|seed_index| SimShard::FULL.selects(seed_index)));
    }

    #[test]
    fn sim_shard_rejects_malformed_specs() {
        assert_eq!(
            SimShard::parse("0/4").expect_err("index").to_string(),
            "invalid sim shard `0/4`: shard index must be between 1 and 4"
        );
        assert_eq!(
            SimShard::parse("5/4").expect_err("range").to_string(),
            "invalid sim shard `5/4`: shard index must be between 1 and 4"
        );
        assert_eq!(
            SimShard::parse("1/0").expect_err("total").to_string(),
            "invalid sim shard `1/0`: total shard count must be at least 1"
        );
        assert!(SimShard::parse("18").is_err());
        assert!(SimShard::parse("a/b").is_err());
    }

    #[test]
    fn workload_profile_rejects_unknown_values() {
        let err = generate_workload(42, "typo-random", 24).expect_err("unknown profile");

        assert_eq!(
            err.to_string(),
            "unknown workload profile `typo-random`; expected one of: fast, fast-random, default, default-random, full, full-random"
        );
        assert!(default_seed_count("typo-random").is_err());
        assert!(default_max_boundaries("typo-random").is_err());
    }

    #[test]
    fn workload_generation_covers_required_runtime_boundaries() {
        let workload = generate_workload(42, "default-random", 96).expect("workload");
        for kind in [
            BoundaryKind::Ingress,
            BoundaryKind::QueuedIngress,
            BoundaryKind::Provider,
            BoundaryKind::Tool,
            BoundaryKind::ExecCode,
            BoundaryKind::DurableEffect,
            BoundaryKind::ProcessWake,
            BoundaryKind::ProcessLifecycle,
            BoundaryKind::Worker,
            BoundaryKind::Observer,
            BoundaryKind::Cancellation,
            BoundaryKind::Trigger,
            BoundaryKind::BackendFailure,
            BoundaryKind::ProviderMutation,
            BoundaryKind::LeaseTime,
        ] {
            assert!(
                workload
                    .boundaries
                    .iter()
                    .any(|boundary| boundary.kind == kind),
                "missing {kind:?}"
            );
        }
        assert!(workload.boundaries.len() >= 96);
        assert!(
            workload
                .boundaries
                .iter()
                .filter(|boundary| boundary.kind == BoundaryKind::Provider)
                .count()
                > workload.sessions.len() * 2
        );
        let provider_kinds = workload
            .boundaries
            .iter()
            .filter(|boundary| boundary.kind == BoundaryKind::Provider)
            .filter_map(|boundary| boundary.payload.get("provider_kind"))
            .filter_map(serde_json::Value::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            provider_kinds,
            MIGRATED_RUNTIME_PROVIDER_KINDS
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
        );
    }
}
