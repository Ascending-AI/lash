#!/usr/bin/env python3
"""Attribute the pool's per-action usage to Buck2 actions and target kinds.

The worker's usage log (`ACTION_USAGE_LOG`, see `action_sizes_from_log.py`)
records what an action used, its Cargo identity and its Buck2 category, but
not which target ran it or what it emitted: a library's metadata and rlib
actions and its unit-test binary's link all read `category=rustc` with one
`<package>/<crate>` identity. Buck2's own event log has the other half --
owner, category, identifier, the request, and when the worker ran it -- but no
usage. This joins the two for the invocations of any checkout:

    python3 tools/buck2/action_categories_from_events.py \\
      --events buck-out/kiln/log/<...>_events.pb.zst usage-*.log

`--events` takes Buck2 event logs (`*.pb.zst`, decoded with the checkout's
pinned Buck2) or saved `buck2 log show` output; `buck2 --isolation-dir kiln
log path --all` lists a checkout's logs. An executed (not cached) remote
action matches the one usage record with the same Cargo identity, category
and `memory_kb` request that ended within five seconds of it and ran as long,
to within 0.5 s or 8%. An action with no such record, or with several, is left
out: its worker's log was not given, or the match would be a guess.

The table printed here is evidence. The same join is what
`action_sizes_from_log.py --refresh --events` sizes a library apart from its
unit-test binary with: Buck2 resolves one execution platform per target, so a
target's request covers every category it runs, but a library and the test
binary built from the same crate are two targets.
"""

from __future__ import annotations

import argparse
import collections
import json
import pathlib
import subprocess
import sys

from action_sizes_from_log import as_int, held_bytes, parse_record, percentile

ROOT = pathlib.Path(__file__).resolve().parents[2]
END_TOLERANCE_SECONDS = 5
WALL_TOLERANCE_SECONDS = 0.5
WALL_TOLERANCE_RATIO = 0.08
# The categories whose records carry a first-party Cargo identity.
COMPILE_CATEGORIES = ("rustc", "clippy", "rustdoc", "rustdoc_json")
# The two targets one `<package>/<crate>` identity can name: the library or
# binary itself, and the test binary built from the same crate root.
TARGET_KINDS = {
    "lib": "target",
    "bin": "target",
    "example": "target",
    "bench": "target",
    "unit-test": "test",
    "bin-unit-test": "test",
    # An integration test's crate names that one target; it is listed so an
    # optimized-profile row can name its kind.
    "test": "test",
}
# The configurations that compile with the optimized flags: `--config=optimized`
# and the host configuration of proc macros and build-script dependencies.
OPTIMIZED_CONFIGURATIONS = ("root//tools/buck2:optimized", "lash-rust-host")


def event_lines(path: pathlib.Path):
    """The JSON lines of one event log, decoding a `.pb.zst` with Buck2."""
    if path.name.endswith(".pb.zst"):
        buck2 = ROOT / ".buck2/bin/buck2"
        if not buck2.is_file():
            raise SystemExit(f"{buck2} is missing; run any kiln command to bootstrap it")
        shown = subprocess.run(
            [str(buck2), "--isolation-dir", "kiln-log-show", "log", "show", str(path.resolve())],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        if shown.returncode:
            # The log of an invocation still running ends mid-frame; every
            # complete event before that is still evidence.
            print(f"{path}: incomplete event log, kept what decoded", file=sys.stderr)
        return shown.stdout.splitlines()
    return path.read_text(encoding="utf-8").splitlines()


def executed_actions(lines):
    """Every remote, uncached command of the event log's actions."""
    for line in lines:
        if '"ActionExecution"' not in line or '"SpanEnd"' not in line:
            continue
        action = json.loads(line)["Event"]["data"]["SpanEnd"]["data"]["ActionExecution"]
        for command in action.get("commands") or []:
            details = command["details"]
            remote = (details.get("command_kind") or {}).get("command", {}).get("RemoteCommand")
            if not remote or remote.get("cache_hit"):
                continue
            properties = {
                item["name"]: item["value"]
                for item in remote["details"]["platform"]["properties"]
            }
            metadata = details["metadata"]
            seconds, nanoseconds = metadata["start_time"]
            wall = metadata["execution_time_us"] / 1e6
            category = action["name"]["category"]
            # `rustc` identifiers read `<emit> [pic] ...`; the emit is the kind.
            emit = action["name"].get("identifier", "").split(" ")[0] if category == "rustc" else ""
            owner = action["key"]["owner"].get("TargetLabel", {})
            label = owner.get("label", {})
            package = label.get("package", "")
            configuration = (owner.get("configuration") or {}).get("full_name", "")
            yield {
                "category": category,
                "optimized": configuration.partition("#")[0] in OPTIMIZED_CONFIGURATIONS,
                "emit": emit,
                "third_party": "third-party" in package,
                # `root//crates/lash:lash__unit_test__rust_test` is the binary
                # of the generated `//crates/lash:lash__unit_test`.
                "label": "//{}:{}".format(
                    package.partition("//")[2], label.get("name", "").removesuffix("__rust_test")
                ),
                "end": seconds + nanoseconds / 1e9 + wall,
                "wall": wall,
                "memory_kb": as_int(properties.get("memory_kb")) or 0,
            }


def target_identities(inventory: dict) -> dict[str, tuple[str, str]]:
    """Each generated label's `<package>/<crate>` key and inventory kind.

    A `__fv_` feature variant is its base target under another resolution: it
    compiles the same crate and takes the same row.
    """
    identities = {}
    libraries = {}
    for package in inventory["packages"]:
        for target in package["targets"]:
            if not target.get("label") or not target.get("cargo"):
                continue
            key = f"{package['package']}/{target['cargo'].replace('-', '_')}"
            identities[target["label"]] = (key, target["kind"])
            if target["kind"] == "lib":
                libraries[package["package"]] = key
    for unit in inventory.get("feature_lane_units", []):
        base = identities.get(unit["label"].split("__fv_", 1)[0])
        if base is not None:
            identities[unit["label"]] = base
        elif unit["kind"] in ("lib", "unit-test") and unit["package"] in libraries:
            identities[unit["label"]] = (libraries[unit["package"]], unit["kind"])
    return identities


def shared_identities(inventory: dict) -> set[str]:
    """The keys a library or binary shares with its unit-test binary."""
    kinds = collections.defaultdict(set)
    for key, kind in target_identities(inventory).values():
        if kind in TARGET_KINDS and kind != "test":
            kinds[key].add(TARGET_KINDS[kind])
    return {key for key, both in kinds.items() if both == {"target", "test"}}


def usage_by_identity(lines):
    """The compile records, as `(key, category) -> [(stamp, record)]`."""
    records = collections.defaultdict(list)
    for line in lines:
        record = parse_record(line)
        stamp = line.partition("\t")[0]
        if record is None or not stamp.isdigit():
            continue
        if record.get("category") not in COMPILE_CATEGORIES:
            continue
        if None in [as_int(record.get(name)) for name in ("wall_ms", "cpu_usec", "requested_kb")]:
            continue
        if held_bytes(record) is None:
            continue
        key = f"{record.get('pkg', '-')}/{record.get('crate', '-')}"
        records[(key, record["category"])].append((int(stamp), record))
    return records


def labelled_records(actions, lines, inventory: dict):
    """Each matched first-party compile action with its usage record.

    Yields `(key, inventory kind, category, emit, optimized, record)`, where
    `optimized` says the action ran in an optimized configuration. A usage
    record is matched at most once.
    """
    identities = target_identities(inventory)
    records = usage_by_identity(lines)
    taken = set()
    for action in actions:
        identity = identities.get(action["label"])
        if identity is None or action["category"] not in COMPILE_CATEGORIES:
            continue
        key, kind = identity
        tolerance = max(WALL_TOLERANCE_SECONDS, WALL_TOLERANCE_RATIO * action["wall"])
        candidates = [
            (stamp, record)
            for stamp, record in records.get((key, action["category"]), [])
            if abs(stamp - action["end"]) <= END_TOLERANCE_SECONDS
            and int(record["requested_kb"]) == action["memory_kb"]
            and abs(int(record["wall_ms"]) / 1000 - action["wall"]) <= tolerance
        ]
        if len(candidates) != 1:
            continue
        mark = (key, action["category"], id(candidates[0][1]))
        if mark in taken:
            continue
        taken.add(mark)
        yield (
            key,
            kind,
            action["category"],
            action["emit"],
            action["optimized"],
            candidates[0][1],
        )


def render(matched) -> str:
    out = [
        "| category | emit | target | actions | wall s | peak MiB p50 | p99 | max | p95 cores | memory x |",
        "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for group, rows in sorted(matched.items(), key=lambda item: -sum(row[1] for row in item[1])):
        peaks = [row[0] for row in rows]
        wall = sum(row[1] for row in rows)
        reserved = sum(row[3] * 1024 * row[1] for row in rows)
        used = sum(row[0] * row[1] for row in rows)
        cores = [row[2] / row[1] for row in rows if row[1] >= 1] or [0.0]
        out.append(
            f"| {group[0]} | {group[1] or '-'} | {group[2]} | {len(rows)} | {wall:.0f} "
            f"| {percentile(peaks, 50) >> 20} | {percentile(peaks, 99) >> 20} | {max(peaks) >> 20} "
            f"| {percentile(cores, 95):.2f} "
            f"| {reserved / used if used else 0:.1f} |"
        )
    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("logs", nargs="+", type=pathlib.Path, help="pool usage logs")
    parser.add_argument(
        "--events",
        action="append",
        nargs="+",
        required=True,
        type=pathlib.Path,
        help="Buck2 event logs (*.pb.zst) or saved `buck2 log show` output",
    )
    args = parser.parse_args()
    actions = [
        action
        for group in args.events
        for path in group
        for action in executed_actions(event_lines(path))
    ]
    inventory = json.loads(
        (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
    )
    lines = [
        line
        for path in args.logs
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
    ]
    matched = collections.defaultdict(list)
    for _key, kind, category, emit, optimized, record in labelled_records(
        actions, lines, inventory
    ):
        matched[(category, emit, kind + (", optimized" if optimized else ""))].append(
            (
                held_bytes(record),
                int(record["wall_ms"]) / 1000,
                int(record["cpu_usec"]) / 1e6,
                int(record["requested_kb"]),
            )
        )
    sys.stdout.write(render(matched))
    compiles = sum(
        1 for action in actions
        if action["category"] in COMPILE_CATEGORIES and not action["third_party"]
    )
    print(
        f"{sum(len(rows) for rows in matched.values())} of {compiles} executed "
        "first-party compile actions matched",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
