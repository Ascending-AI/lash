# Independent 1.0 boundaries

`lash-perf boundary` runs one population and writes a functional JSON receipt.
Build once and keep its output report; the CLI and its SQLite child writers use
that materialized executable in place:

```sh
kiln build //crates/lash-perf:lash-perf__bin --materializations final \
  --build-report "$E/boundary-build.json"
kiln run //crates/lash-perf:lash-perf__bin -- boundary \
  --case wire-slots --operations 1 --callers 2 \
  --store-dir "$E/wire-slots-store" --out "$E/wire-slots.json"
```

Every perf core constructs its stable node name with `LeaseOwnerId` and its
boot identity with `LeaseIncarnationId`. Concurrent boundary writers retain
distinct node names (`writer-<lane>`), so a later writer cannot fence an
earlier writer by swapping those constructor arguments.

The store directory must be fresh. Select each case independently; there is no
aggregate population that folds unlike boundary costs into one number.

| Case | Operations and measured boundaries |
|---|---|
| `wire-slots` | One lowering per call, original resolution, provider-file upload/cache hit/invalidation, and delivery/send on each of three attempts. The synthetic provider rejects a file, fails before a response, then answers; the product derivative cache uploads twice and reuses the replacement on the final attempt. |
| `token-healthy`, `token-expiring`, `token-rejected` | `--operations` waves of `--callers` concurrent callers on one route and epoch. Host-source requests and gate waits are separate; a barrier establishes shared leases before refresh. Healthy waves never replace; expiring/rejected waves single-flight one replacement. |
| `root-redrive` | A sent root parks on an unserved profile; restoring the deployment and explicitly committing redrive mail makes it finish. Park, operator mail, and settlement have separate intervals. Root redrive currently uses the durable mail API alongside facade sends. |
| `parked-takeover` | A deferred tool call survives node shutdown with its key and deadline unchanged. Pinned-call restoration, resolution, the next owner's durable claim, and settlement are separate. Takeover spans node restart through the new claim; the fixture models graceful owner loss. |
| `sqlite-processes` | At least two OS processes open independent product stores and cores on one SQLite file before the parent releases their stdin barriers. Each writes `--operations` sends; child acceptance/provider/settlement intervals and parent boot/join intervals remain separate. |
| `pg-facade` | `--callers` facade nodes, each with its own product PostgreSQL pool/listener, share a baseline-initialized database and concurrently settle `--operations` total sends. The workload refuses a server outside major 18. |
| `typed-history` | Bounded raw-history pages plus typed committed-turn decoding and a client fold, with separate page, node, turn and entry counts. Cursor traversal must observe every sent turn exactly once. |
| `process-lifecycle` | Repeated session-turn process start, terminal await and observation with one active process at a time. Alternate waves hold the synthetic provider until explicit cancellation; successful and cancelled terminals are counted separately. |
| `seeded-plan` | `--workload smoke-v1` or `figments-v1` supplies deterministic plans for `--callers` actors and `--operations` turns each. Generated Code mode cells execute tools, attachment puts and child processes through the current served durable node. Keyed queued inputs run separately. The receipt records seed/hash and lists unmeasured host-process, auxiliary-LLM, fault-window, maintenance, arrival and observation fields. |
