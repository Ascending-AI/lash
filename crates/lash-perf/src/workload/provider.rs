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
    /// Every primary turn finishes through a code mode cell, including turns without tools or processes.
    ///
    /// The response is padded to the sampled output size. A cell whose planned
    /// work does not fit the sampled bucket is served whole and unpadded: the
    /// durable work it executes matters more than the byte bucket.
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

    /// One cell executes every input admitted by a run. Each plan has its
    /// own lexical scope, and only the combined cell finishes the run.
    pub fn admitted_response(&self, keys: &[String], attempt: u32) -> Result<ProviderResponse> {
        ensure!(!keys.is_empty(), "no admitted inputs");
        if let [key] = keys {
            let (id, suffix) = super::OperationId::parse(key)?;
            ensure!(id.run == self.run(), "input belongs to another run");
            let plan = self.plan(id.actor, id.ordinal)?;
            return if suffix.is_empty() {
                self.provider_response(&plan, attempt)
            } else {
                let input = plan
                    .queued_inputs
                    .iter()
                    .find(|input| &input.idempotency_key == key)
                    .ok_or_else(|| anyhow::anyhow!("unplanned queued input {key}"))?;
                self.queued_response(&plan, input)
            };
        }
        let mut source = String::new();
        let mut retryable = false;
        let mut latency = 0;
        let mut chunks = 1;
        for key in keys {
            let (id, suffix) = super::OperationId::parse(key)?;
            ensure!(id.run == self.run(), "input belongs to another run");
            let plan = self.plan(id.actor, id.ordinal)?;
            latency = latency.max(plan.provider_latency_ms);
            if plan.provider_streamed {
                chunks = chunks.max(plan.provider_chunks);
            }
            if suffix.is_empty() {
                let cell = cell_source(&plan, self.tool_word(id.actor, id.ordinal))?;
                let body = cell
                    .strip_suffix("await control.finish({synthetic:true,operation:op});")
                    .ok_or_else(|| anyhow::anyhow!("turn cell has no terminal"))?;
                source.push_str(&format!(
                    "{{
{body}
}}
"
                ));
                retryable |= plan.retryable_first_attempt && attempt == 1;
            } else {
                ensure!(
                    plan.queued_inputs.iter().any(|q| &q.idempotency_key == key),
                    "unplanned queued input {key}"
                );
            }
        }
        let operation = keys.last().ok_or_else(|| anyhow::anyhow!("no inputs"))?;
        source.push_str(&format!(
            "await control.finish({{synthetic:true,operation:{},operations:{}}});",
            serde_json::to_string(operation)?,
            serde_json::to_string(keys)?
        ));
        let text = format!("<typescript>\n{source}\n</typescript>");
        Ok(ProviderResponse {
            operation_id: operation.clone(),
            retryable,
            chunks: stream_chunks(&text, latency, chunks)?,
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
            "await control.finish({{synthetic:true,operation:{}}});",
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

    /// The module a host start links. Cancellation plans retain active work
    /// with an explicit benchmark delay.
    pub fn process_body(&self, process: &ProcessPlan) -> String {
        if process.cancel {
            "const body = async (key) => { await sleep(60000); return { key: key, synthetic: true }; };\nawait control.finish(null);".into()
        } else {
            "const body = async (key) => { return { key: key, synthetic: true }; };\nawait control.finish(null);"
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
        // Each call is [whole_words, result_bytes, callback_ms, tail_bytes]; its key is
        // `op/tool/batch/index`, the plan's idempotency key.
        let mut batches = Vec::new();
        for calls in &plan.tool_batches {
            let mut batch = Vec::new();
            for call in calls {
                let padding = super::payload::tool_argument_padding(call)?;
                batch.push([
                    (padding / word.len()) as u64,
                    u64::from(call.result_bytes),
                    u64::from(call.callback_ms),
                    (padding % word.len()) as u64,
                ]);
            }
            batches.push(batch);
        }
        code.push_str(&format!(
            "const w={};\nconst arg=(key,c)=>({{record:{{kind:\"synthetic\",key:key,result_bytes:c[1],callback_ms:c[2]}},payload:w.repeat(c[0])+w.slice(0,c[3])}});\nconst batches={};\n",
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
        code.push_str(
            "const child=async(key)=>{await tools.mark({key:key});return {synthetic:true};};\n",
        );
        for index in 0..plan.child_processes.len() {
            code.push_str(&format!("const h{index}=await processes.start({{definition:child,args:{{key:op+\"/child/{index}\"}}}});\n"));
        }
        for (index, process) in plan.child_processes.iter().enumerate() {
            if process.await_result {
                code.push_str(&format!("await processes.await({{handle:h{index}}});\n"));
            }
        }
    }
    code.push_str("await control.finish({synthetic:true,operation:op});");
    Ok(code)
}
