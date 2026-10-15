# Lash instrumentation contract

Scope `lash`, version `1.0`. GenAI snapshot `b31e9e8ea26ac1c086d3313d474e31d7c3f391ae`. Exported changes require a release note. No schema URL is claimed.

| Span name or prefix | Kind | Ownership |
|---|---|---|
| `lash.send` | Producer | Live |
| `lash.send.batch` | Producer | Live |
| `lash.process.start` | Producer | Live |
| `lash.tool_intent.submit` | Producer | Live |
| `lash.plugin.invoke` | Producer | Live |
| `invoke_agent` | Internal | Transition |
| `chat` | Client | Live |
| `execute_tool` | Internal | Transition |
| `lash.tool_intent` | Internal | Transition |
| `lash.process` | Internal | Transition |
| `execute_tool` | Internal | Transition |

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
| `http.response.status_code` | Integer | Provenance | metrics only |
| `lash.session.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.turn.id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.attempt.invocation_id` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `gen_ai.operation.name` | String | Provenance | agent, model, tool, cell, workflow |
| `gen_ai.request.model` | String | Provenance | model, process, tool_intent |
| `gen_ai.response.model` | String | Provenance | model |
| `gen_ai.tool.name` | String | Provenance | tool, cell, process, tool_intent |
| `gen_ai.tool.call.id` | String | Provenance | tool, process, tool_intent |
| `gen_ai.tool.type` | String | Provenance | cell |
| `lash.cell.id` | String | Provenance | cell |
| `lash.cell.language` | String | Provenance | cell |
| `lash.cell.tool_calls` | Integer | Provenance | cell |
| `gen_ai.usage.input_tokens` | Integer | Provenance | model, process, tool_intent |
| `gen_ai.usage.output_tokens` | Integer | Provenance | model, process, tool_intent |
| `gen_ai.usage.cache_read.input_tokens` | Integer | Provenance | model, process, tool_intent |
| `gen_ai.usage.cache_write.input_tokens` | Integer | Provenance | model, process, tool_intent |
| `lash.usage.reasoning_output_tokens` | Integer | Provenance | model, process, tool_intent |
| `lash.model.attempt.ordinal` | Integer | Provenance | model_attempt |
| `lash.model.attempts` | Integer | Provenance | model |
| `lash.request.model_variant` | String | Provenance | metrics only |
| `lash.response.text_chars` | Integer | Provenance | metrics only |
| `lash.outcome` | String | Provenance | agent, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent |
| `lash.cancel.origin` | String | Provenance | agent |
| `lash.payload.json` | String | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.payload.truncated` | Boolean | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.payload.truncated_fields` | Integer | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
| `lash.content.omitted` | Boolean | Provenance | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |
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
| `gen_ai.provider.name` | String | Refinable | model, process, tool_intent |
| `error.type` | String | Refinable | producer, agent, model, model_attempt, tool, cell, process, workflow, workflow_node, tool_intent, extension, point |

| Span kind | Attribute reserve |
|---|---|
| `producer` | 21 |
| `agent` | 23 |
| `model` | 30 |
| `model_attempt` | 22 |
| `tool` | 24 |
| `cell` | 27 |
| `process` | 32 |
| `workflow` | 22 |
| `workflow_node` | 21 |
| `tool_intent` | 32 |
| `extension` | 20 |
| `point` | 20 |

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
