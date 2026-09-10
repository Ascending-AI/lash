use super::*;

pub(crate) fn benchmark_stream_profile(scenario: RuntimePerfScenario) -> BenchmarkStreamProfile {
    benchmark_stream_profile_for_request(scenario, &empty_request())
}

pub(super) fn benchmark_stream_profile_for_request(
    scenario: RuntimePerfScenario,
    request: &LlmRequest,
) -> BenchmarkStreamProfile {
    if (matches!(
        scenario,
        RuntimePerfScenario::RlmSubagentSpawn
            | RuntimePerfScenario::DurableAgentChildTurnSqlite
            | RuntimePerfScenario::DurableAgentChildTurnPostgres
            | RuntimePerfScenario::RlmObliqueStackMix
            | RuntimePerfScenario::DeepTurnComposition
    ) || scenario.is_high_traffic())
        && request
            .instructions
            .as_deref()
            .is_some_and(|text| text.contains("Subagent capability: default. Depth: 1/5."))
    {
        if matches!(scenario, RuntimePerfScenario::DeepTurnComposition) {
            return text_profile(lashlang_block(
                r#"
sleep for "0ms"
result = await tools.benchmark_async({ value: len(chunk), delay_ms: 0 })?
finish { len: result.value }"#,
            ));
        }
        return text_profile(lashlang_block("finish { len: len(chunk) }"));
    }

    if request.output_spec.is_some() || request.session_id().ends_with("-llm-query") {
        if request.output_spec.as_ref().is_some_and(|spec| {
            matches!(spec, LlmOutputSpec::JsonSchema(schema) if schema.name == "tool_search_rerank")
        }) {
            return text_profile(
                serde_json::json!({
                    "tool_names": [
                        "GMAIL_SEND_EMAIL",
                        "GMAIL_CREATE_EMAIL_DRAFT",
                        "GMAIL_LIST_MESSAGES",
                        "exec_command"
                    ]
                })
                .to_string(),
            );
        }
        if request.output_spec.as_ref().is_some_and(|spec| {
            matches!(spec, LlmOutputSpec::JsonSchema(schema) if schema.name == "runtime_perf_oblique_judge")
        }) {
            return text_profile(
                serde_json::json!({
                    "ranked_doc_ids": ["doc_4_0000", "doc_4_0005", "doc_4_0010"],
                    "rationale": "synthetic direct judge response"
                })
                .to_string(),
            );
        }
        return text_profile(
            serde_json::json!({
                "kind": "value",
                "value": "runtime perf benchmark ok",
                "error": null,
            })
            .to_string(),
        );
    }

    if scenario.is_high_traffic() {
        return high_traffic_stream_profile(request);
    }

    match scenario {
        RuntimePerfScenario::OpenAiCompatStream => {
            let alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
            let mut deltas = Vec::with_capacity(OPENAI_COMPAT_STREAM_CHUNK_COUNT + 1);
            for index in 0..OPENAI_COMPAT_STREAM_CHUNK_COUNT {
                let prefix = format!("chunk-{index:03}: ");
                let fill_len = OPENAI_COMPAT_STREAM_CHUNK_BYTES.saturating_sub(prefix.len() + 1);
                let body: String = alphabet
                    .chars()
                    .cycle()
                    .skip(index % alphabet.len())
                    .take(fill_len)
                    .collect();
                deltas.push(format!("{prefix}{body}\n"));
            }
            deltas.push("runtime perf benchmark ok".to_string());
            BenchmarkStreamProfile {
                full_text: deltas.concat(),
                deltas,
                parts: Vec::new(),
            }
        }
        RuntimePerfScenario::StandardToolCalls
        | RuntimePerfScenario::DurableStandardToolTurnSqlite
        | RuntimePerfScenario::DurableStandardToolTurnPostgres => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-batch-call",
                    "batch",
                    serde_json::json!({
                        "tool_calls": [
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 1,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 2,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 3,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 4,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 5,
                                }
                            },
                            {
                                "tool": "benchmark_echo",
                                "parameters": {
                                    "value": "runtime perf benchmark ok",
                                    "ordinal": 6,
                                }
                            }
                        ]
                    }),
                )
            }
        }
        RuntimePerfScenario::StandardAsyncToolCompletion => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-async-completion-call",
                    "benchmark_async",
                    serde_json::json!({
                        "value": "runtime perf benchmark ok"
                    }),
                )
            }
        }
        RuntimePerfScenario::StandardShellOutput => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-shell-output-call",
                    "exec_command",
                    serde_json::json!({
                        "cmd": "for i in $(seq 1 160); do printf 'runtime-perf-shell-line-%03d abcdefghijklmnopqrstuvwxyz0123456789\\n' \"$i\"; done",
                        "timeout_ms": 5000,
                        "max_output_tokens": 4096
                    }),
                )
            }
        }
        RuntimePerfScenario::ToolDiscoverySearch => {
            if request_has_tool_result(request) {
                text_profile("runtime perf benchmark ok")
            } else {
                tool_call_profile(
                    "standard-tool-discovery-call",
                    "search_tools",
                    serde_json::json!({
                        "query": "gmail send email draft label message search",
                        "module": "gmail",
                        "limit": 8
                    }),
                )
            }
        }
        RuntimePerfScenario::Rlm
        | RuntimePerfScenario::RlmLargeToolCatalog
        | RuntimePerfScenario::RlmToolCatalogCold
        | RuntimePerfScenario::RlmToolCatalogWarm
        | RuntimePerfScenario::EmbedRlm
        | RuntimePerfScenario::TraceJsonlExtended => {
            let text = lashlang_block(r#"finish "runtime perf benchmark ok""#);
            text_profile(text)
        }
        RuntimePerfScenario::RlmStreamedPairedLashlang => {
            let full_text = concat!(
                "Visible preface before executable code.\n",
                "<lashlang>\n",
                "value = \"runtime perf benchmark ok\"\n",
                "finish value\n",
                "</lashlang>\n",
                "This suffix must be ignored after the close tag."
            )
            .to_string();
            BenchmarkStreamProfile {
                full_text,
                deltas: vec![
                    "Visible preface before executable code.\n<lash".to_string(),
                    "lang>\nvalue = \"runtime perf benchmark ok\"\n".to_string(),
                    "finish value\n</lash".to_string(),
                    "lang>\nThis suffix must be ignored after the close tag.".to_string(),
                ],
                parts: Vec::new(),
            }
        }
        RuntimePerfScenario::RlmGlobals => {
            // Mix of small (inline) and large (degraded preview) globals so the
            // benchmark exercises both branches of `render_bound_variables`:
            // small values render in full, while `big_map`/`big_notes`/`big_text`
            // exceed the inline budget and go through the keys / head-tail
            // truncation path that runs on every prompt build.
            let text = lashlang_block(
                r#"
big_map = {}
for i in range(24) {
  big_map[format("room_{}", i)] = { exits: ["north", "south", "east"], items: [format("item_{}", i)] }
}

big_notes = []
for i in range(45) {
  big_notes = push(big_notes, format("note {}: long observation about world state, plan, and next steps", i))
}
big_text = "Loud Room: "
for i in range(40) {
  big_text = format("{}echo step {} dampens the acoustics; ", big_text, i)
}
live_record = {
  status: "ready",
  turn: input.turn,
  goal: input.goal,
  nested: {
    path: input.path,
    labels: ["runtime", "rlm", "globals"],
    counters: { first: 1, second: 2, third: 3 }
  }
}
live_list = [
  { name: "alpha", count: 1 },
  { name: "beta", count: 2 },
  { name: "gamma", count: 3 }
]
live_message = "runtime perf benchmark ok"
host_snapshot = { benchmark: benchmark, input: input, chat: chat }
finish live_message"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmLargePrint => {
            let text = lashlang_block(
                r#"
big_text = ""
for i in range(70) {
  big_text = format("{}line {}: abcdefghijklmnopqrstuvwxyz0123456789 abcdefghijklmnopqrstuvwxyz0123456789\n", big_text, i)
}

rows = []
for i in range(16) {
  rows = push(rows, {
    id: format("row_{}", i),
    status: "ok",
    exit_code: 0,
    stderr: "short diagnostic stderr remains visible",
    output: big_text
  })
}

payload = {
  status: "ok",
  error: null,
  exit_code: 0,
  stderr: "short diagnostic stderr remains visible",
  output: big_text,
  rows: rows,
  metadata: {
    scenario: "rlm_large_print",
    turn: 1,
    tags: ["runtime", "projection", "print"]
  }
}

result = await tools.benchmark_echo({ value: payload, ordinal: 1 })?
print result
finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmToolCalls
        | RuntimePerfScenario::DurableRlmCheckpointTurnSqlite
        | RuntimePerfScenario::DurableRlmCheckpointTurnPostgres => {
            let text = lashlang_block(
                r#"
first = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 1 })?
second = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 2 })?
third = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 3 })?
fourth = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 4 })?
finish first.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmAsyncToolCompletion => {
            let text = lashlang_block(
                r#"
first = await tools.benchmark_async({ value: "runtime perf benchmark ok", delay_ms: 0 })?
second = await tools.benchmark_async({ value: "runtime perf benchmark ok", delay_ms: 0 })?
finish first.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmProcessHandles => {
            let text = lashlang_block(
                r#"
process benchmark_echo_process(tool: Tools, value: str, ordinal: int) {
  result = await tool.benchmark_echo({ value: value, ordinal: ordinal })?
  finish result
}

process benchmark_slow_process(tool: Tools, value: str, delay_ms: int) {
  result = await tool.benchmark_slow({ value: value, delay_ms: delay_ms })?
  finish result
}

first = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 1)
second = start benchmark_echo_process(tool: tools, value: "runtime perf benchmark ok", ordinal: 2)
slow = start benchmark_slow_process(tool: tools, value: "cancelled", delay_ms: 50)
live = await processes.list({})?
cancel slow
first_result = (await first)?
second_result = (await second)?
finish first_result.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmTriggerMailPipeline => {
            let text = lashlang_block(
                r#"
@label(title: "Define and register mail forwarder")
process forward_mail(event: mail.Received) {
  if event.account == "test" {
    await inbox.test23.send({
      title: format("[Fwd from test] {}", event.title),
      text: event.text
    })?
  }
  finish true
}

existing = await triggers.list({
  name: "runtime-perf-test-to-test23-forwarder",
  enabled: true
})?

if len(existing) > 0 {
  handle = existing[0]
} else {
  handle = await triggers.register({
    source: mail.received({}),
    target: forward_mail,
    inputs: { event: trigger.event },
    name: "runtime-perf-test-to-test23-forwarder"
  })?
}

@label(title: "Send test message to inbox.test")
sent = await inbox.test.send({
  title: "Hello from test",
  text: "This is a forwarding test for runtime perf stack profiling."
})?

finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => {
            let text = lashlang_block(
                r#"
process benchmark_async_process(tool: Tools, value: str) {
  result = await tool.benchmark_async({ value: value, delay_ms: 0 })?
  finish result
}

first = start benchmark_async_process(tool: tools, value: "runtime perf benchmark ok")
second = start benchmark_async_process(tool: tools, value: "runtime perf benchmark ok")
first_result = (await first)?
second_result = (await second)?
finish first_result.value"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::AsyncProcessSettlement2Children
        | RuntimePerfScenario::AsyncProcessSettlement8Children => {
            let children = scenario
                .settlement_children()
                .expect("async settlement child count");
            let starts = (0..children)
                .map(|index| {
                    format!(
                        "child_{index} = start settlement_child(tool: tools, value: \"child-{index}\")"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = lashlang_block(&format!(
                r#"
process settlement_child(tool: Tools, value: str) {{
  result = await tool.benchmark_async({{ value: value, delay_ms: 0 }})?
  finish result
}}

{starts}
finish "runtime perf benchmark ok""#
            ));
            text_profile(text)
        }
        RuntimePerfScenario::RlmSubagentSpawn
        | RuntimePerfScenario::DurableAgentChildTurnSqlite
        | RuntimePerfScenario::DurableAgentChildTurnPostgres => {
            let text = lashlang_block(
                r#"
process spawn_child(agents: Agents) {
  result = await agents.spawn({
    capability: "default",
    task: "Submit `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["alpha", "beta", "gamma"] },
    output: Type { len: int }
  })?
  finish result
}

handle = start spawn_child(agents: agents)
result = (await handle)?
finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::DurableCheckpointCurveSqlite
        | RuntimePerfScenario::DurableCheckpointCurvePostgres => {
            unreachable!("durable checkpoint curves bypass the provider harness")
        }
        RuntimePerfScenario::RlmObliqueStackMix => {
            let text = lashlang_block(
                r#"
process explore(agents: Agents) {
  result = await agents.spawn({
    capability: "default",
    task: "Return `{ len: len(chunk) }` using the seeded chunk.",
    seed: { chunk: ["obliq", "retrieval", "rerank", "trace"] },
    output: Type { len: int }
  })?
  finish result
}

first_pool = await obliq.search({
  queries: [
    "latent algebraic invariant transfer",
    "proof strategy analogue with distractor wording",
    "operator comparison under hidden structure",
    "same abstract solution different surface topic"
  ],
  mode: "hybrid",
  limit: 72,
  candidate_pool: 512
})?

second_pool = await obliq.search({
  queries: [
    "geometric proof reused as combinatorial invariant",
    "relevance by method not vocabulary",
    "avoid surface lexical overlap"
  ],
  mode: "hybrid",
  limit: 72,
  candidate_pool: 512
})?

candidate_ids = [match.doc_id for match in first_pool.matches]
for match in second_pool.matches {
  candidate_ids = push(candidate_ids, match.doc_id)
}

subagent_handle = start explore(agents: agents)
handles = await obliq.list_async_handles({})?
judged = await obliq.judge_candidates({
  verifier_predicate: "documents share the same abstract proof strategy, not topic words",
  candidate_doc_ids: candidate_ids,
  surface_bait: ["same vocabulary", "same topic"]
})?
subagent = (await subagent_handle)?

print {
  first_pool: first_pool,
  second_pool: second_pool,
  handles: handles,
  judged: judged,
  subagent: subagent
}

finish "runtime perf benchmark ok""#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::IngressClaimProjection => {
            if latest_request_item_contains(request, "ingress projection marker") {
                text_profile(lashlang_block(r#"finish "runtime perf benchmark ok""#))
            } else {
                text_profile(lashlang_block(r#"print("checkpoint before projection")"#))
            }
        }
        RuntimePerfScenario::DeepTurnComposition => {
            if request_text(request).contains("deep composition ingress marker") {
                return text_profile(lashlang_block(r#"finish "runtime perf benchmark ok""#));
            }
            let text = lashlang_block(
                r#"
process deep_child(agents: Agents, tool: Tools) {
  pending = await tool.benchmark_async({ value: "parent tool loop", delay_ms: 0 })?
  sleep for "0ms"
  child = await agents.spawn({
    capability: "default",
    task: "Use the seeded chunk and return its length after the durable waits.",
    seed: { chunk: ["parent", "child", "tool", "wait"] },
    output: Type { len: int }
  })?
  finish { pending: pending.value, child: child.len }
}

handle = start deep_child(agents: agents, tool: tools)
result = (await handle)?
print result"#,
            );
            text_profile(text)
        }
        RuntimePerfScenario::RlmLlmQuery => {
            let text = lashlang_block(
                r#"
result = await llm.query({
  task: "Return the exact benchmark marker.",
  inputs: { marker: "runtime perf benchmark ok" }
})?
finish result"#,
            );
            text_profile(text)
        }
        _ => text_profile("runtime perf benchmark ok"),
    }
}

pub(super) fn high_traffic_stream_profile(request: &LlmRequest) -> BenchmarkStreamProfile {
    let kind = high_traffic_operation_kind(request);
    if kind == Some("tool") {
        return text_profile(lashlang_block(
            r#"result = await tools.benchmark_echo({ value: "runtime perf benchmark ok", ordinal: 1 })?
finish result.value"#,
        ));
    }
    if kind == Some("child") {
        return text_profile(lashlang_block(
            r#"process load_child(agents: Agents) {
  result = await agents.spawn({
    capability: "default",
    task: "Submit `{ len: len(chunk) }` using the seeded `chunk` variable.",
    seed: { chunk: ["alpha", "beta", "gamma"] },
    output: Type { len: int }
  })?
  finish result
}
handle = start load_child(agents: agents)
result = (await handle)?
finish "runtime perf benchmark ok""#,
        ));
    }
    if kind == Some("wake") {
        return text_profile(lashlang_block(
            r#"process load_wake(tool: Tools) {
  result = await tool.benchmark_async({ value: "runtime perf benchmark ok", delay_ms: 0 })?
  finish result
}
handle = start load_wake(tool: tools)
result = (await handle)?
finish result.value"#,
        ));
    }
    if kind == Some("trigger") {
        let trigger_name = high_traffic_trigger_name(request);
        let trigger_name = serde_json::to_string(&trigger_name)
            .expect("high-traffic trigger name always serializes");
        return text_profile(lashlang_block(&format!(
            r#"process load_forward(event: mail.Received) {{
  finish event.title
}}
existing = await triggers.list({{ name: {trigger_name}, enabled: true }})?
if len(existing) == 0 {{
  handle = await triggers.register({{
    source: mail.received({{}}),
    target: load_forward,
    inputs: {{ event: trigger.event }},
    name: {trigger_name}
  }})?
}}
finish "runtime perf benchmark ok""#,
        )));
    }
    text_profile(lashlang_block(r#"finish "runtime perf benchmark ok""#))
}

pub(super) fn high_traffic_operation_kind(request: &LlmRequest) -> Option<&str> {
    const KINDS: [&str; 6] = ["plain", "tool", "queued", "child", "wake", "trigger"];
    let text = request_text(request);
    let kind = text
        .rsplit_once("load-kind:")
        .and_then(|(_, suffix)| suffix.split_whitespace().next())?;
    KINDS.into_iter().find(|candidate| *candidate == kind)
}

pub(super) fn high_traffic_trigger_name(request: &LlmRequest) -> String {
    let request_text = request_text(request);
    let session_id = request_text
        .rsplit_once("session:")
        .and_then(|(_, suffix)| suffix.split_whitespace().next())
        .filter(|value| {
            !value.is_empty()
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .unwrap_or("missing-session");
    format!("runtime-perf-load-trigger-{session_id}")
}

pub(super) fn lashlang_block(source: &str) -> String {
    format!("<lashlang>\n{}\n</lashlang>", source.trim())
}

pub(super) fn text_profile(text: impl Into<String>) -> BenchmarkStreamProfile {
    let text = text.into();
    BenchmarkStreamProfile {
        full_text: text.clone(),
        deltas: vec![text],
        parts: Vec::new(),
    }
}

pub(super) fn tool_call_profile(
    call_id: impl Into<String>,
    tool_name: impl Into<String>,
    args: serde_json::Value,
) -> BenchmarkStreamProfile {
    BenchmarkStreamProfile {
        full_text: String::new(),
        deltas: Vec::new(),
        parts: vec![LlmOutputPart::ToolCall {
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            input_json: args.to_string(),
            replay: None,
        }],
    }
}

pub(super) fn request_has_tool_result(request: &LlmRequest) -> bool {
    request.messages.iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    })
}

pub(super) fn latest_request_item_contains(request: &LlmRequest, needle: &str) -> bool {
    // RLM appends its synthetic iteration prompt after history. The rolling
    // cache fence identifies the newest real request item before that suffix.
    request
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    LlmContentBlock::Text {
                        cache_breakpoint: true,
                        ..
                    }
                )
            })
        })
        .is_some_and(|message| message_contains(message, needle))
}

pub(super) fn message_contains(message: &lash_core::llm::types::LlmMessage, needle: &str) -> bool {
    message.blocks.iter().any(|block| match block {
        LlmContentBlock::Text { text, .. } => text.contains(needle),
        LlmContentBlock::ToolResult { content, .. } => content.contains(needle),
        LlmContentBlock::ToolCall {
            tool_name,
            input_json,
            ..
        } => tool_name.contains(needle) || input_json.contains(needle),
        LlmContentBlock::Reasoning { text, .. } => text.contains(needle),
        LlmContentBlock::Attachment { .. } => false,
    })
}

pub(super) fn request_text(request: &LlmRequest) -> String {
    let mut out = String::new();
    for message in &request.messages {
        for block in message.blocks.iter() {
            match block {
                LlmContentBlock::Text { text, .. } => out.push_str(text),
                LlmContentBlock::ToolResult { content, .. } => out.push_str(content),
                LlmContentBlock::ToolCall {
                    tool_name,
                    input_json,
                    ..
                } => {
                    out.push_str(tool_name);
                    out.push_str(input_json);
                }
                LlmContentBlock::Reasoning { text, .. } => out.push_str(text),
                LlmContentBlock::Attachment { .. } => {}
            }
            out.push('\n');
        }
    }
    out
}

pub(super) fn empty_request() -> LlmRequest {
    LlmRequest {
        instructions: None,
        model: "mock-model".to_string(),
        messages: Vec::new(),
        resolved_stored: Default::default(),
        tools: std::sync::Arc::new(Vec::new()),
        tool_choice: Default::default(),
        generation: Default::default(),
        model_variant: Default::default(),
        model_capability: lash_core::ModelCapability::default(),
        scope: LlmRequestScope::new(
            "runtime-perf-empty".to_string(),
            "runtime-perf-empty:frame".to_string(),
            "runtime-perf-empty:request".to_string(),
        ),
        output_spec: None,
        stream_events: None,
        provider_trace: None,
    }
}
