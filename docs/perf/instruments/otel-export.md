# OTel export from an embedding host

Lash borrows the host's tracer and meter providers through
`lash::tracing::OtelTelemetry::new`. The host owns their resources, readers,
exporters, sampling, aggregation views, flush and shutdown. Lash installs no
process-global provider and starts no collector. These settings apply to the
host's production-observability mechanisms; the laws here use hermetic exporters.

## Release identity on both signals

Give both providers the same resource, with the host's `service.name` and
`service.version`. For OTel SDK 0.33.0:

```rust
use lash::tracing::{otel::KeyValue, OtelOptions, OtelTelemetry};
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};

let resource = Resource::builder_empty()
    .with_attributes([
        KeyValue::new("service.name", "my-host"),
        KeyValue::new("service.version", "2026.10.09"),
    ])
    .build();
// Add the host's span exporter and metric reader before building providers.
let tracer = SdkTracerProvider::builder().with_resource(resource.clone()).build();
let meter = SdkMeterProvider::builder().with_resource(resource).build();
let telemetry = OtelTelemetry::new(&tracer, &meter, OtelOptions::standard());
```

Every adapter span and metric stream uses scope `lash`. Its attributes contain
`lash.version`, the `lash-internal-trace` package version supplied by the build.
They also contain `lash.build.revision` when the compiler receives a nonempty,
deterministic `LASH_BUILD_REVISION`. The current hermetic build supplies no
source revision, so that attribute is absent. No build-time Git invocation,
wall clock, runtime environment lookup or generated timestamp supplies identity.

The scope's version remains `LASH_INSTRUMENTATION_CONTRACT` (`1.0`): it identifies
the instrumentation schema, not a release or source revision. Host and library
identities stay separate. `TelemetryMetrics::from_provider` uses the same scope.
A host using `TelemetryMetrics::new(meter)` directly owns that meter's scope too.

Resource and scope attributes accompany metric points through their enclosing
resource/scope records; they are not copied into every point's attribute set.
Exporters must retain both levels, including OTLP scope attributes. Group release
comparisons by `service.version`, `lash.version`, and `lash.build.revision` when
present, using the same labels for traces and metrics. The development package
version `0.0.0-dev` alone cannot distinguish source revisions; retain the host's
release identity rather than treating the contract version as a build version.

## Latency tails and host aggregation settings

Lash's default latency histogram hints use these explicit bounds in milliseconds:

```text
0, 1, 2, 5, 10, 25, 50, 75, 100, 250, 500, 750,
1000, 2500, 5000, 10000, 15000, 30000, 60000
```

The five instruments are `lash.provider.throttle_wait.duration` and
`lash.store.pool.acquire_wait.duration` (milliseconds), and
`lash.durable.commit.acquire_wait.duration`,
`lash.durable.commit.transaction.duration`, and
`lash.durable.commit.lock_statement_elapsed` (microseconds). Microsecond bounds
are multiplied by 1000, so every instrument separates 5 seconds from 30 seconds
and has a finite bucket through 60 seconds. Larger samples enter the overflow
bucket. These are distribution boundaries, not timeout settings; measurements
keep their existing physical or live ownership.

The host's MeterProvider views take precedence over instrument hints. The
`lash::tracing::recommended_latency_histogram_boundaries(name)` helper returns
bounds in the named latency instrument's declared unit, or `None` for other
instruments. Use it in a recommended view, or substitute the host's chosen
bounds or base-2 exponential aggregation. For example:

```rust
use lash::tracing::recommended_latency_histogram_boundaries;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, SdkMeterProvider, Stream};

let recommended_view = |instrument: &Instrument| {
    if instrument.scope().name() != "lash" {
        return None;
    }
    let boundaries = recommended_latency_histogram_boundaries(instrument.name())?;
    Some(Stream::builder()
        .with_aggregation(Aggregation::ExplicitBucketHistogram {
            boundaries,
            record_min_max: true,
        })
        .build()
        .expect("valid recommended latency view"))
};
let meter = SdkMeterProvider::builder().with_view(recommended_view);
// Set the shared host resource and export reader, then build this provider.
```

Views are host settings configured before instruments are created. More buckets
cost memory and export bytes. Tail sampling is a collector policy: preserving a
metric histogram tail does not retain the corresponding traces. Configure trace
sampling and collector tail sampling separately. Provider configuration is
covered by the [OTel SDK metrics documentation](https://docs.rs/opentelemetry_sdk/0.33.0/opentelemetry_sdk/metrics/index.html).

## Exemplars wait for SDK reservoir support

The pinned `opentelemetry_sdk` 0.33.0 defines exemplar data types but has no
reservoir implementation. In that crate's `src/metrics/internal/histogram.rs`,
lines 190 and 244 assign `exemplars: vec![]` for delta and cumulative export;
`src/metrics/internal/exponential_histogram.rs` lines 481 and 541 do the same.
Consequently, recording a latency measurement inside a sampled OTel span still
exports no exemplar. Changing histogram views or setting an exemplar environment
variable cannot add this missing SDK machinery.

Exemplar-to-trace linking waits for a supported SDK with a reservoir. Lash does
not implement one, fabricate exemplars, or add trace IDs as metric labels. Once
that SDK supports it, prove an exported latency exemplar carries the recording
span's trace and span IDs, and configure its filter and reservoir through the
host provider. That positive law is deliberately absent on 0.33.0.

## Attach a host profiler

A host can build its embedding executable on `//tools/buck2:profiling`, with
`-c kiln.rust_profile=optimized`. That platform already retains symbols and
line tables and enables frame pointers for first-party and third-party Rust.
The normal release profile strips symbols; do not assume it has the profiling
platform's properties. Keep the embedding executable and matching debug artifacts
available to the profiler's symbolizer.

On a Linux host, attach a continuous profiler externally to the process embedding
Lash. With Alloy, select its PID with the `__process_pid__` label in
`pyroscope.ebpf.targets`, set its
`service_name` to the OTel service name, and forward the profiles to a
`pyroscope.write` receiver for the host's Pyroscope endpoint. Give the profiler
access to the host PID namespace, executable symbols and required kernel
permissions. Process discovery, collector endpoint, credentials and sampling
frequency are host settings; Lash needs no profiler agent in its runtime.
See [Alloy's eBPF component configuration](https://grafana.com/docs/alloy/latest/reference/components/pyroscope/pyroscope.ebpf/).

Lash also gives the host retained W3C trace/span context and an OTel adapter for
correlation with traces. A profiler integration must bridge that context using
its supported mechanism; an external eBPF stack sample does not automatically
read Rust's OTel Context. Use matching service/version labels and time windows
for process-level correlation. CPU stacks identify the thread executing when
sampled. They cannot assign a whole request's CPU time to multiplexed futures,
and they do not measure a future's waiting time or provider latency. Lash makes
no per-request profiling attribution claim.

## Hermetic proof

The two `otel::tests` laws in `crates/lash-trace/src/otel/tests.rs` export to memory:
`host_and_lash_versions_accompany_exported_spans_and_metric_points` proves shared
host resource identity and library scope identity; and
`latency_histograms_distinguish_thirty_seconds_from_five_seconds` proves each
latency instrument's tail and the host view override. They require no deployment
or live collector and make no claim about one.
