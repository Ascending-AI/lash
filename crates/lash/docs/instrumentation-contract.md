# Lash instrumentation contract

Scope `lash`, version `1.0`. GenAI snapshot `open-telemetry/semantic-conventions-genai@07794021bfe46e1c241408b6f55cb4354bd28318`. Exported changes require a release note. No schema URL is claimed.

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
| `execute_tool` | Internal | Transition | gen_ai.execute_tool.internal |

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
| `http.response.status_code` | Integer | Provenance | model_attempt |
| `lash.session.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.turn.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.attempt.invocation_id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `gen_ai.operation.name` | String | Provenance | agent, model, tool, cell, workflow |
| `gen_ai.request.model` | String | Provenance | model |
| `gen_ai.response.model` | String | Provenance | model |
| `gen_ai.tool.name` | String | Provenance | tool, cell, process, tool_intent |
| `gen_ai.tool.call.id` | String | Provenance | tool, process, tool_intent |
| `lash.cell.id` | String | Provenance | cell |
| `lash.cell.language` | String | Provenance | cell |
| `lash.cell.tool_calls` | Integer | Provenance | cell |
| `gen_ai.usage.input_tokens` | Integer | Provenance | model |
| `gen_ai.usage.output_tokens` | Integer | Provenance | model |
| `gen_ai.usage.cache_read.input_tokens` | Integer | Provenance | model |
| `gen_ai.usage.cache_write.input_tokens` | Integer | Provenance | model |
| `gen_ai.usage.reasoning.output_tokens` | Integer | Provenance | model |
| `lash.model.attempt.ordinal` | Integer | Provenance | model_attempt |
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
| `lash.usage.input_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.output_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.cache_read_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.cache_write_tokens` | Integer | Provenance | model_attempt |
| `lash.usage.reasoning_tokens` | Integer | Provenance | model_attempt |
| `lash.provider.request_id` | String | Provenance | model_attempt |
| `lash.process.kind` | String | Provenance | process, workflow |
| `lash.process.label` | String | Provenance | process, workflow |
| `gen_ai.workflow.name` | String | Provenance | workflow |
| `lash.tool.id` | String | Provenance | tool |
| `lash.tool.attempts` | Integer | Provenance | tool |
| `lash.tool.provider_call_id` | String | Provenance | tool |
| `lash.outcome` | String | Provenance | agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent |
| `lash.turn.inputs` | Integer | Provenance | agent |
| `lash.cancel.origin` | String | Provenance | agent |
| `gen_ai.system_instructions` | String | Provenance | model |
| `gen_ai.input.messages` | String | Provenance | model |
| `gen_ai.output.messages` | String | Provenance | model |
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
| `lash.recovery_leader.name` | String | Provenance | metrics only |
| `gen_ai.tool.type` | String | Refinable | tool, cell |
| `gen_ai.provider.name` | String | Refinable | model |
| `error.type` | String | Refinable | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |

| Span kind | Attribute reserve |
|---|---|
| `producer` | 20 |
| `agent` | 23 |
| `model` | 45 |
| `model_attempt` | 28 |
| `tool` | 31 |
| `cell` | 28 |
| `process` | 26 |
| `workflow` | 24 |
| `workflow_node` | 20 |
| `tool_intent` | 24 |
| `extension` | 19 |
| `point` | 19 |

| Metric | Kind | Unit | Ownership |
|---|---|---|---|
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
| `lash.recovery_leader` | Gauge | `` | Physical |
| `lash.recovery_leader.term` | Gauge | `` | Physical |
