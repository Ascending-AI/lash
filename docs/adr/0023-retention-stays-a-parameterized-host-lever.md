# Retention stays a parameterized host lever

## Decision

Hosts schedule differentiated retention through `prune_terminal_processes(cutoff, filter, watermark)`. `ProcessListFilter` selects provenance, identity and creation ranges; `ProjectionWatermark::UpTo(cursor)` limits deletion to acknowledged changes under ADR 0020, while `NoProjector` explicitly states there is no projector. The public `Processes::prune` forwards the filter and refuses filters selecting non-terminal work.

Eligibility protects pending deliveries, cleanup obligations and consumer holds. Pruning retains typed tombstone evidence and coordinates trigger retention under ADR 0021. Hosts retain process evidence beyond every still-replayable waiter; Lash supplies no finite maximum waiter lifetime.

## Durable-core evidence retention

`SessionStoreFactory::reclaim_retained_evidence(RetentionBound)` is an explicit factory-wide lever with an exclusive commit-timestamp horizon. Only receipts belonging to durably deleted sessions are eligible. Permanent deleted-session identity evidence remains. Live receipts and usage deltas survive; terminal usage is reclaimed only when its matching receipt is absent.

SQLite and PostgreSQL delete eligible receipts and dependent usage in one fenced transaction. Reports describe committed counts; errors roll back the operation. Repetition after exhausting the eligible set removes nothing. Attachment liveness follows referrer and graph-retirement contracts under ADRs 0028, 0113 and 0124 rather than receipt age. SQL stores own no effect journals; Restate invocation journals use engine retention under ADR 0025.

`vacuum` cleans eligible tombstoned graph and terminal ingress rows without a receipt horizon. Blob GC uses its separate explicit policy. Trigger mutation receipts have a low-level pruning primitive but no public pruning facade or production schedule; age alone cannot prove a retry identity dead, so those receipts remain retained.

## Why and alternatives

Producer-declared retention classes are rejected because retention windows are host operational policy rather than an execution correctness declaration. A projector watermark is required because pruning unacknowledged state would destroy completeness evidence. The host chooses how much eligible evidence to keep; Lash owns eligibility and atomic deletion.

[Process retention contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs), [SQLite receipt retention](../../crates/lash-sqlite-store/src/retention.rs) and [PostgreSQL receipt retention](../../crates/lash-postgres-store/src/postgres/evidence_retention.rs) implement these levers.
