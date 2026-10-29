# Lash instrumentation contract

Scope `lash`, version `1.0`. GenAI snapshot `open-telemetry/semantic-conventions-genai@07794021bfe46e1c241408b6f55cb4354bd28318`. Exported changes require a release note. No schema URL is claimed.

`TurnCompleted.output` carries the settled assistant answer under `OutputMessages` consent; its closer alone supplies `gen_ai.output.messages` on `invoke_agent`. An assistant message supplies its text; a declared finish control supplies one assistant text part, with string values verbatim and other values as compact JSON. Content is omitted by default and passes through the content hook and bounds. Model decorators read the logical chat span from `LlmRequestScope.trace_carrier` (absent when no model span is built). Content hooks identify tool records through `ContentView.tool_name`.

Openers carry no content: `LlmCallStarted` always carries an omitted request; hosts read request content from `LlmCallCompleted` or `LlmCallFailed`. `ResponseMetadata` is a separate consent category for record sinks, not an OTel-exported content attribute.

`ContentLimits::standard()` cuts parts at 16 KiB, which truncates typical agent system prompts. Hosts wanting the full prompt raise `max_part_bytes` (Figments uses 64 KiB parts and 256 KiB attributes).

`lash.model.attempt.response_metadata.keys` names the response-metadata keys the provider leaf captured for the attempt, such as `header:x-opper-cost`, and never their values. A host process start's `lash.process.start` producer carries one `lash.process.start.<phase>_us` attribute per phase the start reached; the phases run back to back from the producer's opening. Definition starts split descriptor loading (`definition_load`), engine derivation (`definition_resolve`), closure acquisition and its store waits (`definition_acquire`), and a supplied signature claim's verification (`definition_verify`); `definition` holds the remaining payload construction or inline realization. Sum all five for the former coarse definition total.

`lash.provider.validation` on `invoke_agent` retains a failed turn's Lash-owned provider-validation check even when the model call completed. `empty_response` means assistant output contained neither visible text nor tool calls. It carries no provider text and needs no content opt-in.

FIG-7426: `lash.effect.refused.kind`, `lash.effect.refused.controller_code` and `lash.effect.refused.detail` on `invoke_agent` retain the refused operation and the controller’s namespaced code and bounded diagnostic. The same `effect_refusal` is carried by the turn settlement, failure envelope and terminal trace record. No new opt-in is required.

Provider terminal delivery timeouts set `lash.provider.terminal_delivery_timed_out` on the logical call and attempt spans and increment `lash.provider.terminal_delivery.timeouts`. A bounded invocation does not promise completed host settlement.

FIG-7452: Failed HTTP attempts retain a content-free transport kind in `error.type` and `lash.transport.error.kind`, monotonic elapsed milliseconds in `lash.transport.error.elapsed_ms`, and a parsed endpoint host of at most 253 ASCII bytes in `server.address`. Kinds are `dns`, `connect`, `tls`, `connect_timeout`, `request_timeout`, `reset`, `body_read`, and `other`. The 1-based attempt ordinal and `lash.model.attempt.retry_delay_ms` identify the attempt and selected next wait, with zero when no retry is scheduled. HTTP failures retain status and a typed provider code in `lash.provider.error.code` only when it is at most 128 ASCII letters, digits, underscores, dots or hyphens. No URL paths, queries, credentials, prompts or raw error text are exported. The host owns admission spans such as Figments `lash.llm_admission` and must consume the typed evidence there.

FIG-7553: Failed model-call and attempt spans retain `http.response.status_code`, `lash.model.response_headers_ms`, `lash.model.first_body_chunk_ms`, `lash.model.bytes_received`, `lash.model.protocol_position` and `lash.model.partial_output`. Timings use the injected monotonic clock, relative to HTTP send. Missing observations stay absent. Partial output is a bounded ordered set of text, reasoning, tool_arguments and other_event, with no content. The logical call uses its final failed attempt.

Vendor rendering: the pinned semconv 07794021 uses `tool_call_response.response` and `reasoning`. Laminar reads `result`/`thinking` and semconv messages only on LLM spans. Hosts map these themselves; for example, map an agent span's `gen_ai.output.messages` into the host agent-output representation and translate nested tool response/reasoning parts for that renderer.

| Span name or prefix | Kind | Ownership | Semconv type |
|---|---|---|---|
| `lash.send` | Producer | Live | — |
| `lash.send.batch` | Producer | Live | — |
| `lash.process.start` | Producer | Live | — |
| `lash.tool_intent.submit` | Producer | Live | — |
| `lash.plugin.invoke` | Producer | Live | — |
| `invoke_agent` | Internal | Transition | gen_ai.invoke_agent.internal |
| `chat` | Client | Live | gen_ai.client.inference |
| `lash.model.attempt` | Client | Live | — |
| `execute_tool` | Internal | Transition | gen_ai.execute_tool.internal |
| `lash.tool_intent` | Internal | Transition | — |
| `lash.process` | Internal | Transition | — |
| `invoke_workflow` | Internal | Transition | gen_ai.invoke_workflow.internal |
| `lash.workflow.node` | Internal | Transition | — |
| `execute_tool` | Internal | Transition | gen_ai.execute_tool.internal |
| `lash.extension` | Internal | Live | — |

| Attribute | Type | Mutability | Set on |
|---|---|---|---|
| `lash.scope.kind` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.scope.boundary` | Integer | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.producer.outcome` | String | Provenance | producer |
| `lash.links.omitted` | Integer | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.record.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.event.type` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `gen_ai.conversation.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.run.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.process.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.llm_call.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.send.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `server.address` | String | Provenance | model_attempt |
| `lash.transport.error.kind` | String | Provenance | model_attempt |
| `lash.transport.error.elapsed_ms` | Integer | Provenance | model_attempt |
| `lash.provider.error.code` | String | Provenance | model_attempt |
| `lash.model.attempt.retry_delay_ms` | Integer | Provenance | model_attempt |
| `http.response.status_code` | Integer | Provenance | model, model_attempt |
| `lash.model.response_headers_ms` | Integer | Provenance | model, model_attempt |
| `lash.model.first_body_chunk_ms` | Integer | Provenance | model, model_attempt |
| `lash.model.bytes_received` | Integer | Provenance | model, model_attempt |
| `lash.model.protocol_position` | String | Provenance | model, model_attempt |
| `lash.model.partial_output` | StringArray | Provenance | model, model_attempt |
| `lash.session.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.turn.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.attempt.invocation_id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `gen_ai.operation.name` | String | Provenance | agent, model, tool, cell, workflow, extension |
| `gen_ai.request.model` | String | Provenance | model, extension |
| `gen_ai.data_source.id` | String | Provenance | extension |
| `lash.plugin.id` | String | Provenance | extension, point |
| `lash.plugin.callback` | String | Provenance | extension, point |
| `lash.span.ended_by` | String | Provenance | extension |
| `lash.span.point` | Boolean | Provenance | point |
| `gen_ai.response.model` | String | Provenance | model |
| `gen_ai.tool.name` | String | Provenance | tool, cell, process, tool_intent |
| `gen_ai.tool.call.id` | String | Provenance | tool, process, tool_intent |
| `lash.cell.id` | String | Provenance | cell |
| `lash.cell.charges.consumed` | Integer | Provenance | cell |
| `lash.cell.charges.limit` | Integer | Provenance | cell |
| `lash.cell.charges.observed` | Integer | Provenance | cell |
| `lash.cell.worker_cpu_nanos.consumed` | Integer | Provenance | cell |
| `lash.cell.worker_cpu_nanos.limit` | Integer | Provenance | cell |
| `lash.cell.worker_cpu_nanos.observed` | Integer | Provenance | cell |
| `lash.cell.held_wall_nanos.consumed` | Integer | Provenance | cell |
| `lash.cell.held_wall_nanos.limit` | Integer | Provenance | cell |
| `lash.cell.held_wall_nanos.observed` | Integer | Provenance | cell |
| `lash.cell.memory_bytes.consumed` | Integer | Provenance | cell |
| `lash.cell.memory_bytes.limit` | Integer | Provenance | cell |
| `lash.cell.memory_bytes.observed` | Integer | Provenance | cell |
| `lash.cell.consumption` | String | Provenance | cell |
| `lash.cell.language` | String | Provenance | cell |
| `lash.cell.trailing_discarded` | Integer | Provenance | cell |
| `lash.cell.tool_calls` | Integer | Provenance | cell |
| `gen_ai.usage.input_tokens` | Integer | Provenance | model |
| `gen_ai.usage.output_tokens` | Integer | Provenance | model |
| `gen_ai.usage.cache_read.input_tokens` | Integer | Provenance | model |
| `gen_ai.usage.cache_write.input_tokens` | Integer | Provenance | model |
| `gen_ai.usage.reasoning.output_tokens` | Integer | Provenance | model |
| `lash.model.attempt.ordinal` | Integer | Provenance | model_attempt |
| `lash.provider.terminal_delivery_timed_out` | Boolean | Provenance | model, model_attempt |
| `lash.model.attempt.response_metadata.keys` | StringArray | Provenance | model_attempt |
| `lash.model.attempts` | Integer | Provenance | model |
| `gen_ai.request.stream` | Boolean | Provenance | model |
| `gen_ai.request.reasoning.level` | String | Provenance | model |
| `lash.request.reasoning` | String | Provenance | model |
| `gen_ai.output.type` | String | Provenance | model |
| `gen_ai.request.max_tokens` | Integer | Provenance | model |
| `gen_ai.request.seed` | Integer | Provenance | model |
| `gen_ai.request.temperature` | Double | Provenance | model |
| `gen_ai.request.stop_sequences` | StringArray | Provenance | model |
| `gen_ai.response.id` | String | Provenance | model |
| `gen_ai.response.finish_reasons` | StringArray | Provenance | model |
| `lash.usage.overflow` | Boolean | Provenance | model |
| `lash.usage.unknown` | Boolean | Provenance | model |
| `lash.usage.input_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.output_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.cache_read_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.cache_write_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.reasoning_tokens` | Integer | Provenance | model_attempt |
| `lash.provider.request_id` | String | Provenance | model_attempt |
| `lash.process.kind` | String | Provenance | process, workflow |
| `lash.process.start.dispatch_us` | Integer | Provenance | producer |
| `lash.process.start.admission_us` | Integer | Provenance | producer |
| `lash.process.start.environment_us` | Integer | Provenance | producer |
| `lash.process.start.definition_load_us` | Integer | Provenance | producer |
| `lash.process.start.definition_resolve_us` | Integer | Provenance | producer |
| `lash.process.start.definition_acquire_us` | Integer | Provenance | producer |
| `lash.process.start.definition_verify_us` | Integer | Provenance | producer |
| `lash.process.start.definition_us` | Integer | Provenance | producer |
| `lash.process.start.admit_us` | Integer | Provenance | producer |
| `lash.process.start.artifacts_us` | Integer | Provenance | producer |
| `lash.process.start.link_check_us` | Integer | Provenance | producer |
| `lash.process.start.input_us` | Integer | Provenance | producer |
| `lash.process.start.prepare_us` | Integer | Provenance | producer |
| `lash.process.start.commit_us` | Integer | Provenance | producer |
| `lash.process.start.adopt_us` | Integer | Provenance | producer |
| `lash.process.start.observe_us` | Integer | Provenance | producer |
| `lash.process.label` | String | Provenance | process, workflow |
| `gen_ai.workflow.name` | String | Provenance | workflow |
| `lash.workflow.node.site` | String | Provenance | workflow_node |
| `lash.workflow.node.occurrence` | String | Provenance | workflow_node |
| `lash.tool.id` | String | Provenance | tool |
| `lash.tool.attempts` | Integer | Provenance | tool |
| `lash.tool.provider_call_id` | String | Provenance | tool |
| `lash.outcome` | String | Provenance | agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent |
| `lash.turn.sql.statements` | Integer | Provenance | agent |
| `lash.turn.sql.dml` | Integer | Provenance | agent |
| `lash.turn.sql.transactions` | Integer | Provenance | agent |
| `lash.turn.store.bytes_written` | Integer | Provenance | agent |
| `lash.turn.snapshots` | Integer | Provenance | agent |
| `lash.turn.manifests` | Integer | Provenance | agent |
| `lash.turn.link_payloads` | Integer | Provenance | agent |
| `lash.turn.profile.omitted_intervals` | Integer | Provenance | agent |
| `lash.turn.profile.omitted_sql_keys` | Integer | Provenance | agent |
| `lash.turn.inputs` | Integer | Provenance | agent |
| `lash.effect.refused.kind` | String | Provenance | agent |
| `lash.effect.refused.controller_code` | String | Provenance | agent |
| `lash.effect.refused.detail` | String | Provenance | agent |
| `lash.provider.validation` | String | Provenance | agent |
| `lash.cancel.origin` | String | Provenance | agent |
| `gen_ai.system_instructions` | String | Provenance | model |
| `gen_ai.input.messages` | String | Provenance | agent, model |
| `gen_ai.output.messages` | String | Provenance | agent, model |
| `gen_ai.tool.definitions` | String | Provenance | model |
| `gen_ai.tool.call.arguments` | String | Provenance | tool, cell |
| `gen_ai.tool.call.result` | String | Provenance | tool, cell |
| `lash.tool.arguments.wrapped` | Boolean | Provenance | tool |
| `lash.tool.result.wrapped` | Boolean | Provenance | tool |
| `lash.content.truncated` | StringArray | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.content.oversize` | StringArray | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.content.dropped_messages` | Integer | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.tool_intent.kind` | String | Provenance | process, tool_intent |
| `lash.tool_intent.refusal_reason` | String | Provenance | process, tool_intent |
| `lash.trace.omitted` | StringArray | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.trace.omitted.count` | Integer | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.provider` | String | Provenance | metrics only |
| `lash.provider.retry.kind` | String | Provenance | metrics only |
| `lash.store.pool.acquire.outcome` | String | Provenance | metrics only |
| `lash.durable.commit.label` | String | Provenance | metrics only |
| `lash.runtime_commit.budget.outcome` | String | Provenance | metrics only |
| `lash.parked_work.kind` | String | Provenance | metrics only |
| `lash.parked_work.reason` | String | Provenance | metrics only |
| `lash.obligation.kind` | String | Provenance | metrics only |
| `lash.obligation.outcome` | String | Provenance | metrics only |
| `gen_ai.tool.type` | String | Refinable | tool, cell |
| `gen_ai.provider.name` | String | Refinable | model, extension |
| `error.type` | String | Refinable | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |

| Span kind | Attribute reserve |
|---|---|
| `producer` | 36 |
| `agent` | 38 |
| `model` | 53 |
| `model_attempt` | 40 |
| `tool` | 31 |
| `cell` | 42 |
| `process` | 26 |
| `workflow` | 24 |
| `workflow_node` | 22 |
| `tool_intent` | 24 |
| `extension` | 26 |
| `point` | 22 |

| Metric | Kind | Unit | Ownership |
|---|---|---|---|
| `gen_ai.client.inference.duration` | Histogram | `s` | Live |
| `gen_ai.client.inference.time_to_first_chunk` | Histogram | `s` | Live |
| `gen_ai.client.inference.usage.input_tokens` | Counter | `{token}` | Live |
| `gen_ai.client.inference.usage.output_tokens` | Counter | `{token}` | Live |
| `gen_ai.client.inference.usage.cache_read.input_tokens` | Counter | `{token}` | Live |
| `gen_ai.client.inference.usage.cache_write.input_tokens` | Counter | `{token}` | Live |
| `gen_ai.client.inference.usage.reasoning.output_tokens` | Counter | `{token}` | Live |
| `gen_ai.client.inference.operation.input_tokens` | Histogram | `{token}` | Live |
| `gen_ai.client.inference.operation.output_tokens` | Histogram | `{token}` | Live |
| `gen_ai.invoke_agent.duration` | Histogram | `s` | Transition |
| `gen_ai.invoke_agent.inference_calls` | Histogram | `{inference_call}` | Transition |
| `gen_ai.invoke_agent.tool_calls` | Histogram | `{tool_call}` | Transition |
| `gen_ai.execute_tool.duration` | Histogram | `s` | Transition |
| `gen_ai.invoke_workflow.duration` | Histogram | `s` | Transition |
| `lash.trace.omissions` | Counter | `` | Live |
| `lash.provider.terminal_delivery.timeouts` | Counter | `` | Live |
| `lash.provider.retries` | Counter | `` | Live |
| `lash.provider.throttle_wait.duration` | Histogram | `ms` | Live |
| `lash.store.pool.acquire_wait.duration` | Histogram | `ms` | Physical |
| `lash.durable.commit.acquire_wait.duration` | Histogram | `us` | Physical |
| `lash.durable.commit.transaction.duration` | Histogram | `us` | Physical |
| `lash.durable.commit.sql_statements` | Histogram | `` | Physical |
| `lash.durable.commit.returned_bytes` | Histogram | `By` | Physical |
| `lash.durable.commit.lock_statement_elapsed` | Histogram | `us` | Physical |
| `lash.durable.commit.group_commit.members` | Histogram | `` | Physical |
| `lash.runtime_commit.budgeted_size` | Histogram | `By` | Live |
| `lash.parked_work.parks` | Counter | `` | Transition |
| `lash.tool_intent.executed` | Counter | `` | Transition |
| `lash.tool_intent.refused` | Counter | `` | Transition |
| `lash.obligation.attempts` | Counter | `` | Transition |
| `lash.obligations.stalled` | Gauge | `` | Gauge |

GenAI histograms supply advisory semconv boundaries. Durations in seconds: `[0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92]`. Token and call counts: `[1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0, 16777216.0, 67108864.0]`. Host meter-provider views take precedence.

## Turn physical work

`TurnCompleted.profile` carries bounded content-free physical owner-attempt
measurements. `invoke_agent` exports these integer attributes:

| Attribute | Unit | Meaning |
|---|---|---|
| `lash.turn.sql.statements` | statements | attempted SQL executions |
| `lash.turn.sql.dml` | statements | INSERT, UPDATE, DELETE and REPLACE executions |
| `lash.turn.sql.transactions` | transactions | BEGIN executions |
| `lash.turn.store.bytes_written` | bytes | newly installed canonical graph bytes, committed only on success |
| `lash.turn.snapshots` | calls | provider environment snapshot entries |
| `lash.turn.manifests` | calls | manifest collection in the default provider snapshot |
| `lash.turn.link_payloads` | calls | link-payload entries in the default provider snapshot |
| `lash.turn.profile.omitted_intervals` | intervals | phase intervals beyond the capture bound |
| `lash.turn.profile.omitted_sql_keys` | observations | detail beyond the SQL/write key bound |

Phase children are `lash.turn.claim`, `lash.turn.readback`, `lash.turn.admission`, `lash.turn.owner_orchestration`,
`lash.turn.prompt_environment_build`, `lash.turn.model_call`,
`lash.turn.tool_calls`, `lash.turn.cell_execution`,
`lash.turn.checkpoint_encode`, `lash.turn.commit`,
`lash.turn.store_queue_wait`, `lash.turn.store_gate_wait`,
`lash.turn.store_pool_wait`, `lash.turn.worker_checkout_wait` and
`lash.turn.worker_IPC`, when reached. Each has `lash.turn.phase`; commits also
have `lash.owner.revision`. Each child exports exclusive SQL counters for its
exact interval: `lash.turn.sql.statements`, `lash.turn.sql.round_trips`,
`lash.turn.sql.rows_returned`, `lash.turn.sql.rows_affected`,
`lash.turn.sql.bound_bytes` and `lash.turn.sql.result_bytes`.
Round trips count observed PostgreSQL statement submissions, excluding pool
checks and protocol control; SQLite contributes zero. Bytes are parameter
and returned scalar payload, excluding framing, WAL and fsync.
`lash.turn.sql.top_shapes` is a JSON array of at most eight shape entries,
ranked by statement count, with literals removed and each shape capped at
256 characters. Entries combine all origins for the same backend and shape, and contain
statements, round trips, rows and payload bytes.
`lash.turn.sql.omitted_shapes` counts additional captured backend/shape keys;
`lash.turn.profile.omitted_sql_keys` on the parent reports capture overflow.
Repeated phase names have independent counters. They parent to `invoke_agent` and follow its
sampling decision. Their kind is `Agent`; the host's export policy and bounded
enricher apply to each phase. Sinks recover the same phase identities through
`TurnProfile::phase_positions` using the terminal's record id and position.
Phase spans carry correlation ids and omit terminal content and usage.
Nested spans overlap; exclusive shares require an interval
union with precedence for the deepest active phase. No additional metric
labels contain SQL shapes, origins, revisions, session ids or turn ids.

Cold owners carry their successful physical claim into the first pass. Shared
batch SQL is assigned once to the first claimed actor; each member retains its
shared wall. Warm resident turns have no claim.

The profile retains at most 512 intervals, 512 SQL shape/origin keys and 512
graph-write kind keys per attempt. SQL literals are redacted. Physical totals
are distinct from logical lifecycle counters and do not survive owner death.
The flight recorder can trigger on a slow turn terminal using its retained
logical duration and the configured threshold.
