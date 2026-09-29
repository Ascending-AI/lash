use super::{Workload, distribution::unit};
use anyhow::{Result, ensure};
use rand_chacha::{ChaCha20Rng, rand_core::SeedableRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationId {
    pub run: String,
    pub actor: u64,
    pub ordinal: u64,
}

impl OperationId {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.run, self.actor, self.ordinal)
    }
    pub fn child_key(&self, purpose: &str, index: usize) -> String {
        format!("{}/{purpose}/{index}", self.key())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmRequestPlan {
    pub operation: OperationId,
    pub index: u32,
    pub prompt_bytes: u32,
    pub output_bytes: u32,
    pub latency_ms: u32,
    pub streamed: bool,
    pub chunks: u32,
    pub retryable_first_attempt: bool,
}

impl LlmRequestPlan {
    pub fn key(&self) -> String {
        self.operation.child_key("llm", self.index as usize)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallPlan {
    pub idempotency_key: String,
    pub argument_bytes: u32,
    pub result_bytes: u32,
    pub callback_ms: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessPlan {
    pub idempotency_key: String,
    pub await_result: bool,
    pub parked: bool,
    pub signal: bool,
    pub cancel: bool,
    pub wake_delay_ms: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttachmentPlan {
    pub blob_key: String,
    pub bytes: u32,
    pub owner_actors: Vec<u64>,
    pub explicit_reads: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueuedInputPlan {
    pub idempotency_key: String,
    pub during_active_turn: bool,
    pub cancel: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnPlan {
    pub operation: OperationId,
    pub arrival_gap_s: f64,
    pub prompt_bytes: u32,
    pub input_bytes: u32,
    pub provider_output_bytes: u32,
    pub provider_latency_ms: u32,
    pub provider_streamed: bool,
    pub provider_chunks: u32,
    pub retryable_first_attempt: bool,
    pub tool_batches: Vec<Vec<ToolCallPlan>>,
    pub parallel_tools: bool,
    pub child_processes: Vec<ProcessPlan>,
    pub host_processes: Vec<ProcessPlan>,
    pub attachments: Vec<AttachmentPlan>,
    pub queued_inputs: Vec<QueuedInputPlan>,
    pub auxiliary_llm_requests: u32,
    pub external_occurrences: u32,
    pub trigger_edits: u32,
    pub promotion_reads: u32,
    pub cancel: bool,
    pub delete: bool,
    pub reconnect: bool,
    pub rotate: bool,
    pub compact: bool,
}

/// No clock or service participates in generation. Retry callers reuse the same operation ID.
pub struct Generator<'a> {
    workload: &'a Workload,
    run: String,
}

impl<'a> Generator<'a> {
    pub fn new(workload: &'a Workload, run: &str) -> Result<Self> {
        ensure!(
            !run.is_empty()
                && run.len() <= 128
                && run
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "run ID must contain 1..128 letters, digits, dots, underscores or hyphens"
        );
        Ok(Self {
            workload,
            run: run.to_owned(),
        })
    }

    pub fn workload(&self) -> &Workload {
        self.workload
    }

    pub fn operation(&self, actor: u64, ordinal: u64) -> OperationId {
        OperationId {
            run: self.run.clone(),
            actor,
            ordinal,
        }
    }

    /// Domain-separated ChaCha20 v1 streams keyed by seed, actor, ordinal and purpose.
    pub fn stream(&self, actor: u64, ordinal: u64, purpose: &str) -> ChaCha20Rng {
        let mut hash = Sha256::new();
        hash.update(b"lash-perf/ChaCha20/1\0");
        hash.update(self.workload.spec().seed.to_le_bytes());
        hash.update(actor.to_le_bytes());
        hash.update(ordinal.to_le_bytes());
        hash.update((purpose.len() as u64).to_le_bytes());
        hash.update(purpose.as_bytes());
        ChaCha20Rng::from_seed(hash.finalize().into())
    }

    fn chance(&self, actor: u64, ordinal: u64, purpose: &str, probability: f64) -> bool {
        unit(&mut self.stream(actor, ordinal, purpose)) < probability
    }

    fn count(&self, actor: u64, ordinal: u64, purpose: &str, rate: f64) -> u32 {
        rate.floor() as u32 + u32::from(self.chance(actor, ordinal, purpose, rate.fract()))
    }

    /// Exponential gaps stay independent of acceptance, completion and fault timing.
    pub fn arrival_gap_s(&self, actor: u64, ordinal: u64, rate_per_session_s: f64) -> Result<f64> {
        ensure!(
            rate_per_session_s.is_finite() && rate_per_session_s > 0.0,
            "arrival rate must be positive"
        );
        Ok(-unit(&mut self.stream(actor, ordinal, "arrival")).ln() / rate_per_session_s)
    }

    pub fn prefill_turns(&self, actor: u64, session_ordinal: u64) -> u32 {
        self.workload
            .spec()
            .history_prefill_turns
            .sample(&mut self.stream(actor, session_ordinal, "prefill"))
    }

    pub fn cron_phase_s(&self, subscription: u64) -> f64 {
        unit(&mut self.stream(subscription, 0, "cron-phase"))
            * f64::from(self.workload.spec().cron.cadence_s)
    }

    pub fn plan(&self, actor: u64, ordinal: u64) -> Result<TurnPlan> {
        let spec = self.workload.spec();
        ensure!(
            actor < u64::from(spec.sessions),
            "actor exceeds session population"
        );
        let operation = self.operation(actor, ordinal);
        let mut tool_batches = Vec::new();
        if self.chance(actor, ordinal, "tool-membership", spec.tools.turn_share) {
            for batch in 0..spec.tools.batches {
                let mut rng = self.stream(actor, ordinal, &format!("tool-batch/{batch}"));
                let fanout = spec.tools.fanout.sample(&mut rng);
                let mut calls = Vec::new();
                for call in 0..fanout {
                    calls.push(ToolCallPlan {
                        idempotency_key: operation
                            .child_key(&format!("tool/{batch}"), call as usize),
                        argument_bytes: spec.tool_argument_bytes.sample(&mut rng),
                        result_bytes: spec.tool_result_bytes.sample(&mut rng),
                        callback_ms: spec.tools.callback_ms.sample(&mut rng),
                    });
                }
                tool_batches.push(calls);
            }
        }
        let mut child_processes = Vec::new();
        if self.chance(
            actor,
            ordinal,
            "process-membership",
            spec.processes.turn_share,
        ) {
            let fanout =
                spec.processes
                    .fanout
                    .sample(&mut self.stream(actor, ordinal, "process-fanout"));
            for index in 0..fanout {
                child_processes.push(self.process(&operation, "child", index));
            }
        }
        let host_count = self.count(
            actor,
            ordinal,
            "host-start-count",
            spec.host_process_starts_per_turn,
        );
        let host_processes = (0..host_count)
            .map(|index| self.process(&operation, "host", index))
            .collect();
        let shared = ordinal.is_multiple_of(10) && spec.sessions >= 2;
        let attachment_actor = if shared { actor / 2 * 2 } else { actor };
        let mut attachments = Vec::new();
        if self.chance(
            attachment_actor,
            ordinal,
            "attachment-membership",
            spec.attachments.turn_share,
        ) {
            let mut rng = self.stream(attachment_actor, ordinal, "attachment-shape");
            let count = spec.attachments.count.sample(&mut rng);
            let aggregate = spec.attachments.aggregate_bytes.sample(&mut rng);
            let pair = attachment_actor + 1;
            let owners = if shared && pair < u64::from(spec.sessions) {
                vec![attachment_actor, pair]
            } else {
                vec![actor]
            };
            for index in 0..count {
                attachments.push(AttachmentPlan {
                    blob_key: self
                        .operation(attachment_actor, ordinal)
                        .child_key("blob", index as usize),
                    bytes: aggregate / count + u32::from(index < aggregate % count),
                    owner_actors: owners.clone(),
                    explicit_reads: spec.attachments.explicit_reads_per_blob,
                });
            }
        }
        let queued_count = self.count(actor, ordinal, "queued-count", spec.queued.inputs_per_turn);
        let queued_inputs = (0..queued_count)
            .map(|index| QueuedInputPlan {
                idempotency_key: operation.child_key("queued", index as usize),
                during_active_turn: self.chance(
                    actor,
                    ordinal,
                    &format!("queued-active/{index}"),
                    spec.queued.active_turn_share,
                ),
                cancel: self.chance(
                    actor,
                    ordinal,
                    &format!("queued-cancel/{index}"),
                    spec.queued.cancel_share,
                ),
            })
            .collect();
        Ok(TurnPlan {
            operation,
            arrival_gap_s: self.arrival_gap_s(actor, ordinal, spec.turns_per_session_s)?,
            prompt_bytes: spec
                .prompt_bytes
                .sample(&mut self.stream(actor, ordinal, "prompt-size")),
            input_bytes: spec
                .input_bytes
                .sample(&mut self.stream(actor, ordinal, "input-size")),
            provider_output_bytes: spec.provider_output_bytes.sample(&mut self.stream(
                actor,
                ordinal,
                "output-size",
            )),
            provider_latency_ms: spec.provider.latency_ms.sample(&mut self.stream(
                actor,
                ordinal,
                "provider-latency",
            )),
            provider_streamed: self.chance(
                actor,
                ordinal,
                "provider-stream",
                spec.provider.stream_share,
            ),
            provider_chunks: spec.provider.chunks,
            retryable_first_attempt: self.chance(
                actor,
                ordinal,
                "provider-retry",
                spec.provider.retryable_first_attempt_share,
            ),
            tool_batches,
            parallel_tools: self.chance(actor, ordinal, "tool-parallel", spec.tools.parallel_share),
            child_processes,
            host_processes,
            attachments,
            queued_inputs,
            auxiliary_llm_requests: self.count(
                actor,
                ordinal,
                "llm-count",
                spec.llm_requests_per_turn,
            ),
            external_occurrences: self.count(
                actor,
                ordinal,
                "occurrence-count",
                spec.external_occurrences_per_turn,
            ),
            trigger_edits: self.count(
                actor,
                ordinal,
                "trigger-edit-count",
                spec.trigger_edits_per_turn,
            ),
            promotion_reads: self.count(
                actor,
                ordinal,
                "promotion-read-count",
                spec.promotion_reads_per_turn,
            ),
            cancel: self.chance(actor, ordinal, "turn-cancel", spec.turn_cancel_share),
            delete: self.chance(actor, ordinal, "delete", spec.delete_per_turn),
            reconnect: self.chance(
                actor,
                ordinal,
                "reconnect",
                spec.observation.reconnect_share,
            ),
            rotate: ordinal % u64::from(spec.rotate_after_turns) + 1
                == u64::from(spec.rotate_after_turns),
            compact: ordinal % u64::from(spec.compact_every_turns) + 1
                == u64::from(spec.compact_every_turns),
        })
    }

    /// Auxiliary requests have their own IDs and independently sampled payload/timing streams.
    pub fn llm_request(&self, turn: &TurnPlan, index: u32) -> Result<LlmRequestPlan> {
        ensure!(
            turn.operation.run == self.run,
            "turn belongs to a different run"
        );
        ensure!(
            index < turn.auxiliary_llm_requests,
            "auxiliary request index exceeds turn count"
        );
        let id = &turn.operation;
        let spec = self.workload.spec();
        let purpose = |name: &str| format!("llm/{index}/{name}");
        Ok(LlmRequestPlan {
            operation: id.clone(),
            index,
            prompt_bytes: spec.prompt_bytes.sample(&mut self.stream(
                id.actor,
                id.ordinal,
                &purpose("prompt-size"),
            )),
            output_bytes: spec.provider_output_bytes.sample(&mut self.stream(
                id.actor,
                id.ordinal,
                &purpose("output-size"),
            )),
            latency_ms: spec.provider.latency_ms.sample(&mut self.stream(
                id.actor,
                id.ordinal,
                &purpose("latency"),
            )),
            streamed: self.chance(
                id.actor,
                id.ordinal,
                &purpose("stream"),
                spec.provider.stream_share,
            ),
            chunks: spec.provider.chunks,
            retryable_first_attempt: self.chance(
                id.actor,
                id.ordinal,
                &purpose("retry"),
                spec.provider.retryable_first_attempt_share,
            ),
        })
    }

    pub fn llm_prompt(&self, request: &LlmRequestPlan) -> String {
        self.text(
            request.operation.actor,
            request.operation.ordinal,
            &format!("llm/{}/prompt", request.index),
            request.prompt_bytes,
        )
    }

    fn process(&self, operation: &OperationId, kind: &str, index: u32) -> ProcessPlan {
        let spec = &self.workload.spec().processes;
        let chance = |purpose: &str, probability| {
            self.chance(
                operation.actor,
                operation.ordinal,
                &format!("{kind}-process/{index}/{purpose}"),
                probability,
            )
        };
        ProcessPlan {
            idempotency_key: operation.child_key(kind, index as usize),
            await_result: chance("await", spec.await_share),
            parked: chance("park", spec.park_share),
            signal: kind == "host" && chance("signal", spec.signal_share),
            cancel: kind == "host" && chance("cancel", spec.cancel_share),
            wake_delay_ms: spec.wake_delay_ms,
        }
    }
}
