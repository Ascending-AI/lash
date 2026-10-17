# Direct durable-substrate process waits

```sh
kiln run //crates/lash-perf:durable-substrate__bin -- \
  --store sqlite --case process-waits-10 --samples 1 \
  --sqlite-dir "$PWD/.benchmarks/process-waits-10" \
  --out "$E/process-waits-10.jsonl"
```

Use a fresh SQLite directory. Deployment publishes the content-addressed
process-execution environment before wrapping stores for recording or starting
measurement. The receipt therefore measures real process registration and ten
waits rather than refusing on a missing environment artifact. The same setup
serves the parked-process and idle-process populations; this reduced command
exercises only `process-waits-10`.


For the declared-read producer and its text reader, use a fresh directory:

```sh
kiln run //crates/lash-perf:durable-substrate__bin -- \
  --store sqlite --case declared-reads --samples 1 \
  --read-population typescript-read-32-false \
  --vm-worker "$LASH_VM_WORKER" --sqlite-dir "$E/read-stores" --out "$E/reads.jsonl"
python3 crates/lash-perf/src/bin/durable_substrate/reads_report.py --inputs "$E/reads.jsonl"
```

Only samples whose output, dependent read/write chain and accepted commit/fact
geometry passed validation enter the timing table. Metadata/settings reads and
validation are outside send-to-outcome timing. Each sample and distribution
retains its producer identity. The reader prints separate full identities and
tables for source, build, host, worker, backend policy, observer and window
cohorts, and reports excluded samples. Old rows without successful validation
or complete identity cannot qualify a timing claim.

The offline admission control prints `read-admission.json`:

```sh
python3 -m unittest \
  crates.lash-perf.src.bin.durable_substrate.reads_report.AdmissionTests.test_source_build_host_and_observer_cohorts_never_pool
kiln test //crates/lash-perf:durable-substrate__bin__unit_test \
  --test_arg=heap::tests::backend_availability_preserves_unknown_and_measured_zero --test_arg=--exact --test_arg=--nocapture
```

The heap control prints `backend-availability.json`. SQLite-only statement,
written-row, expanded-literal-byte and publication VM-step counters are null
on PostgreSQL and `unknown` in the text table. Their growth exponents stay
unavailable. Actual zero measurements remain zero, and common page and accepted
publication statement/returned-row counters remain available on both backends.
These are backend availability controls, not PostgreSQL service certification.
