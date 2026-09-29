use super::{Generator, LlmRequestPlan, ProcessPlan, TurnPlan};
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
    pub fn provider_response(&self, plan: &TurnPlan, attempt: u32) -> Result<ProviderResponse> {
        ensure!(attempt > 0, "attempts start at one");
        let id = &plan.operation;
        let source = cell_source(plan, self.tool_word(id.actor, id.ordinal))?;
        let response = format!("<typescript>\n{source}\n</typescript>");
        ensure!(
            response.len() <= plan.provider_output_bytes as usize,
            "provider output bucket {} cannot fit the {}-byte cell",
            plan.provider_output_bytes,
            response.len()
        );
        let padding_bytes = plan.provider_output_bytes - response.len() as u32;
        let text = if padding_bytes > 0 {
            let padding = self.text(id.actor, id.ordinal, "provider-padding", padding_bytes - 1);
            format!("{padding}\n{response}")
        } else {
            response
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

    /// A host-start body has the same durable wait as a cell-authored child.
    pub fn process_body(&self, process: &ProcessPlan) -> String {
        if process.parked {
            "const body = async () => { return await waitSignal(\"resume\"); };".into()
        } else {
            "const body = async () => { return { synthetic: true }; };".into()
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
    let mut code = String::new();
    if !plan.tool_batches.is_empty() {
        let batches: Vec<Vec<u32>> = plan
            .tool_batches
            .iter()
            .map(|batch| batch.iter().map(|call| call.argument_bytes).collect())
            .collect();
        let overhead =
            serde_json::to_vec(&serde_json::json!({"record":{"kind":"synthetic"},"payload":""}))?
                .len();
        code.push_str(&format!("const argument = (size) => ({{record:{{kind:\"synthetic\"}},payload:\"{word}\".repeat(size).slice(0,size-{overhead})}});\n"));
        code.push_str(&format!(
            "for(const batch of {}){{const calls=[];for(const size of batch){{",
            serde_json::to_string(&batches)?
        ));
        if plan.parallel_tools {
            code.push_str(
                "calls.push(tools.synthetic(argument(size)));}await Promise.all(calls);}\n",
            );
        } else {
            code.push_str("await tools.synthetic(argument(size));}}\n");
        }
    }
    if !plan.child_processes.is_empty() {
        let parked: Vec<bool> = plan.child_processes.iter().map(|p| p.parked).collect();
        code.push_str("const child = async (parked) => {if(parked){return await waitSignal(\"resume\");}return {synthetic:true};};\n");
        code.push_str(&format!("const parked={};const handles=[];for(const park of parked){{handles.push(await processes.start({{definition:child,args:{{parked:park}}}}));}}\n", serde_json::to_string(&parked)?));
        if parked.contains(&true) {
            let delay = plan
                .child_processes
                .iter()
                .map(|p| p.wake_delay_ms)
                .max()
                .unwrap_or(0);
            code.push_str(&format!("await sleep({delay});for(let i=0;i<handles.length;i=i+1){{if(parked[i]){{await processes.signal({{handle:handles[i],name:\"resume\",payload:{{synthetic:true}}}});}}}}\n"));
        }
        let awaits: Vec<bool> = plan
            .child_processes
            .iter()
            .map(|p| p.await_result)
            .collect();
        code.push_str(&format!("const awaits={};for(let i=0;i<handles.length;i=i+1){{if(awaits[i]){{await processes.await({{handle:handles[i]}});}}}}\n", serde_json::to_string(&awaits)?));
    }
    code.push_str("finish({synthetic:true});");
    Ok(code)
}
