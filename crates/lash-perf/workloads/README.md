# Synthetic multi-node workload

`figments-v1.json` implements FIG-3790 design sections 2 and 3 for lane L1.
Its traffic mix, rates, byte buckets and topology settings are synthetic defaults.
The first measurements may justify revised defaults before baseline extraction.
The `inventory` object pins the Lash and Figments source snapshots used by the
design. This directory contains no production data and needs no Figments checkout.
Workload format 1 and ChaCha20 generator version 1 are independent of Lash's
frozen runtime formats.

`provenance.fields` names every setting by JSON pointer. Arrays are one setting.
`I` marks an inferred synthetic default; `V` points to the design's verified
source reference. Attachment bucket probabilities are inferred, while their
maximum follows the verified image limit. The original design's provenance
labels remain in the file for context.

Source the fork's environment before running these commands:

```sh
. ./env.sh
kiln run //crates/lash-perf:lash-perf__bin -- workload-validate \
  --file crates/lash-perf/workloads/figments-v1.json
kiln run //crates/lash-perf:lash-perf__bin -- workload-schema \
  --out crates/lash-perf/workloads/workload-v1.schema.json
kiln test //crates/lash-perf:workload__test --test_output=all
```

The JSON Schema is generated from the Rust contract. The focused fixture test
checks freshness. `Workload::parse` also rejects duplicate typed fields,
probability sums other than one, repeated buckets, incomplete provenance,
invalid attachment splits, fault ordering, and incompatible settings that
standard JSON Schema cannot express. Unknown fields, missing fields, nulls,
versions, numeric limits and individual probabilities are rejected.

A `Generator` accepts validated settings and a run ID. `plan(actor, ordinal)`
returns a primary-turn plan. Tools and child processes have independent
membership and can overlap. Auxiliary operation counts use their own streams:
integer rate plus a Bernoulli draw for the fractional part. They do not consume
primary membership. Exponential arrival gaps model each actor's open-loop
Poisson clock. The runner must accumulate scheduled times without waiting for
acceptance or completion. History prefill and cron phases have separate streams.
Rotation, compaction, cancellation, deletion, queued input and observation
settings remain explicit in plans or the validated workload.

Every stream uses SHA-256 domain separation over the workload seed, actor,
operation ordinal and purpose, then pinned `rand_chacha` 0.9.0 ChaCha20.
Run/actor/ordinal keys identify operations; named child keys identify tools,
processes, queued inputs and blobs. Retries regenerate the same plan and reuse
these keys. Adding another purpose does not consume an existing stream.

`materialize` creates synthetic UTF-8 prompts, nested JSON records, a strict
tool schema, arguments/results and PNG attachments. Text and compact serialized
JSON match the sampled byte counts. PNGs contain a seeded RGBA pixel plus an
ancillary padding chunk, with exact byte counts, CRCs and SHA-256 digests.
Aggregate attachment sizes split evenly, distributing remainder bytes first.
Every tenth ordinal can share an attachment between an adjacent actor pair;
both plans name the same blob and both owners. No session pair crosses the
configured session population.

`provider_response` creates a TypeScript cell for every primary turn, including
turns without tools or processes. Cells contain tool batches and durable process
definitions, starts, awaits and delayed signals. Opening and closing delimiters
occupy their own lines, as the RLM scanner requires. `llm_request` and
`llm_response` supply separately keyed auxiliary requests with independently
sampled prompt sizes, output sizes, latency and retry decisions. Their responses
are plain completions.
Its decoded response body, including cell framing and synthetic padding,
matches the sampled provider output byte count. Stream chunks preserve UTF-8
and their final deadline equals the sampled total latency. Only the first
attempt is retryable when selected; successful response content and operation
identity remain the same. Fresh calls, replayed effects and retries have separate
counters. A host-start process body is available independently.

`fixtures/sizes-v1.json` covers every text/JSON byte bucket and all six attachment
aggregate/count combinations. `fixtures/mix-v1.json` sets a 25,000-plan sample
and statistical bounds for 12 distributions, overlapping membership and
conditional/auxiliary rates. Provider fixtures parse 300 responses and their
retries with the existing TypeScript frontend. These are input-generation
checks. The later workload and measurement lanes own public API execution,
callback services, clocks, witnesses and proof of model-code pool execution.
