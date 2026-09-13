#!/usr/bin/env bash
# Records the argument vector of every `cargo` invocation made by
# scripts/confidence-gate.sh instead of executing it, so the routing probes in
# crates/lash-core/src/runtime/tests/runtime_scenarios/fault_matrix.rs can
# assert which commands the gate would run. Checked in rather than written at
# runtime so it travels as a declared test input (Bazel runfiles / Cargo
# package files) rather than being materialised from a string literal.
if [ "${1:-}" = "nextest" ] && [ "${2:-}" = "--version" ]; then
  exit 1
fi
{
  printf 'BEGIN\n'
  for arg in "$@"; do
    printf '%s\n' "$arg"
  done
  printf 'END\n'
} >> "$LASH_FAKE_CARGO_LOG"
