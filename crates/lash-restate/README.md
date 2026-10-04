# lash-restate

`lash-restate` is Lash's Restate engine. A deployment starts its endpoint from
`RestateEngine::endpoint_builder`, which binds Lash's own services beside the
host's: the `LashSession` virtual object works each session and the `LashTurn`
workflow executes each run it admits, through a `RestateRuntimeEffectController`
(a `RuntimeEffectController`). A host handler never runs a turn (FIG-3600). It
sends and, where it waits, waits durably:

```rust,ignore
// Inside a service, workflow, or shared handler: `ctx` holds no exclusive lock.
let handle = session
    .send(lash::TurnInput::text(request.text))
    .id(request.turn_id)
    .accept_restate(&ctx)
    .await?;
let outcome = handle
    .outcome_restate(&ctx, lash::restate::RestateWait::new())
    .await?;
```

Serve the built endpoint with host-selected incoming message limits:

```rust,ignore
let limits = lash_restate::RestateEndpointLimits::new(
    32 * 1024 * 1024,
    32 * 1024 * 1024 + 8,
);
lash_restate::serve_endpoint(listener, endpoint, limits, shutdown).await;
```

The first limit counts one service-protocol message's payload. The second
counts its pending framed bytes, including the eight-byte header. The endpoint
checks each declared length before forwarding payload bytes to the SDK and
stops a refused input stream. It retains only a fixed header and counters beside
the SDK's bounded incomplete-message buffer. HTTP/2 flow control also limits
queued input. A replay may contain arbitrarily many legal messages; there is no
aggregate request limit. These limits belong to the deployment, as ADR 0025
requires, and do not impose a core-wide tool-result ceiling. The supplied hosts
choose 32 MiB per payload and eight additional bytes for framing.

Several deployments share one `restate-server` by namespace (ADR 0111):
`RestateConfig::with_namespace` prefixes every service name the engine binds or
calls (`alpha.LashSession`), and `RestateEngine::register_deployment` registers
the endpoint only when no other deployment holds those names. The default
namespace keeps the bare names.

`accept_restate` journals the input id before it accepts, so every replay of the
handler submits under the same id; `outcome_restate` follows the run in bounded,
journaled probes, so the wait survives suspension, replay, and a turn longer than
the invocation's timers. An exclusive object handler cannot wait for a run its
own object may serve: it accepts, returns the receipt, and a shared handler or
the caller waits.

The adapter records Lash LLM calls, tool attempts, independent direct
completions, checkpoints and execution-surface syncs with named Restate runs.
A direct completion made by opaque tool code is captured inside that attempt.
Standard batch expansion and the code interpreter dispatch admitted members
through the same Run coordinator; they create no wrapper invocation. Runtime
sleeps outside tool attempts use Restate durable timers.

The workspace pins SDK 0.12.1 from `SamGalanakis/sdk-rust` at
`c25608305340e431dff8808a3303bc3106762d6c` in `Cargo.toml` and `Cargo.lock` (H01).
`lash-restate` alone owns that dependency. Hosts import
`lash_restate::restate_sdk` or `lash::restate::restate_sdk`; they do not add
a direct SDK edge or a separate handler context.
Sequential `ctx.run` calls retain fluent `.name(...)` and `.retry_policy(...)`
configuration and are awaited directly. To start concurrent work, configure
each run and call consuming `.start()` in deterministic order before awaiting
any result. Started closures own their captures and futures for the invocation;
they keep progressing while the handler awaits another result. Sequential
actions may borrow local values. Hosts use the existing SDK re-export and one
Endpoint.

This git pin supports development and runtime acceptance, not crates.io
publication. U02/FIG-4906 must replace it with an upstream release containing
the concurrent-run fix or the approved Lash-owned published fork, remove the
`deny.toml` allow-git entry, and prove registry-resolvable dependencies and the
external-consumer build. Runtime and reset-gate success do not waive that exit.

A started result is settled only once awaited. Dropping its future neither
cancels nor settles the run, and a successful return drops any run still
executing without recording it. A run's bounded retry budget counts failed
attempts since the invocation's last recorded entry, so a sibling's recorded
result restarts it; concurrent handles share no per-handle budget.
`tests/server_semantics.rs` in `lash-restate-test` pins these rules with the
crash, retry and cancellation laws for started runs.

The [SDK acceptance recipe](SDK_ACCEPTANCE.md) selects the unchanged handler
laws and the concurrent-run laws by full test path, verifies executed-case
receipts, and records the accepted dependency and predecessor live V7 proof.

The Run journals admitted calls and independent attempts in its opener. It
records final-or-cancel, protected drain, presentation and incorporation there.
SQL stores retain domain state and final session commits; they do not replay
effects. The [tool-run contract](../../docs/architecture/tool-run-contract.md)
and [ADR 0099](../../docs/adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md)
define K1-K10 and their laws.

Each handler's effect journal keeps a payload of at least 1 KiB inline once.
Later envelopes and outcomes name it by a domain-separated BLAKE3 digest.
Replay rebuilds the dictionary from served entries before checking the exact
canonical envelope bytes. A discarded run cannot supply a reference, and a
missing reference is a typed integrity refusal. The dictionary lasts for the
handler attempt; cold reopen reads the journal to rebuild it.

`RunOutcome::Committed` and the shift reply carry the run id and terminal
kind. Callers read the answer from `RunTerminalCause::Committed` in the run's
durable terminal record, or through the send handle. The turn's workflow state
and replies therefore carry no second copy of the answer. The committed turn's
terminal promise carries its status and typed stop, preserving cancellation
evidence without the answer. A scope close records only its acknowledgement.

For host-tier HTTP integration, use `RestateIngressClient` to submit `/send`
requests and capture the returned invocation id. The client accepts Restate's
`Accepted` and `PreviouslyAccepted` send statuses and returns the
`RestateInvocationId` from the response body, so the host can track a durable
turn invocation instead of modeling it as local in-process work.
`RestateAdminClient` cancels those active invocations through the Admin API,
queries invocation status, and exposes unfinished-invocation introspection for
tests and cleanup. `kill_invocation` stops an invocation for good; it is the
release half of an operator's cancel or fork of a parked run, run only after
the store recorded the run's end. The Restate CLI remains a
useful operator tool, but Lash tests and examples use these HTTP APIs directly.

Deterministic contract failures are terminal handler errors, not retry loops.
If a replayed recorded effect no longer matches the current Lash envelope hash,
or a previously recorded Restate run completed with a terminal failure, the
adapter returns an explicit terminal error code to the handler. Hosts should
surface that failure and clear their running state rather than leaving the
invocation to back off forever.

Lash binds its session and turn handlers, `LashProcessWorkflow`, and the durable
wait index/workflow, including immutable source seals. A deployment serving
Lash work starts with `RestateEngine::endpoint_builder` and binds its own host
services on that same Endpoint. A submit-only host serves no handlers.

```rust,no_run
use lash_restate::{RestateEngine, RestateProcessServing, restate_sdk};

fn endpoint(
    engine: &RestateEngine,
    worker: lash_core::DurableProcessWorker,
) -> restate_sdk::endpoint::Endpoint {
    engine
        .endpoint_builder(RestateProcessServing::new(worker))
        .build()
}
```

The Run coordinates its admitted callbacks and recorded attempts on the owning
handler. Process-backed work uses a captured environment and admitted worker;
it needs no child resolver or independently reconstructed tool context.

A missing binding is itself a deterministic contract failure, so it is treated
as one: Restate answers an invocation of a service no deployment binds with
`404`, and the adapter classifies that (`RestateHttpError::is_service_unregistered`)
and raises the engine's own `404`-class terminal naming the address nothing
binds. A retryable 404 would turn a forgotten `bind` into an invocation backing
off forever with no operator told what is wrong.

`RestateHttpError::classification` distinguishes transient ingress failures
from definitive answers. A process await reattaches after connection failures,
EOF, timeouts, overload, and ingress-generated 5xx responses, preserving the
durable process and its wait address. An invocation's terminal error stays
terminal even with a 5xx code. See ADR 0016.

An already-terminal process await records the outcome after acquiring receiver
attachment references. Otherwise short process-terminal registration delivers
the result to a retained source seal. Transfer rebinds subscriptions and acquires
successor leases before predecessor release. A `Resolved(ref)` winner drains
protected finalization; a `Cancelled` seal cannot later revive work. Recorded
cancellation/revocation observations decide the journaled command branch.

## Run recovery and deployment drain

A paused invocation retains its journal. Redrive and cancel address its logical
Run owner; recorded material, admission, source seals and protected finals remain
authoritative. Missing material refuses typed rather than rerunning a body.
`tests::run_coordinator_on_the_double::owner_park` holds the owner-recovery law
on the Restate server double.

Wait workflows retain scoped promise and revocation facts. Deferred tool sources
have no runtime deadline, timeout terminal or long attach invocation. Tools own
transport timeouts inside their bodies; Runs retain cancellation and existing
turn/no-progress bounds. Generic sleep and retry timers retain their own jobs.
A physical cut quiesces issued local attempts through durable acknowledgement,
then transfers pending sources without closing the logical opener.

At turn start the handler records its cancellation observation. Later branches
use recorded Run decisions, gate peeks and subscribed wakes. A host-local stop
reaches the durable gate; an out-of-band observation never chooses replay's
command order. Compatible redrive serves durable X without body execution.

Non-forced deployment removal requires `unfinished_invocations` and independently
owned old work to be drained. Transferred pending sources alone must not pin the
predecessor. A failed admin query refuses removal. The
[deployment guide](../../docs/operations/deploying-and-upgrading.md) describes the
operator contract; an invocation kill cannot substitute for it.

The controller submits workflow `run` with workflow key
`ProcessRegistration.id` and sends cancellation to the workflow's shared
`cancel` handler. The workflow runner should be built from the host's
deployment config: plugin factories, runtime host config, session-store
factory, process registry, attachment store, and provider policy.
Process rows carry the process input plus `ProcessProvenance`: originator
and optional causal parent. Tool and Lashlang rows also carry a
captured execution-environment reference, so workers do not parse grant keys or
rebuild origin sessions to recover execution context.

`Call<T>` also checks JSON structure before the SDK constructs `T`. Its default
allowances are 32 MiB of encoded bytes, 1,000,000 nodes, depth 64, and 128 MiB of
estimated allocation bytes. Object keys and values each count as nodes; the
root has depth one. The estimate charges 64 bytes per node plus each string's
encoded content length. It estimates JSON storage, not arbitrary allocations a
custom deserializer may perform. The iterative preflight holds only counters,
then checks syntax without constructing a value tree. The wire probe skips the
body, so an unsupported range never materializes it. Direct callers can choose
all four allowances with `Call::decode_json_with_limits`; SDK ingress uses the
default allowances. Remote envelopes and turn inputs use the same preflight
through their `decode_json_with_limits` methods.

## Release journal replay

The replay law reads `LASH_REPLAY_CORPUS_ROOT` as the directory containing
`<scenario>/journal.json`, the `replay-corpus` leg of
`lash.release-fixtures-manifest.v1`. An explicit root has no fallback.
Without the variable it reads `testdata/replay-corpus`.

Each `journal.json` records the complete build generation `G` that wrote it.
Controller fixtures hold one ordered `journal` of named effects or process
command facts. Each `service-<Service>` fixture holds the ordered command
sequences its real handlers wrote on the server double. A law derives the
service list from the source and fails when a service has no scenario.
The capture manifest names the source commit; journals repeat no provenance.

Both fixture kinds use the generation bound by the corpus's real
standard-protocol core, including its fixture process-engine plugin in hook
order. Replay compares only matching generations. A handler step added,
removed or reordered then fails with
`journal logic changed: bump JOURNAL_LOGIC_EPOCH`. Another generation returns
`DifferentGeneration` and prints `different generation, not compared`.
Controller and service added-step self-tests prove both outcomes without
changing production constants.

```sh
kiln test //crates/lash-restate:lash-restate__unit_test \
  --local-test-execution --no-test-cache \
  --test_arg=tests::replay_corpus:: --test_arg=--nocapture \
  --test_env LASH_REPLAY_CORPUS_ROOT=crates/lash-restate/testdata/replay-corpus
```

The `Release journal replay` workflow runs on main and registered-surface
pull requests. It stays non-required under FIG-4097 during the version
freeze and reads the current-tree corpus at `testdata/replay-corpus`. The cut
repoints its root to `fixtures/release/v1.0.0/replay-corpus` and makes the job
required as described in `docs/release/cut-1.0.md`.
