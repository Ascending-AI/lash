# Codemode parity examples

These pairs show the same flagship host flow in both permanent RLM dialects.
They are source examples, not language tutorials: the host API and lifecycle
are the point. `docs-snippets` parses every file so examples cannot drift from
the accepted surfaces.

- `turn.lash` / `turn.ts`: inspect two host results and finish a compact value.
- `durable-process.lash` / `durable-process.ts`: define a durable process that
  suspends on a named signal, resumes through a timer, emits progress, and
  returns. The process artifact retains the source dialect across worker
  restarts.

Hosts state the pair at session creation through the plugin options seam
(`RLM_PROTOCOL_PLUGIN_ID` with `RlmCreateExtras { dialect, .. }`), applied as a
guarded set-if-unset write; absence selects Lashlang.
