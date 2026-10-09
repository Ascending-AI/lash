# VM worker matrix

The optimized `//crates/lash-perf:vm-worker-matrix__bin` certifies by default.
It writes `budgets.json` and the exchange report before refusing any selected
failed verdict: exchange nearest-rank p50 / p99 above configured 100 / 500 us
per leaf, zero-effect paired overhead above configured 1 / 5 ms per case, or
phase reconciliation errors above the configured 1 us tolerance. Exchange
windows cover worker request serialization through the next request's arrival;
zero-effect overhead is the paired worker-minus-reference case interval.
`--exchanges N --out DIRECTORY` selects exchanges only; a full measurement also
selects zero-effect overhead. `--report-only` labels output and receipts as
noncertifying and retains samples for shared-host diagnosis. These runs do not
establish quiet-host baselines.
