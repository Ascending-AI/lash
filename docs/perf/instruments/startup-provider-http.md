# Startup phases and loopback provider HTTP

```sh
kiln run //crates/lash-perf:lash-perf__bin -- startup \
  --store-dir "$PWD/.buck2/startup-population" --out "$E/startup.json"
kiln run //crates/lash-perf:lash-perf__bin -- provider-http \
  --requests 6 --rates 10,100,1000 --body-bytes 128,8192 \
  --max-inflight 2 --server-parallel 1 \
  --first-chunk-delay-ms 2 --chunk-delay-ms 1 --chunk-bytes 1024 \
  --retry-every 2 --out "$E/provider-http.json"
```

`startup` launches concurrent fresh-process waves at widths 1, 2 and 4. Each
child opens a fresh SQLite file with `Normal` synchronous mode, builds a durable
node, observes its registration, creates a session, prewarms one real VM helper,
and submits its first loopback-provider turn through `send()`. Each process uses
two configured Tokio workers. The existing `latency worker ready` handshake
still marks core construction; node registration is a separate marker. Use a
fresh `--store-dir` for each run; existing database files are refused. OS page
and executable caches are uncontrolled, and no cache is dropped.

The opt-in `perf_witness::startup` recorder retains the first of each named
phase, sharing one monotonic epoch initialized at process entry. It joins the
first connection open/setup, store-set completion, core construction, durable
node registration, session creation, VM spawn/spawned/ready, first provider
request, and first committed send result. VM markers retain the helper PID and
are parent observations. Child clocks are never subtracted from parent launch
clocks. Parent launch/ready/exit intervals and child entry-to-first-result
samples remain separate in the receipt. Width summaries preserve raw markers,
completion counts, finite-wave rate, and the first width whose first-result p95
is at least 1.25 times width 1. This configured diagnostic knee criterion is
not a shared-host performance gate or sustainable startup-capacity claim.

`provider-http` extends the existing loopback OpenAI-compatible fixture with
prescribed first-body/interchunk delays, chunk sizes, server parallelism and
one scripted 503 on each selected call before its successful SSE retry. It runs
real `ProviderHandle` reliability and parsing, explicitly permitting one bounded
retry in this fixture with no billing, against two configured user/answer
text sizes and independently scheduled arrival rates. Every finite arrival is
recorded, including generator rejection at `--max-inflight`, provider error,
actual offer/admission/completion, and scheduled-to-completion latency. Offered
rate uses the actual offer window; achieved rate uses the completion window.
The generator never waits for an earlier call before scheduling its next
arrival. The fixture uses plain loopback HTTP without a proxy, a client per
population, and `Connection: close`; it never contacts a live provider.

The reusable `lash_http_transport::observation` decorator is content-free and
opt-in. A bounded ledger belongs to one logical provider call; its HTTP ordinal
joins that call's sealed attempt ordinal and optional usage, so retry bytes and
latency stay attached to the correct attempt. Request-built means the completed
`HttpRequest` entering the transport seam, excluding earlier provider lowering.
Headers, first nonempty transport chunk, body end and offered/delivered body
bytes use monotonic samples. End distinguishes EOF, failure and early drop.
First chunk is not first visible model token, and the decorator cannot split
DNS/TLS/connect or measure kernel wire bytes. It retains no second response
body and preserves the existing body-budget checks. Usage is synthetic fixture
input/output/cache/reasoning token data; missing attempt usage remains unknown.

Both receipts define quantity, unit, process/window and statistic for their
numbers, retain raw samples, and declare `functional_noncertifying`. Small
shared-host populations establish phase/attempt/accounting completeness only;
quiet-host repeated populations are required for timing and capacity decisions.
These populations do not overlap the observation, trace-sink or workflow-overlay
instruments.
