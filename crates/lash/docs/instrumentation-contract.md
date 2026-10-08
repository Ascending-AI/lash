# Lash instrumentation contract

Scope `lash`, version `1.0`. GenAI snapshot `b31e9e8ea26ac1c086d3313d474e31d7c3f391ae`. Exported changes require a release note. No schema URL is claimed.

| Span name or prefix | Kind | Ownership |
|---|---|---|
| `lash.admission.attempt` | Internal | Candidate |
| `lash.run.admitted` | Internal | Candidate |
| `lash.turn.admitted` | Internal | Candidate |
| `lash.tool.admitted` | Internal | Candidate |
| `lash.tool_intent.admitted` | Internal | Candidate |
| `lash.process.admitted` | Internal | Candidate |
| `lash.send` | Producer | Live |
| `lash.run` | Internal | Transition |
| `invoke_agent` | Internal | Transition |
| `chat` | Client | Live |
| `execute_tool` | Internal | Transition |
| `lash.tool_intent` | Internal | Live |
| `lash.process` | Internal | Transition |
| `lash.process.segment` | Internal | Transition |
| `lash.wait` | Internal | Transition |
| `lash.exec_code` | Internal | Live |

| Attribute | Type |
|---|---|
| `lash.scope.kind` | String |
| `lash.scope.boundary` | Integer |
| `lash.admission.outcome` | String |
| `lash.links.omitted` | Integer |
| `lash.record.id` | String |
| `lash.event.type` | String |
| `lash.session.id` | String |
| `lash.turn.id` | String |
| `lash.attempt.invocation_id` | String |
| `gen_ai.operation.name` | String |
| `gen_ai.provider.name` | String |
| `gen_ai.request.model` | String |
| `gen_ai.response.model` | String |
| `gen_ai.tool.name` | String |
| `gen_ai.tool.call.id` | String |
| `gen_ai.usage.input_tokens` | Integer |
| `gen_ai.usage.output_tokens` | Integer |
| `gen_ai.usage.cache_read.input_tokens` | Integer |
| `gen_ai.usage.cache_write.input_tokens` | Integer |
| `lash.usage.reasoning_output_tokens` | Integer |
| `lash.model.attempt.ordinal` | Integer |
| `lash.request.model_variant` | String |
| `lash.response.text_chars` | Integer |
| `lash.outcome` | String |
| `lash.wait.kind` | String |
| `error.type` | String |
| `lash.context.metadata` | String |
| `lash.payload.json` | String |
| `lash.payload.truncated` | Boolean |
| `lash.payload.truncated_fields` | Integer |
| `lash.events.omitted` | Integer |
| `lash.tool_intent.kind` | String |
| `lash.tool_intent.refusal_reason` | String |
| `lash.provider` | String |
| `lash.provider.retry.kind` | String |
| `lash.session_execution_lane.wait.outcome` | String |
| `lash.session_execution_lane.give_up` | String |
| `lash.store.pool.acquire.outcome` | String |
| `lash.durable.commit.label` | String |
| `lash.runtime_commit.budget.outcome` | String |
| `lash.parked_work.kind` | String |
| `lash.parked_work.reason` | String |
| `lash.obligation.kind` | String |
| `lash.obligation.outcome` | String |
| `lash.recovery_leader.name` | String |

| Metric | Kind | Unit | Ownership |
|---|---|---|---|
| `lash.provider.retries` | Counter | `` | Live |
| `lash.provider.throttle_wait.duration` | Histogram | `ms` | Live |
| `lash.session_execution_lane.contention_wait.duration` | Histogram | `ms` | Live |
| `lash.session_execution_lane.give_ups` | Counter | `` | Live |
| `lash.queued_work.wake_retries` | Counter | `` | Live |
| `lash.store.pool.acquire_wait.duration` | Histogram | `ms` | Physical |
| `lash.durable.commit.acquire_wait.duration` | Histogram | `us` | Physical |
| `lash.durable.commit.transaction.duration` | Histogram | `us` | Physical |
| `lash.durable.commit.sql_statements` | Histogram | `` | Physical |
| `lash.durable.commit.returned_bytes` | Histogram | `By` | Physical |
| `lash.durable.commit.lock_statement_elapsed` | Histogram | `us` | Physical |
| `lash.durable.commit.group_commit.members` | Histogram | `` | Physical |
| `lash.runtime_commit.budgeted_size` | Histogram | `By` | Live |
| `lash.parked_work.parks` | Counter | `` | Transition |
| `lash.parked_work.count` | Gauge | `` | Gauge |
| `lash.parked_work.oldest_age` | Gauge | `ms` | Gauge |
| `lash.tool_intent.executed` | Counter | `` | Transition |
| `lash.tool_intent.refused` | Counter | `` | Transition |
| `lash.obligation.attempts` | Counter | `` | Transition |
| `lash.obligations.stalled` | Gauge | `` | Gauge |
| `lash.recovery_leader` | Gauge | `` | Physical |
| `lash.recovery_leader.term` | Gauge | `` | Physical |
