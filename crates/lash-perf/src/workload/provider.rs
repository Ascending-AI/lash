use super::{Generator, LlmRequestPlan, ProcessPlan, QueuedInputPlan, TurnPlan};
use anyhow::{Result, ensure};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProviderChunk {
    /// Offset from request acceptance, rather than a per-chunk sleep duration.
    pub due_ms: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderResponse {
    pub operation_id: String,
    pub retryable: bool,
    /// A later provider service wraps this decoded content in its wire protocol.
    pub text: String,
    pub chunks: Vec<ProviderChunk>,
    pub cell_source: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum CallKind {
    Fresh,
    ReplayedEffect,
    Retry,
}

#[derive(Debug, Default, Serialize)]
pub struct CallCounts {
    pub fresh: u64,
    pub replayed_effects: u64,
    pub retries: u64,
}

impl CallCounts {
    pub fn record(&mut self, kind: CallKind) {
        match kind {
            CallKind::Fresh => self.fresh += 1,
            CallKind::ReplayedEffect => self.replayed_effects += 1,
            CallKind::Retry => self.retries += 1,
        }
    }
}

impl Generator<'_> {
    /// Every primary turn finishes through an RLM cell, including turns without tools or processes.
    ///
    /// The response is padded to the sampled output size. A cell whose planned
    /// work does not fit the sampled bucket is served whole and unpadded: the
    /// durable work it drives matters more than the byte bucket.
    pub fn provider_response(&self, plan: &TurnPlan, attempt: u32) -> Result<ProviderResponse> {
        ensure!(attempt > 0, "attempts start at one");
        let id = &plan.operation;
        let source = cell_source(plan, self.tool_word(id.actor, id.ordinal))?;
        let response = format!("<typescript>\n{source}\n</typescript>");
        let text = match (plan.provider_output_bytes as usize).checked_sub(response.len()) {
            Some(padding_bytes) if padding_bytes > 0 => {
                let padding = self.text(
                    id.actor,
                    id.ordinal,
                    "provider-padding",
                    padding_bytes as u32 - 1,
                );
                format!("{padding}\n{response}")
            }
            _ => response,
        };
        Ok(ProviderResponse {
            operation_id: id.key(),
            retryable: plan.retryable_first_attempt && attempt == 1,
            chunks: stream_chunks(
                &text,
                plan.provider_latency_ms,
                if plan.provider_streamed {
                    plan.provider_chunks
                } else {
                    1
                },
            )?,
            text,
            cell_source: Some(source),
        })
    }

    /// A queued input's own turn: a cell that finishes with the input's key,
    /// so the terminal proves which input the turn answered.
    pub fn queued_response(
        &self,
        plan: &TurnPlan,
        queued: &QueuedInputPlan,
    ) -> Result<ProviderResponse> {
        ensure!(
            plan.queued_inputs
                .iter()
                .any(|input| input.idempotency_key == queued.idempotency_key),
            "`{}` is not a queued input of {}",
            queued.idempotency_key,
            plan.operation.key()
        );
        let source = format!(
            "finish({{synthetic:true,operation:{}}});",
            serde_json::to_string(&queued.idempotency_key)?
        );
        let text = format!("<typescript>\n{source}\n</typescript>");
        Ok(ProviderResponse {
            operation_id: queued.idempotency_key.clone(),
            retryable: false,
            chunks: stream_chunks(&text, plan.provider_latency_ms, 1)?,
            text,
            cell_source: Some(source),
        })
    }

    /// The cell that registers every cron schedule of the run as a trigger
    /// subscription whose target body marks each emission it runs for.
    pub fn cron_setup_response(&self) -> Result<ProviderResponse> {
        let mut source = String::from(
            "const on_tick=async(event: load.cron.Tick)=>{return await tools.mark({key:event.schedule+\"/tick/\"+event.tick});};\n",
        );
        let schedules = (0..u64::from(self.workload().spec().cron.subscriptions))
            .map(|subscription| self.cron_schedule(subscription))
            .collect::<Vec<_>>();
        for schedule in &schedules {
            let schedule = serde_json::to_string(schedule)?;
            source.push_str(&format!(
                "await triggers.register({{source:load.cron.tick({{schedule:{schedule}}}),target:on_tick,inputs:(event)=>({{event:event}}),name:{schedule}}});\n"
            ));
        }
        source.push_str(&format!(
            "finish({{synthetic:true,operation:{},schedules:{}}});",
            serde_json::to_string(&self.cron_setup_key())?,
            serde_json::to_string(&schedules)?
        ));
        let text = format!("<typescript>\n{source}\n</typescript>");
        Ok(ProviderResponse {
            operation_id: self.cron_setup_key(),
            retryable: false,
            chunks: stream_chunks(&text, 1, 1)?,
            text,
            cell_source: Some(source),
        })
    }

    /// The key of the turn that registers the run's cron schedules.
    pub fn cron_setup_key(&self) -> String {
        format!("{}/cron", self.run())
    }

    /// Plain completions belong to independently keyed auxiliary LLM requests.
    pub fn llm_response(&self, request: &LlmRequestPlan, attempt: u32) -> Result<ProviderResponse> {
        ensure!(attempt > 0, "attempts start at one");
        let id = &request.operation;
        let text = self.text(
            id.actor,
            id.ordinal,
            &format!("llm/{}/completion", request.index),
            request.output_bytes,
        );
        Ok(ProviderResponse {
            operation_id: request.key(),
            retryable: request.retryable_first_attempt && attempt == 1,
            chunks: stream_chunks(
                &text,
                request.latency_ms,
                if request.streamed { request.chunks } else { 1 },
            )?,
            text,
            cell_source: None,
        })
    }

    /// The module a host start links: a durable `body` process taking the
    /// start's key. A body that waits for its `resume` signal ends with the
    /// signal's payload beside the key; any other body ends with the key.
    pub fn process_body(&self, process: &ProcessPlan) -> String {
        if process.waits_for_signal() {
            "const body = async (key) => { const resumed = await waitSignal(\"resume\"); return { key: key, resumed: resumed }; };\nfinish(null);".into()
        } else {
            "const body = async (key) => { return { key: key, synthetic: true }; };\nfinish(null);"
                .into()
        }
    }
}

fn stream_chunks(text: &str, latency_ms: u32, count: u32) -> Result<Vec<ProviderChunk>> {
    ensure!(
        count > 0 && count as usize <= text.len(),
        "chunk count must fit the response"
    );
    let mut chunks = Vec::new();
    let mut start = 0;
    for index in 0..count {
        let mut end = text.len() * (index + 1) as usize / count as usize;
        while !text.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(ProviderChunk {
            due_ms: (u64::from(latency_ms) * u64::from(index + 1) / u64::from(count)) as u32,
            text: text[start..end].to_owned(),
        });
        start = end;
    }
    Ok(chunks)
}

fn cell_source(plan: &TurnPlan, word: &str) -> Result<String> {
    let mut code = format!(
        "const op={};\n",
        serde_json::to_string(&plan.operation.key())?
    );
    if !plan.attachments.is_empty() {
        code.push_str(&format!(
            "for(let i=0;i<{};i=i+1){{await tools.attach({{operation:op,index:i}});}}\n",
            plan.attachments.len()
        ));
    }
    if !plan.tool_batches.is_empty() {
        // Each call is [padding, result_bytes, callback_ms]; its key is
        // `op/tool/batch/index`, the plan's idempotency key.
        let mut batches = Vec::new();
        for calls in &plan.tool_batches {
            let mut batch = Vec::new();
            for call in calls {
                batch.push([
                    super::payload::tool_argument_padding(call)? as u64,
                    u64::from(call.result_bytes),
                    u64::from(call.callback_ms),
                ]);
            }
            batches.push(batch);
        }
        code.push_str(&format!(
            "const w={};\nconst arg=(k,c)=>({{record:{{kind:\"synthetic\",key:k,result_bytes:c[1],callback_ms:c[2]}},payload:w.repeat(c[0]).slice(0,c[0])}});\nconst batches={};\n",
            serde_json::to_string(word)?,
            serde_json::to_string(&batches)?
        ));
        if plan.parallel_tools {
            code.push_str("for(let b=0;b<batches.length;b=b+1){const calls=[];for(let i=0;i<batches[b].length;i=i+1){calls.push(tools.synthetic(arg(op+\"/tool/\"+b+\"/\"+i,batches[b][i])));}await Promise.all(calls);}\n");
        } else {
            code.push_str("for(let b=0;b<batches.length;b=b+1){for(let i=0;i<batches[b].length;i=i+1){await tools.synthetic(arg(op+\"/tool/\"+b+\"/\"+i,batches[b][i]));}}\n");
        }
    }
    if !plan.child_processes.is_empty() {
        // One named handle per child: a cell keeps process handles in
        // variables, never in lists.
        code.push_str("const child=async(parked,key)=>{if(parked){const resumed=await waitSignal(\"resume\");await tools.mark({key:key});return resumed;}await tools.mark({key:key});return {synthetic:true};};\n");
        for (index, process) in plan.child_processes.iter().enumerate() {
            code.push_str(&format!(
                "const h{index}=await processes.start({{definition:child,args:{{parked:{},key:op+\"/child/{index}\"}}}});\n",
                process.parked
            ));
        }
        if plan.child_processes.iter().any(|process| process.parked) {
            let delay = plan
                .child_processes
                .iter()
                .map(|p| p.wake_delay_ms)
                .max()
                .unwrap_or(0);
            code.push_str(&format!("await sleep({delay});\n"));
            for (index, process) in plan.child_processes.iter().enumerate() {
                if process.parked {
                    code.push_str(&format!("await processes.signal({{handle:h{index},name:\"resume\",payload:{{synthetic:true}}}});\n"));
                }
            }
        }
        for (index, process) in plan.child_processes.iter().enumerate() {
            if process.await_result {
                code.push_str(&format!("await processes.await({{handle:h{index}}});\n"));
            }
        }
    }
    code.push_str("finish({synthetic:true,operation:op});");
    Ok(code)
}
