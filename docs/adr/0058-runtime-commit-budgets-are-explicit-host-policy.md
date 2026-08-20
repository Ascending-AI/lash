# Runtime commit budgets are explicit host policy

## Status

Accepted.

## Context

[ADR 0047](0047-history-is-shared-branches-are-sessions.md) established the
pre-transaction commit guard while also naming 1 MiB and 512 nodes as Lash's
authoritative limits. That combined two decisions which change for different
reasons: Lash owns a backend-independent admission mechanism, while an
embedding host owns the latency and capacity envelope that mechanism enforces.

The required-explicit-bounds idiom already exists for durable Lashlang process
lifetimes in [ADR 0055](0055-lashlang-execution-bounds-span-durable-process-lifetimes.md).
The ownership follows [ADR 0014](0014-operational-policy-stays-with-the-host.md)
and [ADR 0023](0023-retention-stays-a-parameterized-host-lever.md): Lash exposes
and enforces a lever without choosing deployment policy for the host.

## Decision

Every host supplies one required `CommitBudget`. Its byte and node fields each
explicitly choose either a non-zero bound or `Unbounded`; `CommitBudget` has no
`Default`. Bytes are the logical persisted payload. The node field bounds rows
written by the commit: graph-node rows plus attachment-intent adoption rows.
The host resolves that value once as part of runtime construction, and every
`RuntimeCommit` carries it separately from semantic commit identity. The facade
and the in-memory, SQLite, and PostgreSQL backends all enter the shared
realization path, which validates that same carried value before a backend
transaction starts. Adoption-row evidence is recorded during the turn and
threaded into the commit; admission never queries the store.

The former 1 MiB byte limit and 512-node limit remain only a documented
recommended starting point, with "node" now meaning a row written under that
field's widened contract. Hosts tune them for their own backend latency and
capacity envelope. Exceeding either dimension is a typed terminal rejection on
turn settlement, public append, and park; retrying the same commit unchanged
cannot succeed, so a host must increase its configured bound or submit a
smaller commit.

### Reference sizing curve

The reference target is a **60 ms p95 physical commit interval** at the 1 MiB
logical-byte point. The measured interval is the elapsed production
`commit_runtime_state` call with session admission and manifest fixture seeding
outside the timer. It conservatively encloses SQLite's writer-lock hold and the
PostgreSQL transaction interval rather than pretending those backends expose an
identical lock primitive. On the reference setup, 1 MiB stays under that target
on both backends: SQLite is 29.484 ms p95 and PostgreSQL is 44.657 ms p95. That
is why 1 MiB remains the recommended logical-byte starting point. The 512-row
node bound remains a separate starting point; its curve is published so hosts
can choose a lower row limit when their latency target requires one.

The 2026-08-20 reference setup was an AMD Ryzen 9 5950X host with 125 GiB RAM
and local NVMe-backed ext4 storage. SQLite used its factory-wide WAL database;
PostgreSQL used a same-host `postgres:16-alpine` container. The benchmark ran an
optimized (`cargo test --release`) build, took three warmups and 21 measured
samples per point, and reports nearest-rank p50/p95. Checkpoints use a seeded
uniform-byte generator, 32 small component bodies, and three large component
bodies so backend compression cannot turn the sweep into a constant-byte
write. The byte curve holds 96 graph rows plus 32 adoption rows fixed. The row
curve holds logical bytes at 512 KiB and uses a 25/75 graph/adoption mix.
Half of each adoption set is carried as explicit attachment references and half
is owner-only, so the commit exercises both production adoption paths.

Logical accounting and physical cost are deliberately separate below. The
logical columns come from the same production measurement used for admission.
The latency columns time the physical commit with an unbounded benchmark budget
so the 1.25 MiB and 640-row over-limit points actually execute. Fresh manifest
rows are inserted in one untimed fixture transaction per sample; the measured
commit still runs the production attachment-adoption statements.

| axis | target | backend | logical bytes | graph rows | adoption rows | total rows | logical checkpoint bytes | p50 commit ms | p95 commit ms | samples |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|
| logical_bytes | 262144 | sqlite | 262144 | 96 | 32 | 128 | 216429 | 7.052 | 7.992 | 21 |
| logical_bytes | 262144 | postgres | 262144 | 96 | 32 | 128 | 216397 | 20.596 | 22.963 | 21 |
| logical_bytes | 524288 | sqlite | 524288 | 96 | 32 | 128 | 478573 | 13.422 | 14.273 | 21 |
| logical_bytes | 524288 | postgres | 524288 | 96 | 32 | 128 | 478541 | 25.752 | 29.566 | 21 |
| logical_bytes | 786432 | sqlite | 786432 | 96 | 32 | 128 | 740717 | 19.962 | 20.438 | 21 |
| logical_bytes | 786432 | postgres | 786432 | 96 | 32 | 128 | 740685 | 25.130 | 30.165 | 21 |
| logical_bytes | 1048576 | sqlite | 1048576 | 96 | 32 | 128 | 1002845 | 27.097 | 29.484 | 21 |
| logical_bytes | 1048576 | postgres | 1048576 | 96 | 32 | 128 | 1002813 | 26.856 | 44.657 | 21 |
| logical_bytes | 1310720 | sqlite | 1310720 | 96 | 32 | 128 | 1264989 | 33.522 | 38.581 | 21 |
| logical_bytes | 1310720 | postgres | 1310720 | 96 | 32 | 128 | 1264957 | 28.549 | 48.734 | 21 |
| rows_written | 64 | sqlite | 524288 | 16 | 48 | 64 | 515400 | 12.758 | 13.464 | 21 |
| rows_written | 64 | postgres | 524288 | 16 | 48 | 64 | 515352 | 14.688 | 15.385 | 21 |
| rows_written | 256 | sqlite | 524288 | 64 | 192 | 256 | 489022 | 14.101 | 15.445 | 21 |
| rows_written | 256 | postgres | 524288 | 64 | 192 | 256 | 488830 | 32.284 | 58.101 | 21 |
| rows_written | 512 | sqlite | 524288 | 128 | 384 | 512 | 453884 | 16.133 | 16.490 | 21 |
| rows_written | 512 | postgres | 524288 | 128 | 384 | 512 | 453500 | 51.167 | 56.087 | 21 |
| rows_written | 640 | sqlite | 524288 | 160 | 480 | 640 | 436310 | 16.901 | 17.220 | 21 |
| rows_written | 640 | postgres | 524288 | 160 | 480 | 640 | 435830 | 64.299 | 83.972 | 21 |

## Consequences

- Missing configuration fails runtime construction instead of silently
  inheriting Lash-owned constants.
- Bounded and intentionally unbounded deployments are both explicit and
  serializable.
- One policy value reaches every host commit surface and every backend; no
  backend may substitute a local limit.
- Changing the recommendation does not change the runtime contract or durable
  semantic commit identity.
- Attachment adoption consumes the existing node budget rather than adding a
  byte surcharge or a third independently configured bound.
