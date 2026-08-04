#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"

out_dir="${1:-${LASH_FOCUSED_SQLITE_REPRO_OUT_DIR:-$repo/target/confidence/$LASH_GATE_WORKTREE_SLUG/focused-sqlite-seed-tail}}"
profile="${LASH_FOCUSED_SQLITE_REPRO_PROFILE:-full-random}"
max_boundaries="${LASH_FOCUSED_SQLITE_REPRO_MAX_BOUNDARIES:-384}"

focused_single_seed="4101155038242989457"
focused_tail_previous_seed="17785827714152183977"
absolute_fence_drift_seed="14526660659617982248"
artifact="${out_dir}/focused-sqlite-seed-tail.json"

usage() {
  cat <<'USAGE'
Usage: scripts/lash-sim-focused-sqlite-repro.sh [artifact-dir]

Runs the focused generated-simulation SQLite cross-backend repro gate:
  1. single seed 4101155038242989457
  2. two-seed tail 17785827714152183977, 4101155038242989457
  3. absolute-fence drift seed 14526660659617982248

Environment:
  LASH_FOCUSED_SQLITE_REPRO_OUT_DIR         default artifact dir
  LASH_FOCUSED_SQLITE_REPRO_PROFILE         default: full-random
  LASH_FOCUSED_SQLITE_REPRO_MAX_BOUNDARIES default: 384
USAGE
}

case "${1:-}" in
  -h|--help)
    usage
    exit 0
    ;;
esac

mkdir -p "$out_dir"

run_case() {
  local name="$1"
  shift
  local case_dir="${out_dir}/${name}"
  local log_path="${case_dir}/lash-sim-run.log"
  local status_path="${case_dir}/status.txt"
  local command_path="${case_dir}/command.txt"
  mkdir -p "$case_dir"

  local cmd=(cargo run -p lash-sim --locked -- run --out "$case_dir" --profile "$profile" --max-boundaries "$max_boundaries")
  local seed
  for seed in "$@"; do
    cmd+=(--seed "$seed")
  done

  printf '%q ' "${cmd[@]}" >"$command_path"
  printf '\n' >>"$command_path"

  set +e
  "${cmd[@]}" >"$log_path" 2>&1
  local status=$?
  set -e
  printf '%s\n' "$status" >"$status_path"
}

run_case "single-seed-4101155038242989457" "$focused_single_seed"
run_case "tail-seeds-17785827714152183977-4101155038242989457" \
  "$focused_tail_previous_seed" "$focused_single_seed"
run_case "absolute-fence-drift-seed-14526660659617982248" \
  "$absolute_fence_drift_seed"

python3 - "$out_dir" "$artifact" "$profile" "$max_boundaries" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
artifact = pathlib.Path(sys.argv[2])
profile = sys.argv[3]
max_boundaries = int(sys.argv[4])

cases = [
    (
        "single_seed",
        "single-seed-4101155038242989457",
        [4101155038242989457],
    ),
    (
        "two_seed_tail",
        "tail-seeds-17785827714152183977-4101155038242989457",
        [17785827714152183977, 4101155038242989457],
    ),
    (
        "absolute_fence_drift",
        "absolute-fence-drift-seed-14526660659617982248",
        [14526660659617982248],
    ),
]


def rel(path: pathlib.Path) -> str:
    return path.relative_to(root).as_posix()


runs = []
overall_status = "passed"
for case_name, dir_name, seeds in cases:
    case_dir = root / dir_name
    status_path = case_dir / "status.txt"
    exit_code = int(status_path.read_text(encoding="utf-8").strip())
    status = "passed" if exit_code == 0 else "failed"
    if status != "passed":
        overall_status = "failed"

    summary_path = case_dir / "summary.json"
    counts = None
    if summary_path.is_file():
        with summary_path.open("r", encoding="utf-8") as handle:
            counts = (json.load(handle).get("counts") or {})

    sqlite_reports = sorted(case_dir.glob("replays/*.sqlite-replay.json"))
    divergent_reports = []
    for report_path in sqlite_reports:
        try:
            with report_path.open("r", encoding="utf-8") as handle:
                report = json.load(handle)
        except json.JSONDecodeError:
            continue
        if report.get("matches_reference") is False:
            divergent_reports.append(rel(report_path))

    runs.append(
        {
            "name": case_name,
            "status": status,
            "exit_code": exit_code,
            "profile": profile,
            "max_boundaries": max_boundaries,
            "seeds": seeds,
            "command": (case_dir / "command.txt").read_text(encoding="utf-8").strip(),
            "log_path": rel(case_dir / "lash-sim-run.log"),
            "summary_path": rel(summary_path) if summary_path.is_file() else "not_written",
            "sqlite_replay_reports": [rel(path) for path in sqlite_reports],
            "sqlite_divergence_reports": divergent_reports,
            "counts": counts,
        }
    )

payload = {
    "schema": "lash.confidence.focused-sqlite-seed-tail-repro.v1",
    "status": overall_status,
    "purpose": "focused generated full-random SQLite cross-backend repro for the seed-tail that previously exposed a deterministic backend divergence",
    "profile": profile,
    "max_boundaries": max_boundaries,
    "required_cases": [
        "single_seed_4101155038242989457",
        "tail_17785827714152183977_then_4101155038242989457",
        "absolute_fence_drift_seed_14526660659617982248",
    ],
    "runs": runs,
}

artifact.parent.mkdir(parents=True, exist_ok=True)
with artifact.open("w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2, sort_keys=True)
    handle.write("\n")

if overall_status != "passed":
    for run in runs:
        if run["status"] != "passed":
            print(
                f"focused SQLite repro case {run['name']} failed; log={root / run['log_path']}",
                file=sys.stderr,
            )
            for report in run["sqlite_divergence_reports"]:
                print(f"sqlite divergence report={root / report}", file=sys.stderr)
    sys.exit(1)
PY

printf 'Focused SQLite seed-tail repro artifact: %s\n' "$artifact"
