#!/usr/bin/env bash
# Run the FIG-3398 pre-cutover tool-batch baseline and archive the evidence.
#
# FIG-3397 replaces the tool batch with a first-class effect group; before that
# cutover lands, someone has to write down what the batch costs today so the
# after-number has something honest to be compared to. One run of this script
# produces that record: for every (backend, producer, width) cell it drives the
# conformance producers' batch through `crates/lash-perf`'s
# `tool_batch_baseline` bin `reps` times and writes one JSONL row per rep —
# turn wall time, leaf window, peak in-flight concurrency, journal rows, and
# the load the rep ran under.
#
# The three legs:
#
#   * `sqlite`   — runs in-process on a throwaway database file.
#   * `postgres` — wrapped in `scripts/ci/with-service.sh pg16`, so the lane
#                  measures the same image and credentials CI measures.
#   * `restate`  — starts `restatedev/restate` in a throwaway container on
#                  ephemeral loopback ports, serves the probe endpoint from
#                  the bin, registers the deployment, and counts the
#                  invocation's `sys_journal` rows. Restate is serial today —
#                  `supports_concurrent_effects()` is hardcoded false — so the
#                  leg records a serial baseline, not a defect.
#
# Evidence goes under `<archive-root>/<short-sha>/`: one JSONL per backend, a
# `MANIFEST.md` naming the commit, tree state, quiet-box verdict and exact
# commands, and a `SUMMARY.md` of medians and spreads. A number without its
# load figure is not evidence, so the run refuses to start on a contended box
# unless `--allow-busy` is passed — in which case the violation is recorded in
# the manifest the way `scripts/perf_baseline.py` does.
#
# Usage:
#   scripts/tool-batch-baseline.sh --archive-root <dir> [options]
#
# Options:
#   --archive-root DIR    evidence root (or LASH_PERF_BASELINE_ROOT); required
#   --backends LIST       comma list of sqlite,postgres,restate (default: all)
#   --widths LIST         batch widths (default: 2,8,50)
#   --reps N              repetitions per cell (default: 5)
#   --producers LIST      comma list of standard,rlm (default: both)
#   --debug               measure a debug build instead of --release
#   --no-build            skip the build; use the existing binary
#   --max-load F          refuse above this one-minute load (default: 8.0)
#   --allow-busy          record quiet-box violations instead of refusing
set -euo pipefail

readonly PROGRAM="scripts/tool-batch-baseline.sh"
readonly RESTATE_IMAGE="${TOOL_BATCH_RESTATE_IMAGE:-restatedev/restate:1.7.0}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

note() {
  echo "${PROGRAM}: $*" >&2
}

die() {
  note "error: $*"
  exit 2
}

archive_root="${LASH_PERF_BASELINE_ROOT:-}"
backends="sqlite,postgres,restate"
widths="2,8,50"
reps=5
producers="standard,rlm"
profile=release
build=1
max_load=8.0
allow_busy=0

while (($#)); do
  case "$1" in
    --archive-root) archive_root="$2"; shift 2 ;;
    --backends) backends="$2"; shift 2 ;;
    --widths) widths="$2"; shift 2 ;;
    --reps) reps="$2"; shift 2 ;;
    --producers) producers="$2"; shift 2 ;;
    --debug) profile=debug; shift ;;
    --no-build) build=0; shift ;;
    --max-load) max_load="$2"; shift 2 ;;
    --allow-busy) allow_busy=1; shift ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) die "unknown argument $1" ;;
  esac
done

[[ -n "$archive_root" ]] || die "--archive-root or LASH_PERF_BASELINE_ROOT is required"
cd "$repo_root"

# --- quiet box --------------------------------------------------------------
# Same precondition as perf_baseline.py: wall clock on a contended box is a
# load measurement, not a latency measurement. Reads /proc/comm directly so the
# check can never match its own command line.
quiet_violations=()
load1="$(cut -d' ' -f1 /proc/loadavg)"
if awk -v a="$load1" -v b="$max_load" 'BEGIN{exit !(a > b)}'; then
  quiet_violations+=("one-minute load ${load1} exceeds the ${max_load} limit")
fi
for comm in rustc cargo bazel lash-perf; do
  count=0
  for entry in /proc/[0-9]*/comm; do
    [[ "$(cat "$entry" 2>/dev/null)" == "$comm" ]] && ((count++)) || true
  done
  ((count == 0)) || quiet_violations+=("${count} \`${comm}\` process(es) are running")
done
if ((${#quiet_violations[@]})) && ((allow_busy == 0)); then
  printf '%s\n' "${quiet_violations[@]}" | sed 's/^/error: /' >&2
  die "refusing to measure on a busy box; pass --allow-busy to record the violation"
fi

# --- build ------------------------------------------------------------------
target_dir="${CARGO_TARGET_DIR:-$repo_root/target}"
bin="$target_dir/$profile/tool_batch_baseline"
if ((build)); then
  build_args=(cargo build -q -p lash-perf --bin tool_batch_baseline --locked)
  [[ "$profile" == release ]] && build_args+=(--release)
  note "building: ${build_args[*]}"
  "${build_args[@]}"
fi
[[ -x "$bin" ]] || die "no binary at $bin"

# --- archive ----------------------------------------------------------------
sha="$(git rev-parse HEAD)"
short="${sha:0:9}"
dirty="$(git status --porcelain)"
destination="$archive_root/$short"
mkdir -p "$destination"
scratch="$(mktemp -d)"
containers=()

cleanup() {
  for c in "${containers[@]+"${containers[@]}"}"; do
    docker rm -f "$c" >/dev/null 2>&1 || true
  done
  rm -rf "$scratch"
}
trap cleanup EXIT

free_port() {
  python3 -c 'import socket
probe = socket.socket()
probe.bind(("127.0.0.1", 0))
print(probe.getsockname()[1])
probe.close()'
}

commands=()

run_sqlite() {
  local out="$destination/sqlite.jsonl"
  commands+=("$bin --backend sqlite --widths $widths --reps $reps --producers $producers --out $out --db-path <scratch>/effect.db")
  "$bin" --backend sqlite --widths "$widths" --reps "$reps" \
    --producers "$producers" --out "$out" --db-path "$scratch/effect.db"
}

run_postgres() {
  local out="$destination/postgres.jsonl"
  commands+=("scripts/ci/with-service.sh pg16 -- $bin --backend postgres --widths $widths --reps $reps --producers $producers --out $out")
  bash scripts/ci/with-service.sh pg16 -- \
    "$bin" --backend postgres --widths "$widths" --reps "$reps" \
    --producers "$producers" --out "$out"
}

wait_for_port() {
  local port="$1" what="$2" deadline=$((SECONDS + 60))
  until (echo >"/dev/tcp/127.0.0.1/$port") >/dev/null 2>&1; do
    ((SECONDS < deadline)) || die "$what did not open on port $port"
    sleep 1
  done
}

run_restate() {
  command -v docker >/dev/null || die "the restate leg needs docker"
  local admin_port ingress_port node_port endpoint_port container out
  admin_port="$(free_port)"
  ingress_port="$(free_port)"
  node_port="$(free_port)"
  endpoint_port="$(free_port)"
  container="tool-batch-baseline-restate-$$-${RANDOM}"
  out="$destination/restate.jsonl"

  bash scripts/docker-pull-with-retry.sh "$RESTATE_IMAGE" >/dev/null
  # Host networking, same as justfile's restate recipes: the bin's probe
  # endpoint and the container share loopback, so the endpoint URL the admin
  # API registers is the port the bin binds.
  docker run -d --name "$container" --network host \
    -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
    -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
    -e RESTATE_BIND_PORT="$node_port" \
    "$RESTATE_IMAGE" >/dev/null
  containers+=("$container")
  wait_for_port "$admin_port" "Restate admin"
  wait_for_port "$ingress_port" "Restate ingress"
  note "restate: $RESTATE_IMAGE admin=$admin_port ingress=$ingress_port endpoint=$endpoint_port"

  commands+=("RESTATE_INGRESS_URL=http://127.0.0.1:$ingress_port RESTATE_ADMIN_URL=http://127.0.0.1:$admin_port EG_RESTATE_ENDPOINT_BIND=127.0.0.1:$endpoint_port EG_RESTATE_ENDPOINT_URL=http://127.0.0.1:$endpoint_port $bin --backend restate --widths $widths --reps $reps --producers $producers --out $out")
  RESTATE_INGRESS_URL="http://127.0.0.1:$ingress_port" \
  RESTATE_ADMIN_URL="http://127.0.0.1:$admin_port" \
  EG_RESTATE_ENDPOINT_BIND="127.0.0.1:$endpoint_port" \
  EG_RESTATE_ENDPOINT_URL="http://127.0.0.1:$endpoint_port" \
    "$bin" --backend restate --widths "$widths" --reps "$reps" \
      --producers "$producers" --out "$out"
}

before_uptime="$(uptime)"
started=$SECONDS
IFS=',' read -ra backend_list <<<"$backends"
for backend in "${backend_list[@]}"; do
  case "$backend" in
    sqlite) run_sqlite ;;
    postgres) run_postgres ;;
    restate) run_restate ;;
    *) die "unknown backend $backend" ;;
  esac
done
measure_seconds=$((SECONDS - started))
after_uptime="$(uptime)"

# --- summary ----------------------------------------------------------------
python3 - "$destination" <<'PY'
import json
import statistics
import sys
from pathlib import Path

dest = Path(sys.argv[1])
rows = []
for ledger in sorted(dest.glob("*.jsonl")):
    for line in ledger.read_text().splitlines():
        if line.strip():
            rows.append(json.loads(line))

cells = {}
for row in rows:
    key = (row["backend"], row["producer"], row["width"])
    cells.setdefault(key, []).append(row)

lines = [
    "# Tool-batch baseline summary",
    "",
    "| backend | producer | width | reps | turn ms median [min–max] | leaf window ms median | peak in-flight | journal rows | load1 range |",
    "|---|---|---|---|---|---|---|---|---|",
]
for (backend, producer, width), group in sorted(cells.items()):
    turns = [r["turn_ms"] for r in group]
    windows = [r["leaf_window_ms"] for r in group if r["leaf_window_ms"] is not None]
    peaks = max(r["peak_in_flight"] for r in group)
    loads = [r["load1"] for r in group]
    journal = "; ".join(
        f"{k}={sorted({r['journal_rows'].get(k, 0) for r in group})}"
        for k in sorted({k for r in group for k in r["journal_rows"]})
    )
    window = f"{statistics.median(windows):.1f}" if windows else "n/a"
    lines.append(
        f"| {backend} | {producer} | {width} | {len(group)} "
        f"| {statistics.median(turns):.1f} [{min(turns):.1f}–{max(turns):.1f}] "
        f"| {window} | {peaks} | {journal} | {min(loads):.2f}–{max(loads):.2f} |"
    )
lines.append("")
(dest / "SUMMARY.md").write_text("\n".join(lines))
PY

# --- manifest ---------------------------------------------------------------
{
  echo "# Tool-batch baseline manifest"
  echo
  echo "- SHA: \`$sha\`"
  echo "- Date: \`$(date -u '+%Y-%m-%d %H:%M:%SZ')\`"
  echo "- Host: \`$(hostname)\` ($(nproc) CPUs)"
  echo "- Working tree: $(if [[ -z "$dirty" ]]; then echo clean; else echo 'DIRTY — the ledger is not attributable to the SHA alone'; fi)"
  if ((${#quiet_violations[@]})); then
    echo "- Quiet-box precondition: NOT met (measured under --allow-busy)"
    printf '  - %s\n' "${quiet_violations[@]}"
  else
    echo "- Quiet-box precondition: met (load1 $load1 <= $max_load, no build processes)"
  fi
  echo "- Pre-run uptime: \`$before_uptime\`"
  echo "- Post-run uptime: \`$after_uptime\`"
  echo "- Measurement wall time: ${measure_seconds} s"
  echo "- Restate leg: serial today (\`supports_concurrent_effects()\` hardcoded false); the restate numbers are a serial baseline, not a defect"
  echo
  echo "## Commands"
  echo
  echo '```text'
  printf '%s\n' "${commands[@]}"
  echo '```'
  if [[ -n "$dirty" ]]; then
    echo
    echo "## Uncommitted paths at measurement time"
    echo
    echo '```text'
    echo "$dirty"
    echo '```'
  fi
} >"$destination/MANIFEST.md"

note "archived $short to $destination"
