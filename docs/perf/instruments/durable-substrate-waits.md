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
