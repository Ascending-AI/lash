#!/usr/bin/env bash
set -euo pipefail

# Deterministic companion for runbooks/version-bump-recreation. It seeds a
# PostgreSQL deployment that an older lash owned, proves the exact-match schema
# gate refuses it in both directions, performs the recreation bump, and proves
# the three durable surfaces work on the recreated store.

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

# The harness fixtures are pinned to the newest component generation. Prove they
# still derive from SCHEMA_MIGRATIONS before spending a container on a run whose
# refusals would be about the wrong generation.
python3 "$repo/scripts/check_version_bump_fixtures.py"

# shellcheck source=scripts/worktree-gate-env.sh
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire version-bump-recreation-e2e

postgres_port="${LASH_VERSION_BUMP_POSTGRES_PORT:-$((LASH_E2E_PORT_BASE + 47))}"
export LASH_VERSION_BUMP_POSTGRES_PORT="$postgres_port"
compose_project="${LASH_VERSION_BUMP_COMPOSE_PROJECT:-lash-version-bump-${LASH_GATE_WORKTREE_SLUG}}"
compose=(docker compose -p "$compose_project" -f "$repo/runbooks/version-bump-recreation/docker-compose.yml")
if [ -n "${LASH_VERSION_BUMP_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$LASH_VERSION_BUMP_ARTIFACT_DIR"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-version-bump-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
mkdir -p "$artifact_dir"
test_output="$artifact_dir/version-bump-recreation-e2e.log"

cleanup() {
  status=$?
  "${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
  lash_gate_cleanup
  if [ "$status" -ne 0 ]; then
    echo "version-bump recreation E2E failed with status $status; artifacts: $artifact_dir" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

database_url="postgres://lash:lash@127.0.0.1:${postgres_port}/lash"
export DATABASE_URL="$database_url"

harness() {
  cargo run --locked --quiet -p lash-restate-postgres-workers-e2e \
    --bin lash-e2e-version-bump -- "$1"
}

"${compose[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
"${compose[@]}" up -d postgres

deadline=$((SECONDS + 90))
until docker run --rm --name "lash-version-bump-postgres-probe-${LASH_GATE_WORKTREE_SLUG}-$$" \
  --label "$LASH_GATE_LABEL" --network host postgres:16-alpine \
  pg_isready -h 127.0.0.1 -p "$postgres_port" -U lash -d lash >/dev/null 2>&1; do
  if ((SECONDS >= deadline)); then
    "${compose[@]}" logs postgres >&2 || true
    echo "PostgreSQL did not become ready on port $postgres_port" >&2
    exit 1
  fi
  sleep 1
done

"${compose[@]}" ps --format json >"$artifact_dir/00-live-services.json"
docker ps --filter "publish=$postgres_port" --format json >"$artifact_dir/00-postgres-service.json"
if [ ! -s "$artifact_dir/00-postgres-service.json" ]; then
  echo "No running container publishes the assigned PostgreSQL port $postgres_port" >&2
  exit 1
fi
docker run --rm --name "lash-version-bump-postgres-query-${LASH_GATE_WORKTREE_SLUG}-$$" \
  --label "$LASH_GATE_LABEL" --network host -e PGPASSWORD=lash postgres:16-alpine \
  psql -h 127.0.0.1 -p "$postgres_port" -U lash -d lash -Atqc \
  "SELECT json_build_object('postgres_version', current_setting('server_version'), 'port', ${postgres_port})" \
  >"$artifact_dir/00-postgres.json"
echo "scenario 0 evidence: PostgreSQL:${postgres_port} is live and empty" | tee "$test_output"

harness seed 2>&1 | tee "$artifact_dir/01-seed.jsonl" | tee -a "$test_output"
harness refuse 2>&1 | tee "$artifact_dir/02-refusal.jsonl" | tee -a "$test_output"
harness recreate 2>&1 | tee "$artifact_dir/03-recreation.jsonl" | tee -a "$test_output"
harness health 2>&1 | tee "$artifact_dir/04-health.jsonl" | tee -a "$test_output"

python3 - "$artifact_dir" <<'PY'
import json
import sys
from pathlib import Path

artifacts = Path(sys.argv[1])


def checkpoint(name, filename):
    path = artifacts / filename
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if value.get("checkpoint") == name:
            return value
    raise SystemExit(f"missing {name!r} checkpoint in {path}")


def fail(message):
    raise SystemExit(f"version-bump recreation gate failed: {message}")


# Each phase proves one gate, and the refusal kinds are not interchangeable: a
# non-empty refusal is not evidence for the claim its phase makes. The harness
# classifies every refusal by the prose only that error carries and refuses any
# other kind; these expectations keep that visible in the artifacts.
# scripts/check_version_bump_fixtures.py holds these strings to the harness's
# own `RefusalKind::as_str` literals, so a rename cannot go unnoticed until a
# container gate runs.
EXPECTED_REFUSAL_KINDS = {
    "refused_divergent_store": "divergent_artifacts",
    "refused_older_store": "no_applicable_migration",
    "refused_newer_store": "no_applicable_migration",
    "recreated_store": "no_applicable_migration",
}

seeded = checkpoint("seeded_older_deployment", "01-seed.jsonl")
if seeded["recorded_version"] != seeded["expected_version"] - 1:
    fail(f"seed did not record the previous component version: {seeded}")
for field in ("session_ids", "process_ids", "trigger_process_ids"):
    if not seeded[field]:
        fail(f"seed left {field} empty: {seeded}")
if seeded["committed_sessions"] != len(seeded["session_ids"]):
    fail(f"seed did not commit a turn on every session: {seeded}")
if seeded["trigger_reservations"] != 1:
    fail(f"the seeded trigger did not reserve exactly one delivery: {seeded}")

# The store-readability probe (FIG-1556 pieces 3 and 4) answers "will this
# durable data open under this build?" without opening the store. These
# assertions hold it to the refusal the store itself then delivered: a probe
# that agreed with nothing, or that refused everything, would be worthless as a
# deploy gate, so the run asserts both directions against controlled fixtures.
PROBE_FIELDS = ("backend", "mode", "outcome", "schema", "components", "drain", "not_scanned")


def probe_of(value, key="probe"):
    report = value.get(key)
    if not isinstance(report, dict):
        fail(f"checkpoint {value.get('checkpoint')!r} carried no {key!r} report")
    for field in PROBE_FIELDS:
        if field not in report:
            fail(f"probe report is missing {field!r}: {sorted(report)}")
    # A report that does not name its own blind spots reads as complete when it
    # is not; the enumerated formats with no bounded surface are always listed.
    if not report["not_scanned"]:
        fail(f"a probe report named nothing it had not scanned: {report}")
    if not report["components"]:
        fail(f"a probe report enumerated no durable formats: {report}")
    return report


def probe_rows(report):
    return {row["format"]: row for row in report["components"]}


def probe_stamp(report):
    """The one schema database whose recorded version the fixture moved."""
    # `found` is omitted, not null, when the version was never read, so ask for
    # it rather than indexing: a matching database would KeyError otherwise.
    stamped = [db for db in report["schema"]["databases"] if db.get("found") is not None]
    if len(stamped) != 1:
        fail(f"expected exactly one mismatched schema database: {report['schema']}")
    return stamped[0]


before = probe_of(seeded, "probe_before_rewind")
after = probe_of(seeded, "probe_after_rewind")
for report in (before, after):
    if report["mode"] != "deep":
        fail(f"the pre-bump audit did not run the deep walk: {report['mode']}")
if before["outcome"] != "ready":
    fail(f"the probe refused a store this build had just written: {before}")
if before["drain"]:
    fail(f"a store this build wrote produced drain blockers: {before['drain']}")
rows = probe_rows(before)
undecodable = sorted(name for name, row in rows.items() if row["verdict"] == "undecodable")
if undecodable:
    fail(f"the deep walk could not decode freshly written data: {undecodable}")
manifest = rows.get("session checkpoint manifest")
if manifest is None or manifest["verdict"] != "all_readable":
    fail(f"the deep walk did not read the seeded session checkpoints: {manifest}")
if manifest["scanned"] != seeded["committed_sessions"]:
    fail(f"the deep walk read {manifest['scanned']} of {seeded['committed_sessions']} sessions")
# Only the schema stamp moved between these two reports. The outcome flips and
# the drain list does not, which is exactly the distinction piece 4 exists to
# draw: this deployment cannot open, but it holds nothing that cannot be carried.
if after["outcome"] != "refused" or after["schema"]["outcome"] != "refused":
    fail(f"the probe did not refuse the rewound ledger: {after}")
if after["drain"]:
    fail(f"moving only the schema stamp invented drain blockers: {after['drain']}")
stamp = probe_stamp(after)
if stamp["found"] != seeded["recorded_version"] or stamp["expected"] != seeded["expected_version"]:
    fail(f"the probe misreported the rewound schema versions: {stamp}")

divergent = checkpoint("refused_divergent_store", "02-refusal.jsonl")
stale = checkpoint("refused_older_store", "02-refusal.jsonl")
future = checkpoint("refused_newer_store", "02-refusal.jsonl")
for refusal in (divergent, stale, future):
    found = refusal["found_version"]
    expected = refusal["expected_version"]
    message = refusal["error"]
    if f"version {found}" not in message or f"expected {expected}" not in message:
        fail(f"refusal did not name both versions: {refusal}")
    if refusal["opened"]:
        fail(f"a mismatched store was opened: {refusal}")
if divergent["found_version"] != divergent["expected_version"] - 1:
    fail(f"divergent-store refusal was not the migration-source version: {divergent}")
for refusal, checkpoint_name in (
    (divergent, "refused_divergent_store"),
    (stale, "refused_older_store"),
    (future, "refused_newer_store"),
):
    expected_kind = EXPECTED_REFUSAL_KINDS[checkpoint_name]
    if refusal.get("refusal_kind") != expected_kind:
        fail(f"refusal was not the {expected_kind!r} kind its phase proves: {refusal}")
# The divergence refusal must enumerate the artifacts the newest generation
# introduced, which is what tells an operator what to inspect. The list is
# generation-pinned in the harness (`DIVERGENT_ARTIFACTS`) and derived from
# SCHEMA_MIGRATIONS by scripts/check_version_bump_fixtures.py.
if not divergent["divergent_artifacts"]:
    fail(f"divergence refusal named no newer artifacts: {divergent}")
for artifact in divergent["divergent_artifacts"]:
    if artifact not in divergent["error"]:
        fail(f"divergence refusal omitted {artifact!r}: {divergent}")
if "inspect and recreate" not in divergent["error"]:
    fail(f"divergence refusal omitted its remedy: {divergent}")
if stale["found_version"] >= divergent["found_version"]:
    fail(f"older-store refusal was not older: {stale}")
if stale["current_artifact_count"] != 0:
    fail(f"older-store fixture retained current-only schema artifacts: {stale}")
if future["found_version"] <= future["expected_version"]:
    fail(f"newer-store refusal was not newer: {future}")

# Every refusal the store delivers, the probe predicted first — in summary mode,
# the shape a host runs at boot, which skips the per-session walk and says so.
for refusal, checkpoint_name in (
    (divergent, "refused_divergent_store"),
    (stale, "refused_older_store"),
    (future, "refused_newer_store"),
):
    report = probe_of(refusal)
    if report["mode"] != "summary":
        fail(f"{checkpoint_name}: the boot-shaped probe ran the deep walk: {report['mode']}")
    if report["outcome"] != "refused":
        fail(f"{checkpoint_name}: the probe did not predict the refusal: {report['outcome']}")
    stamp = probe_stamp(report)
    if stamp["found"] != refusal["found_version"] or stamp["expected"] != refusal["expected_version"]:
        fail(f"{checkpoint_name}: probe and refusal disagree about the versions: {stamp}")
    if not any(entry["what"] == "session checkpoints" for entry in report["not_scanned"]):
        fail(f"{checkpoint_name}: summary mode did not name the walk it skipped: {report['not_scanned']}")
    if probe_rows(report)["session checkpoint manifest"]["verdict"] != "not_scanned":
        fail(f"{checkpoint_name}: a skipped surface was reported as empty rather than unscanned")

recreated = checkpoint("recreated_store", "03-recreation.jsonl")
if recreated["premise_refusal_kind"] != EXPECTED_REFUSAL_KINDS["recreated_store"]:
    fail(f"recreation ran from the wrong premise refusal: {recreated}")
if recreated["recorded_version"] != recreated["expected_version"]:
    fail(f"recreated store is not at the expected version: {recreated}")
if recreated["surviving_seeded_rows"] != 0:
    fail(f"recreation preserved pre-bump rows: {recreated}")
if recreated["surviving_seeded_graph_nodes"] != 0:
    fail(f"pre-bump committed session content survived recreation: {recreated}")
if recreated["pre_bump_graph_nodes"] < 1:
    fail(f"the pre-bump store held no committed session content: {recreated}")

# The other direction, on the store recreation just produced: without this, every
# refusal above would be consistent with a probe that refuses everything.
recreated_probe = probe_of(recreated)
if recreated_probe["outcome"] != "ready" or recreated_probe["schema"]["outcome"] != "ready":
    fail(f"the probe refused the store recreation just produced: {recreated_probe}")
if recreated_probe["drain"]:
    fail(f"a freshly recreated store produced drain blockers: {recreated_probe['drain']}")
if any(db["verdict"] != "matches" for db in recreated_probe["schema"]["databases"]):
    fail(f"the recreated store's schema did not read as matching: {recreated_probe['schema']}")

health = checkpoint("verified_recreated_deployment", "04-health.jsonl")
for gate in ("session_turn_committed", "process_ran_to_terminal", "trigger_fired"):
    if health.get(gate) is not True:
        fail(f"post-bump gate {gate} did not pass: {health}")
if health["session_ids_reused"] != seeded["session_ids"]:
    fail(f"health check did not reuse the pre-bump session ids: {health}")
if health["trigger_reservations"] != 1 or health["trigger_process_status"] != "Completed":
    fail(f"the post-bump trigger did not deliver to a finished process: {health}")

print(
    "version-bump recreation gates: seed, divergence/older/newer refusals by kind, "
    "readability-probe agreement in both directions, recreation, and health all asserted"
)
PY

if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in version-bump recreation E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in version-bump recreation E2E output)" | tee -a "$test_output"
echo "version-bump recreation e2e passed: phases=4 refusal_cases=3 artifacts=$artifact_dir" | tee -a "$test_output"
